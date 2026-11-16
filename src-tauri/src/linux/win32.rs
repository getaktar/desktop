//! The Linux half of `win32`: the same calls, answered by the desktop.
//!
//! - The clipboard is kept as text or an image (what arboard can read);
//!   a password manager's private marker isn't visible to it.
//! - There's no clipboard change counter, so a hash of what's on it
//!   stands in: it's only ever compared with itself, to tell whether
//!   something else was copied since.
//! - Ctrl+V and the window in front come from xdotool on X11. Wayland
//!   doesn't let an app see other windows: the paste goes to whatever is
//!   in front once the link is ready, through wtype or ydotool.

use super::*;

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::time::Duration;

use crate::linux;

const TEXT: u32 = 1;
/// Width and height (u32, little-endian), then RGBA pixels.
const IMAGE: u32 = 2;

/// The window id `foreground_window` gives on Wayland, where the window in
/// front can't be known.
const UNKNOWN_WINDOW: isize = -1;

fn hash(bytes: &[u8]) -> u32 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    let value = hasher.finish();
    (value ^ (value >> 32)) as u32
}

/// Stands in for Windows' clipboard sequence number: changes whenever
/// what's on the clipboard does.
pub fn clipboard_sequence() -> u32 {
    crate::clipboard::with(|clipboard| {
        if let Ok(text) = clipboard.get_text() {
            return hash(text.as_bytes()) | 1;
        }
        if let Ok(image) = clipboard.get_image() {
            return hash(&image.bytes) ^ (image.width as u32).rotate_left(16) ^ image.height as u32;
        }
        0
    })
    .unwrap_or(0)
}

pub fn take_snapshot() -> Snapshot {
    let kept = crate::clipboard::with(|clipboard| {
        if let Ok(text) = clipboard.get_text() {
            if text.len() > MAX_SNAPSHOT_BYTES {
                return Err(Snapshot::TooBig);
            }
            return Ok(vec![(TEXT, text.into_bytes())]);
        }
        if let Ok(image) = clipboard.get_image() {
            if image.bytes.len() > MAX_SNAPSHOT_BYTES {
                return Err(Snapshot::TooBig);
            }
            let mut bytes = Vec::with_capacity(8 + image.bytes.len());
            bytes.extend_from_slice(&(image.width as u32).to_le_bytes());
            bytes.extend_from_slice(&(image.height as u32).to_le_bytes());
            bytes.extend_from_slice(&image.bytes);
            return Ok(vec![(IMAGE, bytes)]);
        }
        // Empty, or nothing arboard can read (then putting it back means
        // emptying it, as Windows does with formats it can't copy).
        Ok(Vec::new())
    });
    match kept {
        Ok(Ok(formats)) => Snapshot::Kept(ClipboardSnapshot { formats }),
        Ok(Err(snapshot)) => snapshot,
        Err(_) => Snapshot::Unavailable,
    }
}

pub fn restore(snapshot: &ClipboardSnapshot) -> bool {
    let restored = crate::clipboard::with(|clipboard| match snapshot.formats.first() {
        None => clipboard.clear().is_ok(),
        Some((TEXT, bytes)) => clipboard.set_text(String::from_utf8_lossy(bytes).into_owned()).is_ok(),
        Some((IMAGE, bytes)) if bytes.len() >= 8 => {
            let width = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
            let height = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
            let image = arboard::ImageData { width, height, bytes: bytes[8..].to_vec().into() };
            clipboard.set_image(image).is_ok()
        }
        Some(_) => false,
    });
    restored.unwrap_or(false)
}

pub fn clear_clipboard() -> bool {
    crate::clipboard::with(|clipboard| clipboard.clear().is_ok()).unwrap_or(false)
}

pub fn clipboard_owner_process() -> Option<String> {
    None
}

pub fn clipboard_url() -> Option<String> {
    None
}

/// Not knowable on Wayland, and xdotool's `--clearmodifiers` lets go of
/// the shortcut's keys on X11, so the paste never waits for them.
pub fn modifiers_down() -> bool {
    false
}

pub fn mask_menu_key() {}

/// Ctrl+V in the app in front. The tools press the key that types "v" in
/// the current layout, like the Windows version does.
pub fn send_paste() -> bool {
    // The shortcut's own keys are often still down; give them a moment.
    std::thread::sleep(Duration::from_millis(120));
    if linux::is_wayland() {
        // wtype: wlroots desktops (Sway, Hyprland) and KDE. ydotool: any
        // desktop, with its daemon running. Linux key codes 29 (Ctrl) and
        // 47 (V).
        linux::run("wtype", &["-M", "ctrl", "-k", "v", "-m", "ctrl"])
            || linux::run("ydotool", &["key", "29:1", "47:1", "47:0", "29:0"])
            // An app running under XWayland.
            || linux::run("xdotool", &["key", "--clearmodifiers", "ctrl+v"])
    } else {
        linux::run("xdotool", &["key", "--clearmodifiers", "ctrl+v"])
    }
}

/// Capture uses the screenshot portal on Linux (see `capture`).
pub fn send_screen_snip() -> bool {
    false
}

fn active_window() -> Option<isize> {
    linux::output("xdotool", &["getactivewindow"])?.parse().ok()
}

/// The window in front, unless it's one of Aktar's own. On Wayland, where
/// it can't be known, a placeholder that `is_foreground` always accepts.
pub fn foreground_window() -> Option<isize> {
    if linux::is_wayland() {
        return Some(UNKNOWN_WINDOW);
    }
    let window = active_window()?;
    let pid: u32 = linux::output("xdotool", &["getwindowpid", &window.to_string()]).and_then(|pid| pid.parse().ok()).unwrap_or(0);
    (pid != std::process::id()).then_some(window)
}

pub fn is_foreground(window: isize) -> bool {
    window == UNKNOWN_WINDOW || foreground_handle() == window
}

pub fn foreground_handle() -> isize {
    if linux::is_wayland() {
        return UNKNOWN_WINDOW;
    }
    active_window().unwrap_or(0)
}

/// The desktop's alert sound, where libcanberra's player is installed.
pub fn beep() {
    std::thread::spawn(|| {
        let _ = linux::run("canberra-gtk-play", &["--id", "bell"]);
    });
}

pub fn free_disk_space(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is NUL-terminated and `stats` is written by statvfs
    // before it's read, only when the call succeeds.
    let found = unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } == 0;
    found.then(|| {
        let stats = unsafe { stats.assume_init() };
        // fsblkcnt_t and c_ulong are 32-bit on 32-bit targets.
        #[allow(clippy::unnecessary_cast)]
        (stats.f_bavail as u64).saturating_mul(stats.f_frsize as u64)
    })
}

/// Linux has no one network API every desktop has; an upload that fails
/// for want of a connection waits and retries anyway.
pub fn is_online() -> bool {
    true
}

pub fn watch_network(_changed: impl Fn() + Send + Sync + 'static) {}

/// Notifications shown with `show_toast`, by tag, so a newer one replaces
/// the last and `remove_toast` can take it away.
static SHOWN: Mutex<Option<HashMap<String, u32>>> = Mutex::new(None);

fn shown_id(tag: &str) -> Option<u32> {
    SHOWN.lock().unwrap().as_ref().and_then(|shown| shown.get(tag).copied())
}

pub fn show_toast(
    app_id: &str,
    tag: &str,
    title: &str,
    lines: &[&str],
    button: Option<ToastButton<'_>>,
    on_button: Option<Box<dyn Fn(String) + Send + Sync + 'static>>,
) -> Result<(), String> {
    let _ = app_id;
    let body: Vec<&str> = lines.iter().copied().filter(|line| !line.is_empty()).take(2).collect();
    let mut notification = notify_rust::Notification::new();
    notification
        .appname("Aktar")
        .icon(ICON_NAME)
        .summary(title)
        .body(&body.join("\n"))
        .hint(notify_rust::Hint::DesktopEntry(DESKTOP_ENTRY.into()));
    if let Some(id) = shown_id(tag) {
        notification.id(id);
    }
    if let Some(button) = &button {
        notification.action(button.argument, button.label);
    }
    let handle = off_runtime(move || notification.show()).map_err(|error| error.to_string())?;
    SHOWN.lock().unwrap().get_or_insert_with(HashMap::new).insert(tag.to_string(), handle.id());
    if let (Some(on_button), Some(_)) = (on_button, button) {
        // Waits on a thread of its own until the notification is clicked or
        // closed (replacing it closes it too).
        std::thread::spawn(move || {
            handle.wait_for_action(|action| {
                if action != "__closed" && action != "default" {
                    on_button(action.to_string());
                }
            });
        });
    }
    Ok(())
}

pub fn remove_toast(app_id: &str, tag: &str) {
    let _ = app_id;
    let Some(id) = SHOWN.lock().unwrap().as_mut().and_then(|shown| shown.remove(tag)) else { return };
    // Replacing a notification by id gives a handle that can close it.
    let closed = off_runtime(move || notify_rust::Notification::new().id(id).appname("Aktar").summary("Aktar").show().map(|handle| handle.close()));
    if let Err(error) = closed {
        log::debug!("Could not remove a notification: {error}");
    }
}

/// notify-rust talks to D-Bus with blocking calls, which panic on a thread
/// of an async runtime (where these are often called from): they run on a
/// thread of their own.
fn off_runtime<T: Send + 'static>(call: impl FnOnce() -> Result<T, notify_rust::error::Error> + Send + 'static) -> Result<T, String> {
    std::thread::spawn(move || call().map_err(|error| error.to_string()))
        .join()
        .map_err(|_| "The notification thread panicked".to_string())?
}

/// The icon and desktop file the .deb and AppImage install (the product
/// name, lower-cased, as Tauri's bundler names them).
const ICON_NAME: &str = "aktar";
const DESKTOP_ENTRY: &str = "Aktar";
