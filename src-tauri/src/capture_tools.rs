//! Capture tools (the Mac app's docs/capture-tools.md): Capture can run the
//! screenshot tool the user already likes, so what goes up is the edited
//! image (arrows, blur, text), not the bare snip. Aktar has no editor of
//! its own.
//!
//! No tool hands the edited file back to the app that started it, except a
//! few command-line ones, so every tool here fits one of four kinds (see
//! `Kind`), and `capture` is built around those rather than around each
//! tool. This module is the pure part: which tools this platform has, where
//! they are, and the arguments they're started with.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

/// The tool Capture uses. `System` is the default: Windows' snipping bar,
/// or the desktop's screenshot dialog on Linux.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum CaptureTool {
    #[default]
    #[serde(rename = "system")]
    System,
    #[serde(rename = "flameshot")]
    Flameshot,
    #[serde(rename = "sharex")]
    ShareX,
    #[serde(rename = "greenshot")]
    Greenshot,
    #[serde(rename = "snagit")]
    Snagit,
    #[serde(rename = "picpick")]
    PicPick,
    #[serde(rename = "satty")]
    Satty,
    #[serde(rename = "swappy")]
    Swappy,
    #[serde(rename = "spectacle")]
    Spectacle,
}

/// Reads the setting, falling back to the system tool for a value this
/// version doesn't know (a newer version's tool), so the rest of the
/// settings file still loads.
pub fn deserialize_lenient<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<CaptureTool, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).unwrap_or_default())
}

/// How an image comes back from a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Kind {
    /// Today's capture: the snipping bar or the screenshot portal.
    System,
    /// Aktar runs the tool and reads the file it wrote once it exits
    /// (Flameshot's `-p`). Exit without a file is a cancel.
    Run,
    /// Aktar captures as `System` does, then opens the image in the editor
    /// and reads the file it wrote once it exits (Satty, Swappy).
    EditAfter,
    /// Aktar starts the tool and waits for a new image on the clipboard or
    /// in its save folder (Spectacle).
    Wait,
    /// The tool sends its finished image to Aktar (`--upload <file>`), set
    /// up once in the tool. Aktar's own Capture can start only ShareX this
    /// way; for the others it falls back to `System`.
    Send,
}

/// Windows' tools, in the order Settings lists them.
pub const WINDOWS_TOOLS: [CaptureTool; 6] =
    [CaptureTool::System, CaptureTool::ShareX, CaptureTool::Flameshot, CaptureTool::Greenshot, CaptureTool::Snagit, CaptureTool::PicPick];
/// Linux's tools, in the order Settings lists them.
pub const LINUX_TOOLS: [CaptureTool; 5] = [CaptureTool::System, CaptureTool::Flameshot, CaptureTool::Satty, CaptureTool::Swappy, CaptureTool::Spectacle];

/// The longest Aktar waits for an image from a tool that's being edited
/// in: editing takes its time.
pub const MAX_TOOL_WAIT: Duration = Duration::from_secs(10 * 60);
/// A file in a save folder is read once its size has stayed the same this
/// long, as watched folders do.
pub const STABLE_FOR: Duration = Duration::from_millis(500);

impl CaptureTool {
    /// The tools this platform offers.
    pub fn for_this_platform() -> &'static [CaptureTool] {
        if cfg!(target_os = "linux") {
            &LINUX_TOOLS
        } else {
            &WINDOWS_TOOLS
        }
    }

    pub fn is_on_this_platform(self) -> bool {
        Self::for_this_platform().contains(&self)
    }

    /// Its name, never translated.
    pub fn name(self) -> &'static str {
        match self {
            Self::System => "",
            Self::Flameshot => "Flameshot",
            Self::ShareX => "ShareX",
            Self::Greenshot => "Greenshot",
            Self::Snagit => "Snagit",
            Self::PicPick => "PicPick",
            Self::Satty => "Satty",
            Self::Swappy => "Swappy",
            Self::Spectacle => "Spectacle",
        }
    }

    pub fn kind(self) -> Kind {
        match self {
            Self::System => Kind::System,
            Self::Flameshot => Kind::Run,
            Self::Satty | Self::Swappy => Kind::EditAfter,
            Self::Spectacle => Kind::Wait,
            Self::ShareX | Self::Greenshot | Self::Snagit | Self::PicPick => Kind::Send,
        }
    }

    /// Whether Aktar's own Capture can start the tool. A `Send` tool that
    /// can't be started is used through its own shortcut, and Aktar's
    /// Capture uses the system tool instead.
    pub fn can_start(self) -> bool {
        !matches!(self, Self::Greenshot | Self::Snagit | Self::PicPick)
    }

    /// Where to get it, for one that isn't installed.
    pub fn website(self) -> &'static str {
        match self {
            Self::System => "",
            Self::Flameshot => "https://flameshot.org/",
            Self::ShareX => "https://getsharex.com/",
            Self::Greenshot => "https://getgreenshot.org/",
            Self::Snagit => "https://www.techsmith.com/snagit/",
            Self::PicPick => "https://picpick.app/",
            Self::Satty => "https://github.com/gabm/Satty",
            Self::Swappy => "https://github.com/jtheoof/swappy",
            Self::Spectacle => "https://apps.kde.org/spectacle/",
        }
    }

    /// The executable's file names, looked for on PATH and in the usual
    /// install folders.
    fn executable_names(self) -> &'static [&'static str] {
        match self {
            Self::System => &[],
            Self::Flameshot if cfg!(windows) => &["flameshot.exe"],
            Self::Flameshot => &["flameshot"],
            Self::ShareX => &["ShareX.exe"],
            Self::Greenshot => &["Greenshot.exe"],
            Self::Snagit => &["SnagitCapture.exe", "Snagit32.exe"],
            Self::PicPick => &["picpick.exe"],
            Self::Satty => &["satty"],
            Self::Swappy => &["swappy"],
            Self::Spectacle => &["spectacle"],
        }
    }

    /// Install folders (other than PATH) the executable may be in, under
    /// Program Files and the like.
    fn install_folders(self) -> Vec<PathBuf> {
        if !cfg!(windows) {
            return Vec::new();
        }
        let roots: Vec<PathBuf> = ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"]
            .iter()
            .filter_map(std::env::var_os)
            .map(PathBuf::from)
            .chain(std::env::var_os("LOCALAPPDATA").map(|local| PathBuf::from(local).join("Programs")))
            .collect();
        let mut folders = Vec::new();
        for root in roots {
            match self {
                Self::Flameshot => {
                    folders.push(root.join("Flameshot").join("bin"));
                    folders.push(root.join("Flameshot"));
                }
                Self::ShareX => folders.push(root.join("ShareX")),
                Self::Greenshot => folders.push(root.join("Greenshot")),
                Self::PicPick => folders.push(root.join("PicPick")),
                Self::Snagit => {
                    // "TechSmith\Snagit 2026", one folder per yearly version.
                    if let Ok(entries) = std::fs::read_dir(root.join("TechSmith")) {
                        let mut versions: Vec<PathBuf> = entries
                            .flatten()
                            .map(|entry| entry.path())
                            .filter(|path| path.file_name().is_some_and(|name| name.to_string_lossy().starts_with("Snagit")))
                            .collect();
                        versions.sort();
                        folders.extend(versions.into_iter().rev());
                    }
                }
                _ => {}
            }
        }
        folders
    }

    /// Where the tool's executable is on this computer, if it's installed.
    pub fn find(self) -> Option<PathBuf> {
        if self == Self::System {
            return None;
        }
        let mut folders: Vec<PathBuf> = std::env::var_os("PATH").map(|path| std::env::split_paths(&path).collect()).unwrap_or_default();
        folders.extend(self.install_folders());
        #[cfg(windows)]
        if self == Self::ShareX {
            // Its installer's own record of where it went.
            folders.extend(crate::sharex::Places::of_this_pc().exe_folders);
        }
        find_in(&folders, self.executable_names())
    }

    /// The tool's arguments. `output` is the file the tool writes the
    /// finished image to, and `input` the screenshot an editor opens.
    pub fn arguments(self, input: Option<&Path>, output: &Path) -> Vec<OsString> {
        let mut arguments: Vec<OsString> = Vec::new();
        match self {
            // The editor comes first; with a "final action" (-p) its
            // Copy and Save buttons become Accept, which writes the file
            // and ends the process.
            Self::Flameshot => {
                arguments.extend(["gui".into(), "-p".into(), output.into()]);
            }
            // Region capture, then ShareX's own after-capture tasks
            // (Annotate, Save, Perform actions: `--upload "$input"`).
            Self::ShareX => arguments.push("-RectangleRegion".into()),
            // Saving (Ctrl+S or Enter) writes `output` and quits; Copy
            // saves too, so a copied image comes back as well.
            Self::Satty => {
                arguments.extend(["--filename".into(), input.map(OsString::from).unwrap_or_default()]);
                arguments.extend(["--output-filename".into(), output.into()]);
                arguments.extend(["--early-exit".into(), "--save-after-copy".into(), "--actions-on-enter".into(), "save-to-file".into()]);
            }
            // Writes the final image to `output` when its window closes.
            Self::Swappy => {
                arguments.extend(["-f".into(), input.map(OsString::from).unwrap_or_default()]);
                arguments.extend(["-o".into(), output.into()]);
            }
            // A rectangular region in Spectacle's own window, where it can
            // be annotated; the image comes back through its Copy or Save.
            Self::Spectacle => arguments.extend(["--region".into(), "--new-instance".into()]),
            Self::System | Self::Greenshot | Self::Snagit | Self::PicPick => {}
        }
        arguments
    }

    /// Where Spectacle saves on its own when nothing else is set:
    /// `~/Pictures/Screenshots` (Plasma 6), or the Pictures folder.
    pub fn default_save_folder(self) -> Option<PathBuf> {
        if self != Self::Spectacle {
            return None;
        }
        let pictures = pictures_folder()?;
        let screenshots = pictures.join("Screenshots");
        Some(if screenshots.is_dir() { screenshots } else { pictures })
    }
}

/// `$XDG_PICTURES_DIR` from user-dirs.dirs, or `~/Pictures`.
fn pictures_folder() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config"));
    if let Ok(text) = std::fs::read_to_string(config.join("user-dirs.dirs")) {
        if let Some(folder) = parse_user_dir(&text, "XDG_PICTURES_DIR", &home) {
            return Some(folder);
        }
    }
    Some(home.join("Pictures"))
}

/// One folder from user-dirs.dirs: `XDG_PICTURES_DIR="$HOME/Bilder"`.
pub fn parse_user_dir(text: &str, name: &str, home: &Path) -> Option<PathBuf> {
    for line in text.lines() {
        let line = line.trim();
        let Some(value) = line.strip_prefix(name).and_then(|rest| rest.trim_start().strip_prefix('=')) else { continue };
        let value = value.trim().trim_matches('"');
        if value.is_empty() {
            return None;
        }
        return Some(match value.strip_prefix("$HOME") {
            Some(rest) => home.join(rest.trim_start_matches('/')),
            None => PathBuf::from(value),
        });
    }
    None
}

/// The first `names` file in `folders`, in order.
pub fn find_in(folders: &[PathBuf], names: &[&str]) -> Option<PathBuf> {
    folders.iter().flat_map(|folder| names.iter().map(move |name| folder.join(name))).find(|path| path.is_file())
}

/// The command a tool runs to send a file to Aktar, as the tool's setup
/// shows it: `"<exe>" --upload "<placeholder>"`. ShareX's placeholder is
/// `$input`, Greenshot's `{0}`.
pub fn upload_command(executable: &str, placeholder: &str) -> String {
    format!("\"{executable}\" {} \"{placeholder}\"", crate::shell::UPLOAD_FLAG)
}

/// The arguments alone (for a tool with a separate field for them).
pub fn upload_arguments(placeholder: &str) -> String {
    format!("{} \"{placeholder}\"", crate::shell::UPLOAD_FLAG)
}

/// The alias the Microsoft Store package declares for aktar.exe
/// (packaging/msix/AppxManifest.xml). The package's own folder changes with
/// every update, so a tool is pointed at the alias instead, which Windows
/// keeps in %LOCALAPPDATA%\Microsoft\WindowsApps. Not `aktar.exe`: that
/// would shadow the `aktar` command of the CLI (npm) on PATH.
pub const PACKAGE_ALIAS: &str = "aktar-desktop.exe";

/// The program a tool runs to send Aktar a file: the package's alias in a
/// Microsoft Store install, else this copy's executable (or AppImage).
pub fn aktar_executable() -> Option<PathBuf> {
    if crate::package::is_packaged() {
        let local = std::env::var_os("LOCALAPPDATA")?;
        return Some(PathBuf::from(local).join("Microsoft").join("WindowsApps").join(PACKAGE_ALIAS));
    }
    #[cfg(target_os = "linux")]
    return crate::linux::launch_command();
    #[cfg(not(target_os = "linux"))]
    std::env::current_exe().ok()
}

/// One tool as Settings shows it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolStatus {
    pub id: CaptureTool,
    pub name: &'static str,
    pub kind: Kind,
    /// Aktar's Capture can start it (else it's used with its own shortcut).
    pub can_start: bool,
    pub installed: bool,
    pub website: &'static str,
    /// The tool's own save folder, for one Aktar waits for.
    pub default_folder: Option<String>,
    /// For a tool that sends files to Aktar: the program and its arguments
    /// with the tool's placeholder, and the two as one command line.
    pub upload_program: Option<String>,
    pub upload_arguments: Option<String>,
    pub upload_command: Option<String>,
}

/// The tools of this platform, as Settings lists them.
pub fn statuses() -> Vec<ToolStatus> {
    let executable = aktar_executable().map(|path| path.to_string_lossy().into_owned());
    CaptureTool::for_this_platform()
        .iter()
        .map(|&tool| {
            let sends = tool.kind() == Kind::Send;
            let program = executable.clone().filter(|_| sends);
            ToolStatus {
                id: tool,
                name: tool.name(),
                kind: tool.kind(),
                can_start: tool.can_start(),
                installed: tool == CaptureTool::System || tool.find().is_some(),
                website: tool.website(),
                default_folder: tool.default_save_folder().map(|folder| folder.to_string_lossy().into_owned()),
                upload_arguments: sends.then(|| upload_arguments(placeholder(tool))),
                upload_command: program.as_deref().map(|program| upload_command(program, placeholder(tool))),
                upload_program: program,
            }
        })
        .collect()
}

/// The file name a tool's own placeholder stands for, per tool.
pub fn placeholder(tool: CaptureTool) -> &'static str {
    match tool {
        CaptureTool::ShareX => "$input",
        CaptureTool::Greenshot => "{0}",
        _ => "%1",
    }
}

const IMAGE_EXTENSIONS: [&str; 8] = ["png", "jpg", "jpeg", "webp", "gif", "bmp", "tif", "tiff"];

pub fn is_image(path: &Path) -> bool {
    path.extension().and_then(|extension| extension.to_str()).is_some_and(|extension| IMAGE_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str()))
}

/// Watches a save folder for the image a tool saves during one capture:
/// the files already there (by name, size and time) are what it started
/// with, and the first image that's new or rewritten since, and whose size
/// then stays the same for `STABLE_FOR`, is the capture.
pub struct FolderWatch {
    folder: PathBuf,
    known: std::collections::HashMap<PathBuf, (u64, SystemTime)>,
    /// A candidate and its size when last seen, with when it was.
    settling: Option<(PathBuf, u64, std::time::Instant)>,
}

impl FolderWatch {
    pub fn start(folder: PathBuf) -> Self {
        let known = scan(&folder).into_iter().map(|(path, size, modified)| (path, (size, modified))).collect();
        Self { folder, known, settling: None }
    }

    /// The new image, once there is one and it's done being written.
    pub fn poll(&mut self) -> Option<PathBuf> {
        let mut newest: Option<(PathBuf, u64, SystemTime)> = None;
        for (path, size, modified) in scan(&self.folder) {
            if size == 0 || self.known.get(&path) == Some(&(size, modified)) {
                continue;
            }
            if newest.as_ref().is_none_or(|(_, _, time)| modified > *time) {
                newest = Some((path, size, modified));
            }
        }
        let (path, size, _) = newest?;
        match &self.settling {
            Some((settling, last_size, since)) if *settling == path && *last_size == size => {
                if since.elapsed() >= STABLE_FOR {
                    return Some(path);
                }
            }
            _ => self.settling = Some((path, size, std::time::Instant::now())),
        }
        None
    }
}

/// The images directly in `folder` (not in subfolders), with their sizes
/// and modification times.
fn scan(folder: &Path) -> Vec<(PathBuf, u64, SystemTime)> {
    let Ok(entries) = std::fs::read_dir(folder) else { return Vec::new() };
    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if !is_image(&path) || path.file_name().is_some_and(|name| name.to_string_lossy().starts_with('.')) {
                return None;
            }
            let metadata = entry.metadata().ok().filter(std::fs::Metadata::is_file)?;
            Some((path, metadata.len(), metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_through_serde() {
        let ids = ["system", "flameshot", "sharex", "greenshot", "snagit", "picpick", "satty", "swappy", "spectacle"];
        for id in ids {
            let tool: CaptureTool = serde_json::from_str(&format!("\"{id}\"")).unwrap();
            assert_eq!(serde_json::to_string(&tool).unwrap(), format!("\"{id}\""));
        }
        for tool in WINDOWS_TOOLS.iter().chain(LINUX_TOOLS.iter()) {
            let json = serde_json::to_string(tool).unwrap();
            assert!(ids.iter().any(|id| json == format!("\"{id}\"")));
        }
        assert_eq!(CaptureTool::default(), CaptureTool::System);
    }

    #[test]
    fn an_unknown_tool_reads_as_the_system_one() {
        #[derive(Deserialize)]
        struct Holder {
            #[serde(deserialize_with = "deserialize_lenient")]
            tool: CaptureTool,
        }
        let holder: Holder = serde_json::from_str(r#"{"tool":"cleanshot"}"#).unwrap();
        assert_eq!(holder.tool, CaptureTool::System);
        let holder: Holder = serde_json::from_str(r#"{"tool":"flameshot"}"#).unwrap();
        assert_eq!(holder.tool, CaptureTool::Flameshot);
        let settings: crate::settings::Settings = serde_json::from_str(r#"{"captureTool":"later","showNotification":false}"#).unwrap();
        assert_eq!(settings.capture_tool, CaptureTool::System);
        assert!(!settings.show_notification);
    }

    #[test]
    fn kinds() {
        assert_eq!(CaptureTool::Flameshot.kind(), Kind::Run);
        assert_eq!(CaptureTool::Satty.kind(), Kind::EditAfter);
        assert_eq!(CaptureTool::Swappy.kind(), Kind::EditAfter);
        assert_eq!(CaptureTool::Spectacle.kind(), Kind::Wait);
        assert_eq!(CaptureTool::ShareX.kind(), Kind::Send);
        assert!(CaptureTool::ShareX.can_start());
        assert!(!CaptureTool::Greenshot.can_start());
        assert!(!CaptureTool::Snagit.can_start());
        assert!(!CaptureTool::PicPick.can_start());
    }

    #[test]
    fn arguments() {
        let output = Path::new("/tmp/out.png");
        let input = Path::new("/tmp/in.png");
        let strings = |tool: CaptureTool| -> Vec<String> {
            tool.arguments(Some(input), output).into_iter().map(|argument| argument.to_string_lossy().into_owned()).collect()
        };
        assert_eq!(strings(CaptureTool::Flameshot), ["gui", "-p", "/tmp/out.png"]);
        assert_eq!(strings(CaptureTool::ShareX), ["-RectangleRegion"]);
        assert_eq!(
            strings(CaptureTool::Satty),
            [
                "--filename",
                "/tmp/in.png",
                "--output-filename",
                "/tmp/out.png",
                "--early-exit",
                "--save-after-copy",
                "--actions-on-enter",
                "save-to-file"
            ]
        );
        assert_eq!(strings(CaptureTool::Swappy), ["-f", "/tmp/in.png", "-o", "/tmp/out.png"]);
        assert_eq!(strings(CaptureTool::Spectacle), ["--region", "--new-instance"]);
        assert!(strings(CaptureTool::Greenshot).is_empty());
    }

    #[test]
    fn upload_commands() {
        assert_eq!(upload_command(r"C:\Aktar\aktar.exe", "$input"), r#""C:\Aktar\aktar.exe" --upload "$input""#);
        assert_eq!(upload_arguments("{0}"), r#"--upload "{0}""#);
        assert_eq!(placeholder(CaptureTool::ShareX), "$input");
        assert_eq!(placeholder(CaptureTool::Greenshot), "{0}");
    }

    #[test]
    fn finds_the_first_folder_with_the_executable() {
        let root = std::env::temp_dir().join(format!("aktar-tools-{}", crate::util::new_id()));
        let (first, second) = (root.join("a"), root.join("b"));
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(second.join("flameshot"), b"").unwrap();
        assert_eq!(find_in(&[first.clone(), second.clone()], &["flameshot"]), Some(second.join("flameshot")));
        assert_eq!(find_in(std::slice::from_ref(&first), &["flameshot"]), None);
        // A folder of that name isn't the executable.
        std::fs::create_dir_all(first.join("flameshot")).unwrap();
        assert_eq!(find_in(&[first, second.clone()], &["flameshot"]), Some(second.join("flameshot")));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reads_the_pictures_folder() {
        let home = Path::new("/home/me");
        let text = "# comment\nXDG_DESKTOP_DIR=\"$HOME/Desktop\"\nXDG_PICTURES_DIR=\"$HOME/Resimler\"\n";
        assert_eq!(parse_user_dir(text, "XDG_PICTURES_DIR", home), Some(PathBuf::from("/home/me/Resimler")));
        assert_eq!(parse_user_dir("XDG_PICTURES_DIR=\"/data/pics\"", "XDG_PICTURES_DIR", home), Some(PathBuf::from("/data/pics")));
        assert_eq!(parse_user_dir("XDG_MUSIC_DIR=\"$HOME/Music\"", "XDG_PICTURES_DIR", home), None);
    }

    #[test]
    fn images_only() {
        assert!(is_image(Path::new("a.PNG")));
        assert!(is_image(Path::new("Screenshot_20261123.jpeg")));
        assert!(!is_image(Path::new("notes.txt")));
        assert!(!is_image(Path::new("noextension")));
    }

    #[test]
    fn folder_watch_takes_a_new_settled_image() {
        let folder = std::env::temp_dir().join(format!("aktar-watch-{}", crate::util::new_id()));
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("old.png"), b"old").unwrap();
        let mut watch = FolderWatch::start(folder.clone());
        assert_eq!(watch.poll(), None);
        // Not an image, and an empty file still being made: neither counts.
        std::fs::write(folder.join("notes.txt"), b"text").unwrap();
        std::fs::write(folder.join("new.png"), b"").unwrap();
        assert_eq!(watch.poll(), None);
        std::fs::write(folder.join("new.png"), b"image").unwrap();
        // Seen once: it has to stay the same size for a moment first.
        assert_eq!(watch.poll(), None);
        std::thread::sleep(STABLE_FOR + Duration::from_millis(50));
        assert_eq!(watch.poll(), Some(folder.join("new.png")));
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn folder_watch_notices_a_rewritten_file() {
        let folder = std::env::temp_dir().join(format!("aktar-watch-{}", crate::util::new_id()));
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("same.png"), b"one").unwrap();
        let mut watch = FolderWatch::start(folder.clone());
        std::fs::write(folder.join("same.png"), b"two and more").unwrap();
        assert_eq!(watch.poll(), None);
        std::thread::sleep(STABLE_FOR + Duration::from_millis(50));
        assert_eq!(watch.poll(), Some(folder.join("same.png")));
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn folder_watch_of_a_missing_folder_finds_nothing() {
        let mut watch = FolderWatch::start(PathBuf::from("/definitely/not/here"));
        assert_eq!(watch.poll(), None);
    }
}
