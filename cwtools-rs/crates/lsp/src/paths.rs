use tower_lsp::lsp_types::Url;

/// Convert a `file://` URI string to a filesystem path string, applying
/// percent-decoding (e.g. `%20` → space). Returns the raw URI on failure.
pub(crate) fn uri_to_path_str(uri: &str) -> String {
    if let Ok(url) = Url::parse(uri)
        && let Ok(path) = url.to_file_path()
        && let Some(s) = path.to_str()
    {
        return s.to_string();
    }
    // Fallback: strip scheme manually (no percent-decode, but avoids a panic).
    uri.strip_prefix("file://").unwrap_or(uri).to_string()
}

/// Convert a filesystem path to a `file://` URI, percent-encoding special
/// characters. Uses the `url` crate so paths with spaces or non-ASCII round-trip
/// correctly through `uri_to_path_str`.
pub(crate) fn path_to_uri(path: &std::path::Path) -> String {
    Url::from_file_path(path)
        .map(|u| u.to_string())
        .unwrap_or_else(|_| format!("file://{}", path.display()))
}

/// Derive the logical path (relative to mod root) from a file:// URI and the
/// workspace root URI.  Falls back to the raw path if the workspace prefix
/// cannot be stripped.
pub(crate) fn logical_path_from_uri(uri: &str, workspace_uri: &Option<String>) -> String {
    let path = uri_to_path_str(uri);
    if let Some(ws) = workspace_uri {
        let ws_path = uri_to_path_str(ws);
        // Strip leading slash-terminated prefix
        let prefix = ws_path.trim_end_matches('/');
        if let Some(rel) = path.strip_prefix(prefix) {
            return rel.trim_start_matches('/').to_string();
        }
    }
    // Fallback: use the decoded path as-is
    path
}

/// Parse a string into an LSP Url, falling back to a clone of `fallback` on error.
pub(crate) fn parse_uri(uri_str: impl AsRef<str>, fallback: &Url) -> Url {
    uri_str
        .as_ref()
        .parse()
        .unwrap_or_else(|_| fallback.clone())
}

/// When the line prefix before the cursor reads `key =` (value not typed yet,
/// so the last good parse has no leaf there), return the key so value
/// completions can still resolve. `line0`/`char0` are LSP 0-based.
pub(crate) fn line_value_key(text: &str, line0: u32, char0: u32) -> Option<String> {
    let line = text.lines().nth(line0 as usize)?;
    let n = line.len().min(char0 as usize);
    let n = (0..=n).rev().find(|&i| line.is_char_boundary(i))?;
    let upto = &line[..n];
    let trimmed = upto.trim_end();
    let rest = trimmed
        .strip_suffix(['=', '<', '>'])?
        .trim_end_matches(['=', '<', '>', '!', '?'])
        .trim_end();
    let key = rest
        .rsplit(|c: char| c.is_whitespace() || c == '{')
        .next()?;
    if key.is_empty() || key.contains('}') || key.contains('"') {
        return None;
    }
    Some(key.to_string())
}

/// Best-effort discovery of a base-game install for `game`, checking the usual
/// OS cache directory for persistent caches, used when the client doesn't pass
/// `cacheDir`. Honors `XDG_CACHE_HOME`/`LOCALAPPDATA`, then `~/.cache` (Linux) or
/// `~/Library/Caches` (macOS), and finally the temp dir.
pub(crate) fn default_cache_dir() -> Option<std::path::PathBuf> {
    use std::path::PathBuf;
    if let Ok(x) = std::env::var("XDG_CACHE_HOME")
        && !x.is_empty()
    {
        return Some(PathBuf::from(x).join("cwtools"));
    }
    if let Ok(la) = std::env::var("LOCALAPPDATA")
        && !la.is_empty()
    {
        return Some(PathBuf::from(la).join("cwtools"));
    }
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        let home = PathBuf::from(home);
        #[cfg(target_os = "macos")]
        {
            return Some(home.join("Library/Caches/cwtools"));
        }
        #[cfg(not(target_os = "macos"))]
        {
            return Some(home.join(".cache/cwtools"));
        }
    }
    Some(std::env::temp_dir().join("cwtools"))
}

/// Steam library locations across platforms. Returns the first existing dir.
/// Used as a fallback when the client passes neither `vanilla` nor `vanillaCache`.
pub(crate) fn discover_vanilla_dir(game: &str) -> Option<std::path::PathBuf> {
    // Map our game id to the Steam "common" install folder name.
    let folder = match game {
        "hoi4" => "Hearts of Iron IV",
        "stellaris" => "Stellaris",
        "eu4" => "Europa Universalis IV",
        "ck2" => "Crusader Kings II",
        "ck3" => "Crusader Kings III",
        "vic2" => "Victoria 2",
        "vic3" => "Victoria 3",
        "ir" => "ImperatorRome",
        _ => return None,
    };

    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    // Steam library roots to probe (Linux, macOS, Windows).
    let mut roots: Vec<std::path::PathBuf> = Vec::new();
    if let Some(h) = &home {
        roots.push(h.join(".steam/steam/steamapps/common"));
        roots.push(h.join(".local/share/Steam/steamapps/common"));
        roots.push(h.join("Library/Application Support/Steam/steamapps/common"));
    }
    roots.push(std::path::PathBuf::from(
        "C:/Program Files (x86)/Steam/steamapps/common",
    ));
    roots.push(std::path::PathBuf::from(
        "C:/Program Files/Steam/steamapps/common",
    ));

    roots
        .into_iter()
        .map(|r| r.join(folder))
        .find(|p| p.is_dir())
}

/// Thin wrapper around the info crate's path check (avoids re-exporting it).
pub(crate) fn cwtools_info_path_check(
    opts: &cwtools_rules::rules_types::PathOptions,
    logical_path: &str,
) -> bool {
    if opts.paths.is_empty() {
        return true;
    }
    let norm = logical_path.replace('\\', "/");
    let dir = match norm.rfind('/') {
        Some(idx) => &norm[..idx],
        None => "",
    };
    let dir_lower = dir.to_lowercase();
    for p in &opts.paths {
        let pat = p.replace('\\', "/");
        let pat = pat.trim_matches('/');
        let pat_lower = pat.to_lowercase();
        if opts.path_strict {
            if dir_lower == pat_lower {
                return true;
            }
        } else {
            let after = &dir_lower[std::cmp::min(pat_lower.len(), dir_lower.len())..];
            if dir_lower.starts_with(&pat_lower) && (after.is_empty() || after.starts_with('/')) {
                return true;
            }
        }
    }
    false
}

/// Strip matching outer double quotes from a loc desc string for hover display.
/// `"Hello"` → `Hello`, `Hello` → `Hello`, `""` → `` (empty).
pub(crate) fn strip_loc_quotes(s: &str) -> &str {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Human-readable language name for hover display.
pub(crate) fn lang_display_name(lang: cwtools_localization::Lang) -> &'static str {
    match lang {
        cwtools_localization::Lang::English => "English",
        cwtools_localization::Lang::French => "French",
        cwtools_localization::Lang::German => "German",
        cwtools_localization::Lang::Spanish => "Spanish",
        cwtools_localization::Lang::Russian => "Russian",
        cwtools_localization::Lang::Polish => "Polish",
        cwtools_localization::Lang::BrazPor => "Brazilian Portuguese",
        cwtools_localization::Lang::SimpChinese => "Chinese",
        cwtools_localization::Lang::Japanese => "Japanese",
        cwtools_localization::Lang::Korean => "Korean",
        cwtools_localization::Lang::Turkish => "Turkish",
        cwtools_localization::Lang::Default => "Default",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_line_value_key() {
        let text = "decision = {\n    has_completed_focus = \n}\n";
        // Cursor right after `= ` on line 1 (0-based), char 26.
        assert_eq!(
            line_value_key(text, 1, 26).as_deref(),
            Some("has_completed_focus")
        );
        // Cursor on the key itself — no `=` before it.
        assert_eq!(line_value_key(text, 1, 10), None);
        // Comparison operators count too.
        let text2 = "block = {\n    num > \n}\n";
        assert_eq!(line_value_key(text2, 1, 10).as_deref(), Some("num"));
    }

    #[test]
    fn test_logical_path_from_uri_strips_workspace() {
        let ws = Some("file:///home/user/mod".to_string());
        let lp = logical_path_from_uri("file:///home/user/mod/events/foo.txt", &ws);
        assert_eq!(lp, "events/foo.txt");
    }

    #[test]
    fn test_logical_path_fallback() {
        let lp = logical_path_from_uri("file:///some/path/events/foo.txt", &None);
        assert_eq!(lp, "/some/path/events/foo.txt");
    }

    #[test]
    fn test_uri_to_path_percent_decode() {
        // Paths with spaces must round-trip through percent-encoding.
        let path = std::path::Path::new("/home/user/My Mod/events/foo.txt");
        let uri = path_to_uri(path);
        // The URI must percent-encode the space.
        assert!(
            uri.contains("%20") || uri.contains("+"),
            "expected encoded space in URI, got: {}",
            uri
        );
        // Decoding must recover the original path.
        let decoded = uri_to_path_str(&uri);
        assert_eq!(
            decoded,
            path.to_str().unwrap(),
            "round-trip failed: {}",
            decoded
        );
    }

    #[test]
    fn test_logical_path_from_uri_percent_decode() {
        // Workspace and file URIs with percent-encoded spaces.
        let ws = Some("file:///home/user/My%20Mod".to_string());
        let lp = logical_path_from_uri("file:///home/user/My%20Mod/events/foo.txt", &ws);
        assert_eq!(lp, "events/foo.txt", "got: {}", lp);
    }
}
