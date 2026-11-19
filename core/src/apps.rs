//! The applications installed on this desktop, from their `.desktop` files -
//! the list an app launcher shows. "open vscode" finds Visual Studio Code
//! (its keyword) and runs `code` (its `Exec`); "open episode 001" finds no
//! app, so it is not launched as one.

use std::path::{Path, PathBuf};

/// An installed application.
#[derive(Debug, Clone, PartialEq)]
pub struct App {
    pub name: String,
    /// The command line that starts it, without `%U`-style placeholders.
    pub command: String,
}

/// Where `.desktop` files live: the user's own first, then the system's.
pub fn app_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = dirs::data_dir() {
        dirs.push(home.join("applications"));
    }
    let system = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());
    dirs.extend(std::env::split_paths(&system).map(|d| d.join("applications")));
    dirs
}

/// One `.desktop` file's main entry.
struct Entry {
    stem: String,
    name: String,
    keywords: Vec<String>,
    exec: String,
}

fn read_entry(path: &Path) -> Option<Entry> {
    let text = std::fs::read_to_string(path).ok()?;
    let (mut in_main, mut name, mut exec, mut keywords) = (false, None, None, Vec::new());
    let (mut app_type, mut hidden) = (None, false);
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            // Only the main entry: actions ("New Private Window") have
            // their own Exec lines.
            in_main = line == "[Desktop Entry]";
            continue;
        }
        if !in_main {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "Name" => name = Some(value.trim().to_string()),
            "Exec" => exec = Some(value.trim().to_string()),
            "Type" => app_type = Some(value.trim().to_string()),
            "Keywords" => {
                keywords = value
                    .split(';')
                    .map(|k| k.trim().to_lowercase())
                    .filter(|k| !k.is_empty())
                    .collect()
            }
            "NoDisplay" | "Hidden" => hidden |= value.trim() == "true",
            _ => {}
        }
    }
    if hidden || app_type.as_deref() != Some("Application") {
        return None;
    }
    Some(Entry {
        stem: path.file_stem()?.to_string_lossy().to_lowercase(),
        name: name?,
        keywords,
        exec: exec?,
    })
}

/// The command without field codes: `code %F` is `code`, `%%` is `%`.
fn strip_field_codes(exec: &str) -> String {
    let mut out = Vec::new();
    for word in exec.split_whitespace() {
        if word.len() == 2 && word.starts_with('%') && word != "%%" {
            continue;
        }
        out.push(word.replace("%%", "%"));
    }
    out.join(" ")
}

fn program_name(exec: &str) -> String {
    let program = exec.split_whitespace().next().unwrap_or("");
    Path::new(program)
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

/// The installed app `wanted` names: its name, its file name, its program
/// or one of its keywords, in that order of preference, any case.
pub fn find(dirs: &[PathBuf], wanted: &str) -> Option<App> {
    let wanted = wanted.trim().to_lowercase();
    if wanted.is_empty() {
        return None;
    }
    let mut entries = Vec::new();
    for dir in dirs {
        let Ok(read) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut paths: Vec<PathBuf> = read
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "desktop"))
            .collect();
        paths.sort();
        entries.extend(paths.iter().filter_map(|p| read_entry(p)));
    }
    let rules: [&dyn Fn(&Entry) -> bool; 4] = [
        &|e| e.name.to_lowercase() == wanted,
        &|e| e.stem == wanted,
        &|e| program_name(&e.exec) == wanted,
        &|e| e.keywords.contains(&wanted),
    ];
    rules.iter().find_map(|rule| {
        entries.iter().find(|e| rule(e)).map(|e| App {
            name: e.name.clone(),
            command: strip_field_codes(&e.exec),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desktop(dir: &Path, file: &str, body: &str) {
        std::fs::write(dir.join(file), body).unwrap();
    }

    fn fixture() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jarvis-apps-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        desktop(&dir, "code.desktop", "[Desktop Entry]\nName=Visual Studio Code\nExec=code %F\nType=Application\nKeywords=vscode;\n");
        desktop(&dir, "code-url-handler.desktop", "[Desktop Entry]\nName=Visual Studio Code - URL Handler\nExec=code --open-url %U\nType=Application\nNoDisplay=true\nKeywords=vscode;\n");
        desktop(&dir, "firefox.desktop", "[Desktop Entry]\nName=Firefox\nExec=/usr/lib/firefox/firefox %u\nType=Application\n\n[Desktop Action new-private-window]\nName=New Private Window\nExec=/usr/lib/firefox/firefox --private-window %u\n");
        desktop(&dir, "mpv.desktop", "[Desktop Entry]\nName=mpv Media Player\nExec=mpv --player-operation-mode=pseudo-gui -- %U\nType=Application\nKeywords=mpv;media;player;video;\n");
        dir
    }

    #[test]
    fn an_app_is_found_by_name_file_program_or_keyword() {
        let dir = fixture();
        let dirs = vec![dir.clone()];
        let command = |w: &str| find(&dirs, w).map(|a| a.command);
        assert_eq!(command("vscode").as_deref(), Some("code"), "keyword");
        assert_eq!(
            command("Visual Studio Code").as_deref(),
            Some("code"),
            "name"
        );
        assert_eq!(command("code").as_deref(), Some("code"), "file");
        assert_eq!(
            command("firefox").as_deref(),
            Some("/usr/lib/firefox/firefox"),
            "main entry, not an action"
        );
        assert_eq!(
            command("mpv").as_deref(),
            Some("mpv --player-operation-mode=pseudo-gui --")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn words_that_name_no_app_find_nothing() {
        let dir = fixture();
        let dirs = vec![dir.clone()];
        for not_an_app in ["episode 001", "them", ""] {
            assert_eq!(find(&dirs, not_an_app), None, "{not_an_app:?}");
        }
        // A keyword is a name the app answers to.
        assert_eq!(find(&dirs, "media").unwrap().name, "mpv Media Player");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_hidden_handler_is_never_the_app() {
        let dir = fixture();
        let app = find(std::slice::from_ref(&dir), "vscode").unwrap();
        assert_eq!(app.name, "Visual Studio Code");
        let _ = std::fs::remove_dir_all(dir);
    }
}
