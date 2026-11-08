//! Local editor discovery and argv-only launching. Nothing here executes during discovery.
use serde::{Deserialize, Serialize};
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

type KnownEditor = (
    &'static str,
    &'static str,
    EditorKind,
    &'static [&'static str],
    &'static str,
);
const KNOWN: &[KnownEditor] = &[
    (
        "code",
        "VS Code",
        EditorKind::Gui,
        &["code"],
        "Visual Studio Code",
    ),
    ("cursor", "Cursor", EditorKind::Gui, &["cursor"], "Cursor"),
    ("zed", "Zed", EditorKind::Gui, &["zed", "zeditor"], "Zed"),
    (
        "subl",
        "Sublime Text",
        EditorKind::Gui,
        &["subl"],
        "Sublime Text",
    ),
    (
        "idea",
        "IntelliJ IDEA",
        EditorKind::Gui,
        &["idea"],
        "IntelliJ IDEA",
    ),
    (
        "pycharm",
        "PyCharm",
        EditorKind::Gui,
        &["pycharm"],
        "PyCharm",
    ),
    (
        "webstorm",
        "WebStorm",
        EditorKind::Gui,
        &["webstorm"],
        "WebStorm",
    ),
    (
        "rustrover",
        "RustRover",
        EditorKind::Gui,
        &["rustrover"],
        "RustRover",
    ),
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
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let mut path: Vec<_> = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .filter(|p| p.is_absolute())
            .collect();
        path.push(home.join(".local/share/JetBrains/Toolbox/scripts"));
        let mut data = vec![std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"))];
        data.extend(std::env::split_paths(
            &std::env::var_os("XDG_DATA_DIRS")
                .unwrap_or_else(|| "/usr/local/share:/usr/share".into()),
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
        candidates.into_iter().find(|p| {
            p.is_file()
                && p.metadata()
                    .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
        })
    }
    pub fn detect(&self) -> Vec<Editor> {
        let terminal = self
            .terminal
            .as_deref()
            .into_iter()
            .chain([
                "ghostty",
                "kitty",
                "alacritty",
                "wezterm",
                "gnome-terminal",
                "konsole",
                "xterm",
            ])
            .find_map(|name| {
                let path = self.executable(name)?;
                let kind = path.file_name()?.to_str()?.to_owned();
                Some((path, kind))
            })
            .or_else(|| cfg!(target_os = "macos").then(|| (PathBuf::from("/usr/bin/osascript"), "Terminal.app".to_owned())));
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
                        .map(|p| {
                            (
                                PathBuf::from("/usr/bin/open"),
                                vec!["-a".into(), p.into_os_string()],
                            )
                        })
                })
                .or_else(|| {
                    if kind == EditorKind::Gui {
                        self.desktop(commands)
                    } else {
                        None
                    }
                });
            if let Some((program, args)) = found {
                let mut editor = Editor {
                    view: EditorView {
                        id: id.into(),
                        name: name.into(),
                        kind,
                    },
                    program,
                    args,
                };
                if kind == EditorKind::Terminal {
                    let (program, terminal_kind) = terminal.as_ref().unwrap();
                    editor.args = terminal_prefix(terminal_kind);
                    editor.args.push(editor.program.into_os_string());
                    editor.program = program.clone();
                }
                result.push(editor);
            }
        }
        for (id, name) in [
            ("file-manager", "File manager"),
            ("default-app", "Default app"),
        ] {
            result.push(Editor {
                view: EditorView {
                    id: id.into(),
                    name: name.into(),
                    kind: EditorKind::System,
                },
                program: if cfg!(target_os = "macos") {
                    "/usr/bin/open".into()
                } else {
                    self.executable("xdg-open")
                        .unwrap_or_else(|| "xdg-open".into())
                },
                args: vec![],
            });
        }
        result
    }
    fn desktop(&self, commands: &[&str]) -> Option<(PathBuf, Vec<OsString>)> {
        for dir in &self.data {
            let Ok(entries) = std::fs::read_dir(dir.join("applications")) else {
                continue;
            };
            for entry in entries.flatten() {
                if entry.path().extension().is_none_or(|e| e != "desktop") {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(entry.path()) else {
                    continue;
                };
                let Some(section) = text
                    .split("[Desktop Entry]")
                    .nth(1)
                    .and_then(|s| s.split("\n[").next())
                else {
                    continue;
                };
                if section
                    .lines()
                    .any(|l| matches!(l.trim(), "Hidden=true" | "NoDisplay=true"))
                {
                    continue;
                }
                let Some(exec) = section.lines().find_map(|l| l.strip_prefix("Exec=")) else {
                    continue;
                };
                let Some(mut words) = desktop_words(exec) else {
                    continue;
                };
                if words.is_empty() {
                    continue;
                }
                let program = words.remove(0);
                if !commands
                    .iter()
                    .any(|c| Path::new(&program).file_name().is_some_and(|n| n == *c))
                {
                    continue;
                }
                let Some(program) = self.executable(&program) else {
                    continue;
                };
                return Some((
                    program,
                    words
                        .into_iter()
                        .filter(|w| !w.starts_with('%'))
                        .map(Into::into)
                        .collect(),
                ));
            }
        }
        None
    }
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
    let path = path
        .canonicalize()
        .map_err(|e| format!("Item does not exist: {e}"))?;
    if !path.starts_with(&repo) || path.starts_with(repo.join("imported")) {
        return Err("Item must be inside the repo and cannot be an Import".into());
    }
    let folder = if rule {
        path.parent().ok_or("Rule has no parent")?.to_owned()
    } else {
        path.clone()
    };
    let file = if rule {
        path
    } else {
        folder.join("SKILL.md")
    }
    .canonicalize()
    .map_err(|e| e.to_string())?;
    if !folder.is_dir()
        || !file.is_file()
        || !file.starts_with(&repo)
        || file.starts_with(repo.join("imported"))
    {
        return Err("Item file must exist inside the repo and cannot be an Import".into());
    }
    Ok(Target {
        folder,
        file,
        rule,
    })
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
    let mut child = launch_command(editor, target)
        .spawn()
        .map_err(|e| format!("Cannot open {}: {e}", editor.view.name))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}
