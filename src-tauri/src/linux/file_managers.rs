//! "Upload with Aktar" and "Watch with Aktar" in Linux file managers, the
//! counterpart of File Explorer's right-click menu:
//!
//! - Files (GNOME, `~/.local/share/nautilus/scripts`) runs any executable
//!   in its scripts folder from the Scripts submenu, with the selection as
//!   arguments.
//! - Dolphin (KDE) reads service menus from
//!   `~/.local/share/kio/servicemenus`.
//! - Nemo (Cinnamon) reads actions from `~/.local/share/nemo/actions`.
//!
//! Each entry runs `<aktar> --upload <paths>` or `<aktar> --watch <folder>`;
//! a running Aktar gets them from the single-instance plugin. They're
//! written at launch and on a language change, pointing at this copy, and
//! only for file managers that are installed.

use std::path::{Path, PathBuf};

use crate::shell::{UPLOAD_FLAG, WATCH_FLAG};
use crate::t;

/// The second line of the Nautilus scripts, which show by file name (so in
/// the current language): it finds the ones an older language left behind.
const NAUTILUS_MARKER: &str = "# Written by Aktar";

pub fn register() {
    let (Some(data), Some(command)) = (crate::linux::data_home(), crate::linux::launch_command()) else { return };
    let command = command.to_string_lossy().into_owned();
    let (upload, watch) = (t!("Upload with Aktar"), t!("Watch with Aktar"));
    if installed("nautilus") {
        write_nautilus_scripts(&data.join("nautilus/scripts"), &command, &upload, &watch);
    }
    if installed("dolphin") {
        // Service menus must be executable to be trusted (KDE 5.85+).
        let menus = data.join("kio/servicemenus");
        write(&menus.join("aktar-upload.desktop"), &dolphin_menu(&upload, &command, UPLOAD_FLAG, "all/all;", "%F"), true);
        write(&menus.join("aktar-watch.desktop"), &dolphin_menu(&watch, &command, WATCH_FLAG, "inode/directory;", "%f"), true);
    }
    if installed("nemo") {
        let actions = data.join("nemo/actions");
        // Nemo's own keywords: "any" is files and folders, "dir" folders.
        write(&actions.join("aktar-upload.nemo_action"), &nemo_action(&upload, &command, UPLOAD_FLAG, "any", "notnone"), false);
        write(&actions.join("aktar-watch.nemo_action"), &nemo_action(&watch, &command, WATCH_FLAG, "dir", "s"), false);
    }
}

fn installed(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

fn write(path: &Path, contents: &str, executable: bool) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::read_to_string(path).is_ok_and(|current| current == contents) {
        return;
    }
    if let Err(error) = std::fs::write(path, contents) {
        log::warn!("Could not add {}: {error}", path.display());
        return;
    }
    if executable {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
    }
}

/// Quotes for a POSIX shell.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// Quotes an argument in a desktop entry's Exec line (the Desktop Entry
/// Specification, "The Exec key").
fn exec_quote(text: &str) -> String {
    let escaped: String = text
        .chars()
        .flat_map(|c| match c {
            '"' | '`' | '$' | '\\' => vec!['\\', c],
            '%' => vec!['%', '%'],
            _ => vec![c],
        })
        .collect();
    format!("\"{escaped}\"")
}

/// Two scripts: one uploads whatever's selected, one watches a selected
/// folder. Files lists scripts by file name, so they're named in the
/// current language, and an older one (another language) is removed.
fn write_nautilus_scripts(folder: &Path, command: &str, upload: &str, watch: &str) {
    let upload_script = format!("#!/bin/sh\n{NAUTILUS_MARKER}\nexec {} {UPLOAD_FLAG} \"$@\"\n", shell_quote(command));
    let watch_script = format!(
        "#!/bin/sh\n{NAUTILUS_MARKER}\n[ -d \"$1\" ] && exec {} {WATCH_FLAG} \"$1\"\n",
        shell_quote(command)
    );
    let wanted: [(PathBuf, &str); 2] = [(folder.join(file_name(upload)), &upload_script), (folder.join(file_name(watch)), &watch_script)];
    if let Ok(entries) = std::fs::read_dir(folder) {
        for entry in entries.flatten() {
            let path = entry.path();
            let ours = std::fs::read_to_string(&path).is_ok_and(|contents| contents.lines().nth(1) == Some(NAUTILUS_MARKER));
            if ours && !wanted.iter().any(|(wanted, _)| *wanted == path) {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    for (path, script) in wanted {
        write(&path, script, true);
    }
}

/// A title as a file name: no slashes.
fn file_name(title: &str) -> String {
    title.replace('/', "-")
}

/// A Dolphin service menu. One file's MIME types apply to all of its
/// actions, so the folder-only "Watch" has a file of its own.
fn dolphin_menu(name: &str, command: &str, flag: &str, mime_types: &str, files: &str) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Service\n\
         MimeType={mime_types}\n\
         Actions=aktar;\n\
         X-KDE-Priority=TopLevel\n\
         \n\
         [Desktop Action aktar]\n\
         Name={name}\n\
         Icon=aktar\n\
         Exec={} {flag} {files}\n",
        exec_quote(command)
    )
}

fn nemo_action(name: &str, command: &str, flag: &str, extensions: &str, selection: &str) -> String {
    format!(
        "[Nemo Action]\n\
         Name={name}\n\
         Icon-Name=aktar\n\
         Exec={} {flag} %F\n\
         Selection={selection}\n\
         Extensions={extensions};\n",
        exec_quote(command)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_for_the_shell_and_desktop_entries() {
        assert_eq!(shell_quote("/opt/My App/aktar"), "'/opt/My App/aktar'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(exec_quote("/home/me/Aktar 100%.AppImage"), "\"/home/me/Aktar 100%%.AppImage\"");
        assert_eq!(exec_quote("/a/$b"), "\"/a/\\$b\"");
    }
}
