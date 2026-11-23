//! Capture (the Mac app's docs/capture-and-instant-links.md, "Capture", and
//! docs/capture-tools.md): by default Windows' own snipping bar (Windows+
//! Shift+S, the Snipping Tool) picks an area, a window or the whole screen
//! and puts the snip on the clipboard, where Aktar takes it and uploads it
//! like a pasted image. Settings can pick another tool instead
//! (`capture_tools`), so the image that goes up is the edited one.
//!
//! Snipping Tool's newer `ms-screenclip://capture` protocol only answers
//! packaged apps (and the old `ms-screenclip:` launch is retired), so the
//! snipping bar is opened with its shortcut and the clipboard watched: the
//! first image put there while the bar is up, or within a moment after it
//! closes, is the snip. A bar closed without a snip (Esc) ends the wait
//! without uploading anything, and so does a minute without one.
//!
//! On Linux the desktop's screenshot dialog (GNOME's, KDE's) is asked for
//! through xdg-desktop-portal, which works on Wayland too, and hands back
//! the file it saved.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::capture_tools::{CaptureTool, FolderWatch, Kind, MAX_TOOL_WAIT};
use crate::core::SharedCore;
use crate::quick_share::{Origin, PasteTarget, PASTE_WINDOW_MS};
use crate::uploads::UploadInput;
use crate::win32;

static RUNNING: AtomicBool = AtomicBool::new(false);
/// Asked for again while one waits: the snipping bar (or the tool) opens
/// again.
static AGAIN: AtomicBool = AtomicBool::new(false);
/// "Stop Waiting" in the tray menu, for a tool Aktar waits for.
static STOP: AtomicBool = AtomicBool::new(false);
/// The tool Aktar is waiting for right now, for the tray menu.
static WAITING_FOR: Mutex<Option<CaptureTool>> = Mutex::new(None);
/// A file a tool sent with `--upload` while Aktar waits for it (ShareX's
/// action), taken by the wait.
static DELIVERED: Mutex<Option<PathBuf>> = Mutex::new(None);

/// How long after the snipping bar closed a snip may still turn up on the
/// clipboard.
#[cfg(not(target_os = "linux"))]
const BAR_GRACE: Duration = Duration::from_secs(3);
/// How long the panel takes to hand the foreground back once it's hidden.
const PANEL_SETTLE: Duration = Duration::from_millis(250);
/// The longest a capture is waited for.
#[cfg(not(target_os = "linux"))]
const MAX_WAIT: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(150);
/// How often a tool's save folder is looked at while Aktar waits.
const FOLDER_POLL: Duration = Duration::from_millis(300);

/// Starts a capture; one that's waiting opens the snipping bar (or the
/// tool) again. With `paste`, the link is pasted into the window that was
/// in front.
pub fn start(core: &SharedCore, paste: bool) {
    if RUNNING.swap(true, Ordering::SeqCst) {
        AGAIN.store(true, Ordering::SeqCst);
        return;
    }
    AGAIN.store(false, Ordering::SeqCst);
    STOP.store(false, Ordering::SeqCst);
    let target = if paste { win32::foreground_window() } else { None };
    crate::panel::hide(&core.app);
    let core = core.clone();
    tauri::async_runtime::spawn(async move {
        let tool = tool_to_use(&core);
        match tool.map(|(tool, path)| (tool, tool.kind(), path)) {
            None => system_capture(&core, target).await,
            Some((tool, Kind::Run, path)) => run_tool(&core, target, tool, &path).await,
            Some((tool, Kind::EditAfter, path)) => edit_after(&core, target, tool, &path).await,
            Some((tool, Kind::Wait | Kind::Send, path)) => wait_for_tool(&core, target, tool, &path).await,
            Some((_, Kind::System, _)) => system_capture(&core, target).await,
        }
        RUNNING.store(false, Ordering::SeqCst);
    });
}

/// Whether Aktar waits for a tool now (the tray menu then has "Stop
/// Waiting"), and for which.
pub fn waiting_for() -> Option<CaptureTool> {
    *WAITING_FOR.lock().unwrap()
}

/// "Stop Waiting": ends the wait for a tool without uploading anything.
pub fn stop_waiting() {
    STOP.store(true, Ordering::SeqCst);
}

/// A tool sent these files with `--upload`. While Aktar waits for that
/// tool (ShareX started by Aktar's Capture), its one image is the capture:
/// it goes up with the capture's paste target and clipboard for Undo.
/// Returns whether it was taken.
pub fn deliver(paths: &[PathBuf]) -> bool {
    let [path] = paths else { return false };
    if waiting_for().is_none_or(|tool| tool.kind() != Kind::Send) || !crate::capture_tools::is_image(path) {
        return false;
    }
    *DELIVERED.lock().unwrap() = Some(path.clone());
    true
}

/// The tool Settings picked and where it is, or None for the system tool:
/// also when the picked one isn't on this platform, can't be started by
/// Aktar (it's used with its own shortcut), or isn't installed, which the
/// user is told.
fn tool_to_use(core: &SharedCore) -> Option<(CaptureTool, PathBuf)> {
    let tool = core.settings.get().capture_tool;
    if tool == CaptureTool::System || !tool.is_on_this_platform() || !tool.can_start() {
        return None;
    }
    match tool.find() {
        Some(path) => Some((tool, path)),
        None => {
            crate::uploads::show_notification(
                core,
                &crate::t!("{0} isn’t installed", tool.name()),
                &crate::t!("Capture used the system’s screenshot tool instead. Install {0}, or pick another tool in Settings > General.", tool.name()),
            );
            None
        }
    }
}

fn paste_target(target: Option<isize>) -> Option<PasteTarget> {
    target.map(|window| PasteTarget { window, deadline: crate::util::now_millis() + PASTE_WINDOW_MS })
}

fn enqueue_capture(core: &SharedCore, input: UploadInput, target: Option<isize>, snapshot: win32::Snapshot) {
    crate::uploads::enqueue_with(core, vec![input], None, Origin { paste: paste_target(target), snapshot: Some(snapshot) });
}

async fn take_snapshot() -> win32::Snapshot {
    // What's on the clipboard now is what Undo puts back: never the
    // screenshot itself (or the password in it).
    tauri::async_runtime::spawn_blocking(win32::take_snapshot).await.unwrap_or(win32::Snapshot::Unavailable)
}

fn tool_failed(core: &SharedCore, tool: CaptureTool, error: &str) {
    log::warn!("{} didn't start: {error}", tool.name());
    crate::uploads::show_notification(core, &crate::t!("{0} didn’t start", tool.name()), error);
}

/// Starts a tool (with a clean environment on Linux, so an AppImage's
/// bundled libraries don't leak into it).
fn command(program: &Path, arguments: &[std::ffi::OsString]) -> tokio::process::Command {
    #[cfg(target_os = "linux")]
    let mut command = crate::linux::host_command(program);
    #[cfg(not(target_os = "linux"))]
    let mut command = std::process::Command::new(program);
    command.args(arguments).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped());
    tokio::process::Command::from(command)
}

/// Runs a tool until it exits and returns whether it ran; what it printed
/// on stderr comes with a failure.
async fn run_to_end(program: &Path, arguments: &[std::ffi::OsString]) -> Result<(), String> {
    let output = command(program, arguments).output().await.map_err(|error| error.to_string())?;
    if !output.status.success() {
        log::debug!("{} exited with {}: {}", program.display(), output.status, String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

/// A file the tool wrote, if it wrote one.
fn written(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0)
}

/// Way A: Flameshot's editor, whose Accept writes the image to the file it
/// was given and ends the process. Closing it without Accept is a cancel.
async fn run_tool(core: &SharedCore, target: Option<isize>, tool: CaptureTool, program: &Path) {
    let snapshot = take_snapshot().await;
    tokio::time::sleep(PANEL_SETTLE).await;
    let Some((output, filename)) = crate::clipboard::new_capture_file("png") else { return };
    loop {
        AGAIN.store(false, Ordering::SeqCst);
        if let Err(error) = run_to_end(program, &tool.arguments(None, &output)).await {
            tool_failed(core, tool, &error);
            return;
        }
        if written(&output) {
            enqueue_capture(core, crate::clipboard::temporary_input(output, filename), target, snapshot);
            return;
        }
        // Asked for again while it was up: it opens once more.
        if !AGAIN.load(Ordering::SeqCst) {
            crate::clipboard::remove_capture_file(&output);
            return;
        }
    }
}

/// Satty and Swappy: the desktop's screenshot dialog first, as by default,
/// then the editor on that screenshot. What the editor writes goes up;
/// closing Satty without saving is a cancel. (Swappy writes on close.)
async fn edit_after(core: &SharedCore, target: Option<isize>, tool: CaptureTool, program: &Path) {
    let snapshot = take_snapshot().await;
    tokio::time::sleep(PANEL_SETTLE).await;
    let Some(screenshot) = system_screenshot_file(core).await else { return };
    let Some(input) = crate::clipboard::copy_to_temporary(&screenshot) else { return };
    let Some((output, filename)) = crate::clipboard::new_capture_file("png") else { return };
    let result = run_to_end(program, &tool.arguments(Some(&input.path), &output)).await;
    crate::clipboard::remove_capture_file(&input.path);
    if let Err(error) = result {
        tool_failed(core, tool, &error);
        return;
    }
    if written(&output) {
        enqueue_capture(core, crate::clipboard::temporary_input(output, filename), target, snapshot);
    } else {
        crate::clipboard::remove_capture_file(&output);
    }
}

/// Way B (Spectacle) and ShareX: Aktar starts the tool and waits, up to 10
/// minutes, for the edited image: on the clipboard or in the tool's save
/// folder (Spectacle), or sent back with `--upload` (ShareX's action). A
/// new capture starts the tool again; "Stop Waiting" ends the wait.
async fn wait_for_tool(core: &SharedCore, target: Option<isize>, tool: CaptureTool, program: &Path) {
    let snapshot = take_snapshot().await;
    tokio::time::sleep(PANEL_SETTLE).await;
    *DELIVERED.lock().unwrap() = None;
    let folder = (tool.kind() == Kind::Wait).then(|| save_folder(core, tool)).flatten();
    let watches_clipboard = tool.kind() == Kind::Wait;
    let arguments = tool.arguments(None, Path::new(""));
    if let Err(error) = command(program, &arguments).spawn() {
        tool_failed(core, tool, &error.to_string());
        return;
    }
    set_waiting(core, Some(tool));
    let mut sequence = win32::clipboard_sequence();
    let mut watch = folder.map(FolderWatch::start);
    let mut started = Instant::now();
    let mut last_folder_look = Instant::now();
    let result = loop {
        tokio::time::sleep(POLL).await;
        if STOP.swap(false, Ordering::SeqCst) || started.elapsed() > MAX_TOOL_WAIT {
            break None;
        }
        if AGAIN.swap(false, Ordering::SeqCst) {
            if let Err(error) = command(program, &arguments).spawn() {
                tool_failed(core, tool, &error.to_string());
                break None;
            }
            started = Instant::now();
            continue;
        }
        if let Some(path) = DELIVERED.lock().unwrap().take() {
            break Some(UploadInput::from_path(path));
        }
        if watches_clipboard {
            let current = win32::clipboard_sequence();
            if current != sequence {
                sequence = current;
                if let Some(input) = tauri::async_runtime::spawn_blocking(crate::clipboard::read_capture).await.ok().flatten() {
                    break Some(input);
                }
            }
        }
        if let Some(watch) = watch.as_mut().filter(|_| last_folder_look.elapsed() >= FOLDER_POLL) {
            last_folder_look = Instant::now();
            if let Some(path) = watch.poll() {
                break Some(UploadInput::from_path(path));
            }
        }
    };
    set_waiting(core, None);
    if let Some(input) = result {
        enqueue_capture(core, input, target, snapshot);
    }
}

fn set_waiting(core: &SharedCore, tool: Option<CaptureTool>) {
    *WAITING_FOR.lock().unwrap() = tool;
    let app = core.app.clone();
    let _ = core.app.run_on_main_thread(move || crate::tray::refresh_menu(&app));
}

/// The folder the tool saves to: the one picked in Settings, else its own
/// default.
fn save_folder(core: &SharedCore, tool: CaptureTool) -> Option<PathBuf> {
    core.settings.get().capture_folder.map(PathBuf::from).filter(|folder| folder.is_dir()).or_else(|| tool.default_save_folder())
}

/// Linux: the desktop's own screenshot dialog (xdg-desktop-portal), which
/// hands back a file instead of putting the snip on the clipboard. None
/// when it was cancelled or failed (the user is told).
#[cfg(target_os = "linux")]
async fn system_screenshot_file(core: &SharedCore) -> Option<PathBuf> {
    use crate::linux::Screenshot;

    loop {
        AGAIN.store(false, Ordering::SeqCst);
        match crate::linux::take_screenshot().await {
            Screenshot::Taken(path) => return Some(path),
            // Asked for again while the dialog was up: it was closed for
            // that, so it opens once more.
            Screenshot::Cancelled if AGAIN.load(Ordering::SeqCst) => continue,
            Screenshot::Cancelled => return None,
            Screenshot::Failed(error) => {
                log::warn!("Screenshot portal: {error}");
                crate::uploads::show_notification(
                    core,
                    &crate::t!("Capture couldn’t start"),
                    &crate::t!("The desktop’s screenshot tool didn’t open. Take the screenshot with Print Screen, copy it and upload the clipboard with Aktar."),
                );
                return None;
            }
        }
    }
}

/// Satty and Swappy are Linux tools; Windows never gets here.
#[cfg(not(target_os = "linux"))]
async fn system_screenshot_file(_core: &SharedCore) -> Option<PathBuf> {
    None
}

#[cfg(target_os = "linux")]
async fn system_capture(core: &SharedCore, target: Option<isize>) {
    let snapshot = take_snapshot().await;
    tokio::time::sleep(PANEL_SETTLE).await;
    let Some(path) = system_screenshot_file(core).await else { return };
    let input = tauri::async_runtime::spawn_blocking(move || crate::clipboard::read_screenshot_file(&path)).await.ok().flatten();
    let Some(input) = input else {
        crate::uploads::show_notification(core, &crate::t!("Capture couldn’t start"), &crate::t!("The screenshot couldn’t be read."));
        return;
    };
    enqueue_capture(core, input, target, snapshot);
}

#[cfg(not(target_os = "linux"))]
async fn system_capture(core: &SharedCore, target: Option<isize>) {
    let snapshot = take_snapshot().await;
    let mut sequence = win32::clipboard_sequence();
    let Some(mut before) = open_snipping_bar(core).await else { return };
    let mut started = Instant::now();
    // The snipping bar takes the foreground while it's up, and hands it back
    // to the window that had it once it closes, with a snip or without.
    let mut bar_seen = false;
    let mut back_since: Option<Instant> = None;
    while started.elapsed() < MAX_WAIT {
        tokio::time::sleep(POLL).await;
        if AGAIN.swap(false, Ordering::SeqCst) {
            let Some(front) = open_snipping_bar(core).await else { return };
            (before, started, bar_seen, back_since) = (front, Instant::now(), false, None);
            continue;
        }
        let current = win32::clipboard_sequence();
        if current != sequence {
            sequence = current;
            log::debug!("Clipboard changed during a capture (by {:?})", win32::clipboard_owner_process());
            if let Some(input) = tauri::async_runtime::spawn_blocking(crate::clipboard::read_capture).await.ok().flatten() {
                enqueue_capture(core, input, target, snapshot);
                return;
            }
        }
        let front = win32::foreground_handle();
        if front != before {
            bar_seen = true;
            back_since = None;
        } else if bar_seen {
            // Closed without a snip (Esc, or a click away).
            let since = *back_since.get_or_insert_with(Instant::now);
            if since.elapsed() > BAR_GRACE {
                return;
            }
        }
    }
}

/// Presses Windows+Shift+S once the shortcut's own keys are up (they would
/// make it another shortcut), after the panel had a moment to hand the
/// foreground back to the window behind it. Returns the window in front
/// before the bar opened, or None when the keys couldn't be pressed.
#[cfg(not(target_os = "linux"))]
async fn open_snipping_bar(core: &SharedCore) -> Option<isize> {
    tokio::time::sleep(PANEL_SETTLE).await;
    for _ in 0..30 {
        if !win32::modifiers_down() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let before = win32::foreground_handle();
    if !win32::send_screen_snip() {
        crate::uploads::show_notification(
            core,
            &crate::t!("Capture couldn’t start"),
            &crate::t!("Windows’ snipping tool didn’t open. Try again, or take the screenshot with Windows+Shift+S and paste it into Aktar."),
        );
        return None;
    }
    Some(before)
}
