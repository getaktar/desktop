//! What Aktar needs from a Linux desktop that Windows gives it through its
//! own APIs: the screenshot dialog, the light/dark setting, key presses for
//! upload and paste, and file manager menus. The `win32` and `system`
//! modules take their Linux halves from `win32.rs` and `system.rs` here.
//!
//! Desktop services are reached through xdg-desktop-portal (D-Bus), which
//! GNOME, KDE and the wlroots desktops all provide, on X11 and Wayland
//! alike. Key presses have no portal a tray app can use without asking, so
//! they go through the tool the session allows: xdotool on X11, wtype or
//! ydotool on Wayland.

pub mod file_managers;

use std::future::Future;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Whether the session is Wayland (as opposed to X11).
pub fn is_wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var("XDG_SESSION_TYPE").is_ok_and(|kind| kind.eq_ignore_ascii_case("wayland"))
}

/// The desktop's names, upper-cased ("GNOME", "KDE", "UBUNTU:GNOME").
pub fn current_desktop() -> String {
    std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_ascii_uppercase()
}

/// Runs a program quietly and reports whether it ran and succeeded. A
/// program that isn't installed counts as a failure.
pub fn run(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// A program's trimmed output, when it ran and succeeded.
pub fn output(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The runtime every portal call runs on, for the life of the app. ashpd
/// keeps one D-Bus connection for the whole process, and that connection's
/// tasks live on the runtime that opened it: a runtime made for one call
/// and dropped after it would leave every later call waiting forever.
fn portal_runtime() -> Option<&'static tokio::runtime::Runtime> {
    static RUNTIME: std::sync::OnceLock<Option<tokio::runtime::Runtime>> = std::sync::OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_name("portal")
                .enable_all()
                .build()
                .inspect_err(|error| log::warn!("Could not start the portal runtime: {error}"))
                .ok()
        })
        .as_ref()
}

/// How long a portal call that should answer at once may take before it's
/// given up on (a desktop without the portal, or a stuck one).
const QUICK_CALL: Duration = Duration::from_secs(3);

/// Runs a quick portal call (a setting) and waits for it, from anywhere:
/// the UI thread, a blocking task, or an async task. None when it failed to
/// answer in time.
pub fn block_on<T: Send + 'static>(future: impl Future<Output = T> + Send + 'static) -> Option<T> {
    let (sender, receiver) = std::sync::mpsc::channel();
    portal_runtime()?.spawn(async move {
        let _ = sender.send(future.await);
    });
    receiver.recv_timeout(QUICK_CALL).ok()
}

/// Runs a portal call that waits for the user (the screenshot dialog) on
/// the portal runtime, for an async caller on another runtime.
pub async fn on_portal_runtime<T: Send + 'static>(future: impl Future<Output = T> + Send + 'static) -> Option<T> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    portal_runtime()?.spawn(async move {
        let _ = sender.send(future.await);
    });
    receiver.await.ok()
}

/// Runs a portal call for as long as the app runs (a change signal).
pub fn spawn_forever(future: impl Future<Output = ()> + Send + 'static) {
    if let Some(runtime) = portal_runtime() {
        runtime.spawn(future);
    }
}

/// How a screenshot request ended.
pub enum Screenshot {
    Taken(PathBuf),
    /// Closed without a screenshot (Esc, or Cancel).
    Cancelled,
    /// No screenshot portal, or it failed.
    Failed(String),
}

/// Opens the desktop's own screenshot dialog (GNOME's and KDE's, which let
/// the user pick an area, a window or the whole screen) and waits for it.
pub async fn take_screenshot() -> Screenshot {
    on_portal_runtime(request_screenshot()).await.unwrap_or_else(|| Screenshot::Failed("The portal runtime isn't running".into()))
}

async fn request_screenshot() -> Screenshot {
    use ashpd::desktop::screenshot::Screenshot as Request;
    use ashpd::desktop::ResponseError;

    let response = match Request::request().interactive(true).modal(true).send().await {
        Ok(request) => request.response(),
        Err(error) => Err(error),
    };
    match response {
        Ok(screenshot) => match url::Url::parse(screenshot.uri().as_str()).ok().and_then(|uri| uri.to_file_path().ok()) {
            Some(path) => Screenshot::Taken(path),
            None => Screenshot::Failed(format!("Unexpected screenshot location {}", screenshot.uri())),
        },
        Err(ashpd::Error::Response(ResponseError::Cancelled)) => Screenshot::Cancelled,
        Err(error) => Screenshot::Failed(error.to_string()),
    }
}

/// The command that starts this copy of Aktar: the AppImage itself when it
/// runs from one (its own path is a mount that's gone after it quits),
/// else the executable.
pub fn launch_command() -> Option<PathBuf> {
    if let Some(appimage) = std::env::var_os("APPIMAGE") {
        return Some(PathBuf::from(appimage));
    }
    std::env::current_exe().ok()
}

/// Starts the AppImage again, for after an update replaced it, and
/// returns whether it did (false when this isn't an AppImage).
///
/// The AppImage's launcher points variables like LD_LIBRARY_PATH,
/// XDG_DATA_DIRS and GIO_MODULE_DIR into its own mount. Handed on as they
/// are (as a plain restart does), the new version would load the old
/// version's bundled libraries and keep the old mount alive: everything
/// inside `$APPDIR` is taken out of them first. A shell waits a moment
/// before starting it, so this copy has quit by then and the new one
/// doesn't hand itself over to it as a second instance.
pub fn relaunch_appimage() -> bool {
    let Some(appimage) = std::env::var_os("APPIMAGE") else { return false };
    let appdir = std::env::var("APPDIR").ok().filter(|dir| !dir.is_empty());
    let mut command = Command::new("sh");
    command.args(["-c", "sleep 1; exec \"$0\""]).arg(&appimage).env_clear();
    for (name, value) in std::env::vars_os() {
        if ["APPDIR", "APPIMAGE", "ARGV0", "OWD"].iter().any(|own| name == *own) {
            continue;
        }
        match (&appdir, value.to_str()) {
            (Some(appdir), Some(value)) if value.contains(appdir.as_str()) => {
                let kept: Vec<&str> = value.split(':').filter(|part| !part.is_empty() && !part.starts_with(appdir.as_str())).collect();
                if !kept.is_empty() {
                    command.env(&name, kept.join(":"));
                }
            }
            _ => {
                command.env(&name, &value);
            }
        }
    }
    let directory = std::env::var_os("OWD").or_else(|| std::env::var_os("HOME")).unwrap_or_else(|| "/".into());
    command.current_dir(directory).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // The AppImage runtime leaves a descriptor of its mount open in this
    // process (not close-on-exec); inherited, it would keep the old
    // version's mount, and its runtime, alive for good.
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            if libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32) != 0 {
                for descriptor in 3..4096 {
                    libc::close(descriptor);
                }
            }
            Ok(())
        });
    }
    match command.spawn() {
        Ok(_) => true,
        Err(error) => {
            log::warn!("Could not start the updated AppImage: {error}");
            false
        }
    }
}

/// `$XDG_DATA_HOME`, or `~/.local/share`.
pub fn data_home() -> Option<PathBuf> {
    match std::env::var_os("XDG_DATA_HOME") {
        Some(path) if !path.is_empty() => Some(PathBuf::from(path)),
        _ => std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")),
    }
}
