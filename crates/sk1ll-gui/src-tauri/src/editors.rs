//! Local editor discovery and argv-only launching. Nothing here executes during discovery.
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::os::unix::{fs::PermissionsExt, process::CommandExt};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EditorKind {
    Gui,
    Terminal,
    System,
}
#[derive(Clone, Debug, Serialize)]
pub struct EditorView {
    pub id: String,
    pub name: String,
    pub kind: EditorKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}
#[derive(Clone, Debug)]
pub(crate) struct Editor {
    pub view: EditorView,
    pub program: PathBuf,
    pub args: Vec<OsString>,
}
#[derive(Default)]
pub struct EditorCache(pub(crate) Mutex<Option<Vec<Editor>>>);
#[derive(Serialize)]
pub struct EditorsView {
    pub editors: Vec<EditorView>,
    pub preferred_editor: String,
}
#[derive(Default, Serialize, Deserialize)]
pub(crate) struct Preferences {
    pub preferred_editor: Option<String>,
}

type KnownEditor = (&'static str, &'static str, EditorKind, &'static [&'static str], &'static str);
const KNOWN: &[KnownEditor] = &[
    ("code", "VS Code", EditorKind::Gui, &["code"], "Visual Studio Code"),
    ("cursor", "Cursor", EditorKind::Gui, &["cursor"], "Cursor"),
    ("zed", "Zed", EditorKind::Gui, &["zed", "zeditor"], "Zed"),
    ("subl", "Sublime Text", EditorKind::Gui, &["subl"], "Sublime Text"),
    ("idea", "IntelliJ IDEA", EditorKind::Gui, &["idea"], "IntelliJ IDEA"),
    ("pycharm", "PyCharm", EditorKind::Gui, &["pycharm"], "PyCharm"),
    ("webstorm", "WebStorm", EditorKind::Gui, &["webstorm"], "WebStorm"),
    ("rustrover", "RustRover", EditorKind::Gui, &["rustrover"], "RustRover"),
    ("clion", "CLion", EditorKind::Gui, &["clion"], "CLion"),
    ("goland", "GoLand", EditorKind::Gui, &["goland"], "GoLand"),
    ("nvim", "Neovim", EditorKind::Terminal, &["nvim"], ""),
    ("vim", "Vim", EditorKind::Terminal, &["vim"], ""),
    ("hx", "Helix", EditorKind::Terminal, &["hx"], ""),
    ("nano", "nano", EditorKind::Terminal, &["nano"], ""),
];
pub(crate) struct Discovery {
    pub path: Vec<PathBuf>,
    pub data: Vec<PathBuf>,
    pub apps: Vec<PathBuf>,
    pub terminal: Option<String>,
}
impl Discovery {
    pub fn environment() -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        let mut path: Vec<_> =
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).filter(|p| p.is_absolute()).collect();
        path.push(home.join(".local/share/JetBrains/Toolbox/scripts"));
        let mut data =
            vec![std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".local/share"))];
        data.extend(std::env::split_paths(
            &std::env::var_os("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".into()),
        ));
        Self {
            path,
            data,
            apps: if cfg!(target_os = "macos") {
                vec!["/Applications".into(), home.join("Applications")]
            } else {
                vec![]
            },
            terminal: std::env::var("TERMINAL").ok(),
        }
    }
    fn executable(&self, name: &str) -> Option<PathBuf> {
        let candidates = if Path::new(name).is_absolute() {
            vec![PathBuf::from(name)]
        } else {
            self.path.iter().map(|p| p.join(name)).collect()
        };
        candidates.into_iter().find(|p| p.is_file() && p.metadata().is_ok_and(|m| m.permissions().mode() & 0o111 != 0))
    }
    pub fn detect(&self) -> Vec<Editor> {
        let terminal = self
            .terminal
            .as_deref()
            .into_iter()
            .chain(["ghostty", "kitty", "alacritty", "wezterm", "gnome-terminal", "konsole", "xterm"])
            .find_map(|name| {
                let path = self.executable(name)?;
                let kind = path.file_name()?.to_str()?.to_owned();
                Some((path, kind))
            })
            .or_else(|| {
                cfg!(target_os = "macos").then(|| (PathBuf::from("/usr/bin/osascript"), "Terminal.app".to_owned()))
            });
        let entries = self.desktop_entries();
        let mut result = Vec::new();
        for &(id, name, kind, commands, bundle) in KNOWN {
            if kind == EditorKind::Terminal && terminal.is_none() {
                continue;
            }
            let found = commands
                .iter()
                .find_map(|c| self.executable(c))
                .map(|p| (p, vec![]))
                .or_else(|| {
                    self.apps
                        .iter()
                        .map(|p| p.join(format!("{bundle}.app")))
                        .find(|p| p.is_dir())
                        .map(|p| (PathBuf::from("/usr/bin/open"), vec!["-a".into(), p.into_os_string()]))
                })
                .or_else(|| if kind == EditorKind::Gui { self.desktop(commands, &entries) } else { None });
            if let Some((program, args)) = found {
                let icon = self.icon(commands, bundle, &entries);
                let mut editor =
                    Editor { view: EditorView { id: id.into(), name: name.into(), kind, icon }, program, args };
                if kind == EditorKind::Terminal {
                    let (program, terminal_kind) = terminal.as_ref().unwrap();
                    editor.args = terminal_prefix(terminal_kind);
                    editor.args.push(editor.program.into_os_string());
                    editor.program = program.clone();
                }
                result.push(editor);
            }
        }
        for (id, name) in [("file-manager", "File manager"), ("default-app", "Default app")] {
            result.push(Editor {
                view: EditorView { id: id.into(), name: name.into(), kind: EditorKind::System, icon: None },
                program: if cfg!(target_os = "macos") {
                    "/usr/bin/open".into()
                } else {
                    self.executable("xdg-open").unwrap_or_else(|| "xdg-open".into())
                },
                args: vec![],
            });
        }
        result
    }
    fn desktop_entries(&self) -> Vec<DesktopEntry> {
        let mut result = vec![];
        for dir in &self.data {
            let Ok(entries) = std::fs::read_dir(dir.join("applications")) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|e| e != "desktop") {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let Some(section) = text.split("[Desktop Entry]").nth(1).and_then(|s| s.split("\n[").next()) else {
                    continue;
                };
                if section.lines().any(|l| matches!(l.trim(), "Hidden=true" | "NoDisplay=true")) {
                    continue;
                }
                let value = |key: &str| section.lines().find_map(|l| l.strip_prefix(key)).map(str::to_owned);
                result.push(DesktopEntry {
                    stem: path.file_stem().unwrap_or_default().to_string_lossy().into_owned(),
                    exec: value("Exec="),
                    icon: value("Icon="),
                });
            }
        }
        result
    }
    fn desktop(&self, commands: &[&str], entries: &[DesktopEntry]) -> Option<(PathBuf, Vec<OsString>)> {
        for entry in entries {
            let Some(mut words) = entry.exec.as_deref().and_then(desktop_words) else {
                continue;
            };
            if words.is_empty() {
                continue;
            }
            let program = words.remove(0);
            if !commands.iter().any(|c| Path::new(&program).file_name().is_some_and(|n| n == *c)) {
                continue;
            }
            let Some(program) = self.executable(&program) else {
                continue;
            };
            return Some((program, words.into_iter().filter(|w| !w.starts_with('%')).map(Into::into).collect()));
        }
        None
    }
    // The macOS bundle icon wins; otherwise the icon of a desktop entry that launches or is named after the editor.
    fn icon(&self, commands: &[&str], bundle: &str, entries: &[DesktopEntry]) -> Option<String> {
        let app = self.apps.iter().map(|p| p.join(format!("{bundle}.app"))).find(|p| !bundle.is_empty() && p.is_dir());
        if let Some(icon) = app.and_then(|app| bundle_icon(&app)) {
            return Some(icon);
        }
        let named = |name: &str| commands.iter().any(|c| name.eq_ignore_ascii_case(c));
        entries
            .iter()
            .filter(|e| {
                let program = e.exec.as_deref().and_then(desktop_words).and_then(|w| w.into_iter().next());
                program.is_some_and(|p| Path::new(&p).file_name().and_then(|n| n.to_str()).is_some_and(named))
                    || e.stem.rsplit('.').next().is_some_and(named)
            })
            .find_map(|e| self.theme_icon(e.icon.as_deref()?))
    }
    fn theme_icon(&self, name: &str) -> Option<String> {
        if Path::new(name).is_absolute() {
            return image_uri(Path::new(name));
        }
        if name.is_empty() || name.contains('/') {
            return None;
        }
        let files = if name.ends_with(".svg") || name.ends_with(".png") {
            vec![name.to_owned()]
        } else {
            vec![format!("{name}.svg"), format!("{name}.png")]
        };
        self.data
            .iter()
            .flat_map(|dir| {
                ICON_SIZES
                    .iter()
                    .map(move |size| dir.join("icons/hicolor").join(size).join("apps"))
                    .chain([dir.join("pixmaps")])
            })
            .flat_map(|dir| files.iter().map(move |file| dir.join(file)))
            .find_map(|path| image_uri(&path))
    }
}
pub(crate) struct DesktopEntry {
    stem: String,
    exec: Option<String>,
    icon: Option<String>,
}
const ICON_SIZES: &[&str] = &["scalable", "128x128", "256x256", "96x96", "64x64", "512x512", "48x48", "32x32"];
const ICON_LIMIT: u64 = 1 << 20;
const ICNS_LIMIT: u64 = 8 << 20;
fn read_limited(path: &Path, limit: u64) -> Option<Vec<u8>> {
    let file = std::fs::File::open(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut bytes = vec![];
    file.take(limit + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 <= limit).then_some(bytes)
}
fn data_uri(mime: &str, bytes: &[u8]) -> String {
    format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes))
}
fn image_uri(path: &Path) -> Option<String> {
    let mime = match path.extension()?.to_str()? {
        "svg" => "image/svg+xml",
        "png" => "image/png",
        _ => return None,
    };
    Some(data_uri(mime, &read_limited(path, ICON_LIMIT)?))
}
fn bundle_icon(app: &Path) -> Option<String> {
    let contents = app.join("Contents");
    let plist = std::fs::read_to_string(contents.join("Info.plist")).ok()?;
    let name = plist
        .split("<key>CFBundleIconFile</key>")
        .nth(1)?
        .trim_start()
        .strip_prefix("<string>")?
        .split("</string>")
        .next()
        .filter(|name| !name.is_empty() && !name.contains('/'))?;
    let file = if name.ends_with(".icns") { name.to_owned() } else { format!("{name}.icns") };
    let bytes = read_limited(&contents.join("Resources").join(file), ICNS_LIMIT)?;
    icns_png(&bytes).map(|png| data_uri("image/png", png))
}
// Modern icns chunks embed PNGs; prefer one that is small but still sharp at HiDPI.
pub(crate) fn icns_png(bytes: &[u8]) -> Option<&[u8]> {
    let mut rest = bytes.strip_prefix(b"icns")?.get(4..)?;
    let mut chunks = vec![];
    while rest.len() >= 8 {
        let len = u32::from_be_bytes(rest[4..8].try_into().unwrap()) as usize;
        if len < 8 || len > rest.len() {
            break;
        }
        chunks.push((&rest[..4], &rest[8..len]));
        rest = &rest[len..];
    }
    ["ic07", "ic13", "ic08", "ic14", "ic09", "ic10"].iter().find_map(|kind| {
        chunks.iter().find(|(k, data)| *k == kind.as_bytes() && data.starts_with(b"\x89PNG")).map(|(_, data)| *data)
    })
}
// Desktop Exec quoting only; shell operators remain literal argv and are never evaluated.
fn desktop_words(input: &str) -> Option<Vec<String>> {
    let mut words = vec![];
    let mut word = String::new();
    let mut quoted = false;
    let mut escape = false;
    for c in input.chars() {
        if escape {
            word.push(c);
            escape = false;
        } else if c == '\\' {
            escape = true;
        } else if c == '"' {
            quoted = !quoted;
        } else if c.is_whitespace() && !quoted {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
        } else {
            word.push(c);
        }
    }
    if quoted || escape {
        return None;
    }
    if !word.is_empty() {
        words.push(word);
    }
    Some(words)
}
// Pass argv through osascript's run handler. AppleScript quotes each argument
// for Terminal's shell, including paths containing quotes or shell operators.
const MAC_TERMINAL_SCRIPT: &str = r#"on run argv
    set shellCommand to ""
    repeat with argument in argv
        set shellCommand to shellCommand & quoted form of (argument as text) & " "
    end repeat
    tell application "Terminal"
        activate
        do script shellCommand
    end tell
end run"#;
pub(crate) fn terminal_prefix(kind: &str) -> Vec<OsString> {
    match kind {
        "Terminal.app" => vec!["-e", MAC_TERMINAL_SCRIPT, "--"],
        "wezterm" => vec!["start", "--"],
        "gnome-terminal" => vec!["--"],
        "kitty" => vec!["--"],
        _ => vec!["-e"],
    }
    .into_iter()
    .map(Into::into)
    .collect()
}
pub(crate) struct Target {
    pub folder: PathBuf,
    pub file: PathBuf,
    pub rule: bool,
}
pub(crate) fn validate_target(repo: &Path, path: &Path, rule: bool) -> Result<Target, String> {
    let repo = repo.canonicalize().map_err(|e| e.to_string())?;
    if !path.is_absolute() {
        return Err("Item path must be absolute".into());
    }
    if path.starts_with(repo.join("imported")) {
        return Err("Fork this Import before opening it in an editor".into());
    }
    let path = path.canonicalize().map_err(|e| format!("Item does not exist: {e}"))?;
    if !path.starts_with(&repo) || path.starts_with(repo.join("imported")) {
        return Err("Item must be inside the repo and cannot be an Import".into());
    }
    let folder = if rule { path.parent().ok_or("Rule has no parent")?.to_owned() } else { path.clone() };
    let file = if rule { path } else { folder.join("SKILL.md") }.canonicalize().map_err(|e| e.to_string())?;
    if !folder.is_dir() || !file.is_file() || !file.starts_with(&repo) || file.starts_with(repo.join("imported")) {
        return Err("Item file must exist inside the repo and cannot be an Import".into());
    }
    Ok(Target { folder, file, rule })
}
pub(crate) fn target_path<'a>(editor: &Editor, target: &'a Target) -> &'a Path {
    match editor.view.id.as_str() {
        "file-manager" => &target.folder,
        "default-app" => &target.file,
        _ if editor.view.kind == EditorKind::Gui && !target.rule => &target.folder,
        _ => &target.file,
    }
}
pub(crate) fn launch_command(editor: &Editor, target: &Target) -> Command {
    let mut command = Command::new(&editor.program);
    command
        .args(&editor.args)
        .arg(target_path(editor, target))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.process_group(0);
    command
}

pub(crate) fn launch(editor: &Editor, target: &Target) -> Result<(), String> {
    let mut child =
        launch_command(editor, target).spawn().map_err(|e| format!("Cannot open {}: {e}", editor.view.name))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}
