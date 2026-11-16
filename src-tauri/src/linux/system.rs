//! The Linux half of `system`, from the desktop's portal settings and the
//! locale environment.

use crate::linux;

/// Whether the panel the tray icon sits on is light. GNOME's top bar is
/// dark in light mode too; KDE's panel and most others follow the color
/// scheme.
pub fn taskbar_uses_light_theme() -> bool {
    if linux::current_desktop().contains("GNOME") {
        return false;
    }
    apps_use_light_theme()
}

/// The desktop's light/dark preference (Settings > Appearance), from
/// xdg-desktop-portal. "No preference" is light, as GTK apps take it.
pub fn apps_use_light_theme() -> bool {
    use ashpd::desktop::settings::{ColorScheme, Settings};
    let scheme = linux::block_on(async {
        let settings = Settings::new().await.ok()?;
        settings.color_scheme().await.ok()
    })
    .flatten();
    !matches!(scheme, Some(ColorScheme::PreferDark))
}

/// Calls `changed` whenever the light/dark preference changes, on a thread
/// of its own.
pub fn watch_theme(changed: impl Fn() + Send + 'static) {
    use ashpd::desktop::settings::Settings;
    use futures_util::StreamExt;
    linux::spawn_forever(async move {
        let Ok(settings) = Settings::new().await else { return };
        let Ok(mut changes) = settings.receive_color_scheme_changed().await else { return };
        while changes.next().await.is_some() {
            changed();
        }
    });
}

/// The regional format from the locale environment, e.g. "en-GB" for
/// `LC_TIME=en_GB.UTF-8`, which dates and numbers should follow.
pub fn region_locale() -> Option<String> {
    ["LC_ALL", "LC_TIME", "LANG"]
        .into_iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|value| !value.is_empty())
        .and_then(|value| locale_tag(&value))
}

/// "en_GB.UTF-8@euro" -> "en-GB"; "C" and "POSIX" have no region.
fn locale_tag(value: &str) -> Option<String> {
    let name = value.split(['.', '@']).next()?;
    if name.is_empty() || name == "C" || name == "POSIX" {
        return None;
    }
    Some(name.replace('_', "-"))
}

/// GTK's default; Linux tray icons don't report clicks anyway.
pub fn double_click_millis() -> u64 {
    400
}

/// For when Aktar can't start at all: a dialog from whichever tool the
/// desktop has, and the terminal otherwise.
pub fn show_fatal_error(message: &str) {
    eprintln!("{message}");
    let _ = linux::run("zenity", &["--error", "--title=Aktar", "--text", message])
        || linux::run("kdialog", &["--title", "Aktar", "--error", message])
        || linux::run("notify-send", &["--urgency=critical", "Aktar", message]);
}

/// Not something an app may ask on Wayland; the rename dialog is there
/// from its own shortcut.
pub fn alt_key_down() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::locale_tag;

    #[test]
    fn locale_tags_from_the_environment() {
        assert_eq!(locale_tag("en_GB.UTF-8").as_deref(), Some("en-GB"));
        assert_eq!(locale_tag("tr_TR.UTF-8").as_deref(), Some("tr-TR"));
        assert_eq!(locale_tag("de_DE@euro").as_deref(), Some("de-DE"));
        assert_eq!(locale_tag("C.UTF-8"), None);
        assert_eq!(locale_tag("POSIX"), None);
    }
}
