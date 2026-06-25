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
pub(crate) fn logical_path_from_uri(
    uri: &str,
    workspace_uri: &Option<std::sync::Arc<str>>,
) -> String {
    // Logical paths are `/`-separated everywhere downstream (type-instance
    // indexing, path matching). On Windows `uri_to_path_str` yields backslashes,
    // so normalise before stripping the workspace prefix — else the leading
    // separator survives `trim_start_matches('/')` and the path leaks into name
    // extraction (e.g. `load_oob` false positives).
    let path = normalize_separators(uri_to_path_str(uri));
    if let Some(ws) = workspace_uri {
        let ws_path = normalize_separators(uri_to_path_str(ws));
        // Strip leading slash-terminated prefix
        let prefix = ws_path.trim_end_matches('/');
        if let Some(rel) = path.strip_prefix(prefix) {
            return rel.trim_start_matches('/').to_string();
        }
    }
    // Fallback: use the decoded path as-is
    path
}

/// Normalise path separators to `/`. On the common (Linux/macOS) path the input
/// has no backslash, so the owned string is returned unchanged with no
/// allocation or scan-and-copy; only Windows paths actually pay the `replace`.
fn normalize_separators(path: String) -> String {
    if path.contains('\\') {
        path.replace('\\', "/")
    } else {
        path
    }
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

/// Whether a URI is a localisation file (`.yml` / `.yaml` / `.csv`), where
/// `$KEY$` references resolve to other loc entries rather than to game-script
/// rules. One predicate so hover/goto, completion, and validate agree on what
/// counts as loc (previously hover/goto only matched `.yml`, so loc resolution
/// silently skipped `.yaml`/`.csv` files that completion and validate handled).
pub(crate) fn is_loc_file(uri: &str) -> bool {
    let lower = uri.to_ascii_lowercase();
    lower.ends_with(".yml") || lower.ends_with(".yaml") || lower.ends_with(".csv")
}

/// Whether a URI is a `.cwt` rule-config file. These are the schema the rules
/// engine is built from, not game content, so they get their own structural
/// linting (undefined type/enum/single_alias refs + parse errors) rather than
/// the game-script validator. One predicate so validate/hover/completion/goto
/// all agree on what counts as a rule file.
pub(crate) fn is_cwt_file(uri: &str) -> bool {
    uri.to_ascii_lowercase().ends_with(".cwt")
}

/// Locate the `$KEY$` loc-reference token under the cursor in a localisation
/// line. `col` is the LSP (UTF-16) character offset. Returns the referenced key
/// plus the token's `[start, end)` range in UTF-16 columns (for the editor to
/// highlight). Mirrors the loc parser: the body must be an identifier
/// (`[A-Za-z0-9_.]`, optionally with a `|colour` suffix) or it's literal text
/// (a currency `$`), not a reference.
pub(crate) fn loc_ref_at_cursor(line: &str, col: u32) -> Option<(String, u32, u32)> {
    // Record every `$`'s (utf16 column, byte index).
    let mut dollars: Vec<(u32, usize)> = Vec::new();
    let mut u16col: u32 = 0;
    for (b, ch) in line.char_indices() {
        if ch == '$' {
            dollars.push((u16col, b));
        }
        u16col += ch.len_utf16() as u32;
    }
    // Pair consecutive dollars into `$…$` tokens. A non-identifier body (e.g.
    // `$5 today $`) is a stray currency `$`: skip just the opening one so the
    // next dollar can still open a real token.
    let mut i = 0;
    while i + 1 < dollars.len() {
        let (open_col, open_b) = dollars[i];
        let (close_col, close_b) = dollars[i + 1];
        let inner = &line[open_b + 1..close_b];
        let key = inner.split('|').next().unwrap_or(inner);
        if is_loc_ident(key) {
            let end_col = close_col + 1;
            if col >= open_col && col <= end_col {
                return Some((key.to_string(), open_col, end_col));
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    None
}

/// A `$…$` body that names a loc key / variable: non-empty, identifier chars only.
fn is_loc_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
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

/// Strip matching outer double quotes from a loc desc string for hover display.
/// `"Hello"` → `Hello`, `Hello` → `Hello`, `""` → `` (empty).
/// Strip an inline `#` comment from a loc desc string for hover display.
/// `"value" # comment` → `"value"`, `"value"` → `"value"`.
/// The `#` inside a quoted string is data, not a comment — only the LAST
/// unescaped `#` after the closing quote is stripped.
pub(crate) fn strip_loc_comment(s: &str) -> &str {
    // Find the last `"` in the string. If there is one, only strip `#` after it.
    if let Some(last_quote) = s.rfind('"') {
        let after = &s[last_quote + 1..];
        if let Some(hash) = after.find('#') {
            &s[..last_quote + 1 + hash]
        } else {
            s
        }
    } else {
        // No quotes at all — strip the first `#`.
        if let Some(hash) = s.find('#') {
            &s[..hash]
        } else {
            s
        }
    }
}

/// Extract the display text of a loc line for hover tooltips.
///
/// A loc value is a quoted string, optionally followed by a `# comment`. The
/// value runs from the first `"` to the LAST `"` on the line, so a `#` *inside*
/// the quotes is kept as data (issue #50) while a trailing `# comment` after the
/// closing quote is dropped. An unquoted value just has its inline `# comment`
/// stripped.
pub(crate) fn loc_display_text(desc: &str) -> &str {
    if let Some(rest) = desc.strip_prefix('"') {
        // Quoted: content runs to the last `"` on the line.
        if let Some(end) = rest.rfind('"') {
            return &rest[..end];
        }
        // Unterminated quote: best-effort, strip an inline comment from the rest.
        return strip_loc_comment(rest).trim_end();
    }
    // Unquoted value: drop an inline `# comment`.
    strip_loc_comment(desc).trim_end()
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
    fn is_loc_file_matches_all_loc_extensions() {
        // hover/goto, completion, and validate must agree (#2/#217).
        assert!(is_loc_file("file:///mod/localisation/foo_l_english.yml"));
        assert!(is_loc_file("file:///mod/localisation/foo_l_english.yaml"));
        assert!(is_loc_file("file:///mod/localisation/names.csv"));
        // case-insensitive (Windows)
        assert!(is_loc_file("file:///MOD/FOO.YML"));
        assert!(is_loc_file("file:///MOD/FOO.YAML"));
        // not loc
        assert!(!is_loc_file("file:///mod/common/ideas/foo.txt"));
        assert!(!is_loc_file("file:///mod/gfx/foo.gfx"));
    }

    #[test]
    fn is_cwt_file_matches_only_cwt() {
        assert!(is_cwt_file("file:///rules/Config/events.cwt"));
        assert!(is_cwt_file("file:///RULES/FOO.CWT")); // case-insensitive (Windows)
        assert!(!is_cwt_file("file:///mod/common/ideas/foo.txt"));
        assert!(!is_cwt_file("file:///mod/localisation/foo_l_english.yml"));
    }

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
    fn test_loc_ref_at_cursor() {
        //                0123456789012345678
        let line = "  k:0 \"a $FOO$ b\"";
        // `$FOO$` spans cols 9..14 (`$`=9, F=10..12, `$`=13).
        let (key, start, end) = loc_ref_at_cursor(line, 11).expect("cursor in $FOO$");
        assert_eq!(key, "FOO");
        assert_eq!((start, end), (9, 14));
        // Cursor outside any ref.
        assert!(loc_ref_at_cursor(line, 2).is_none());
    }

    #[test]
    fn test_loc_ref_at_cursor_colour_suffix() {
        let line = "x:0 \"$MY_KEY|Y$\"";
        let (key, _, _) = loc_ref_at_cursor(line, 8).expect("cursor in ref");
        assert_eq!(key, "MY_KEY", "colour suffix must be stripped from the key");
    }

    #[test]
    fn test_loc_ref_at_cursor_currency_not_a_ref() {
        // A stray currency `$` followed by a real ref: only the ref resolves.
        let line = "x:0 \"costs $5 for $ITEM$\"";
        assert!(
            loc_ref_at_cursor(line, 11).is_none(),
            "currency $5 must not be a ref"
        );
        let (key, _, _) = loc_ref_at_cursor(line, 20).expect("cursor in $ITEM$");
        assert_eq!(key, "ITEM");
    }

    #[test]
    fn test_logical_path_from_uri_strips_workspace() {
        let ws: Option<std::sync::Arc<str>> = Some("file:///home/user/mod".into());
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
        let ws: Option<std::sync::Arc<str>> = Some("file:///home/user/My%20Mod".into());
        let lp = logical_path_from_uri("file:///home/user/My%20Mod/events/foo.txt", &ws);
        assert_eq!(lp, "events/foo.txt", "got: {}", lp);
    }

    // ── strip_loc_comment (#50) ───────────────────────────────────────────

    #[test]
    fn strip_loc_comment_removes_inline_comment_after_quoted_value() {
        // `"value" # comment` → `"value" ` (space before # is kept)
        assert_eq!(strip_loc_comment(r#""value" # comment"#), r#""value" "#);
    }

    #[test]
    fn strip_loc_comment_preserves_quoted_value_without_comment() {
        assert_eq!(strip_loc_comment(r#""value""#), r#""value""#);
    }

    #[test]
    fn strip_loc_comment_keeps_hash_inside_quotes_as_data() {
        // The `#` inside a quoted string is data, not a comment.
        assert_eq!(
            strip_loc_comment(r#""value # not a comment""#),
            r#""value # not a comment""#
        );
    }

    #[test]
    fn strip_loc_comment_strips_first_hash_when_no_quotes() {
        assert_eq!(strip_loc_comment("value # comment"), "value ");
    }

    #[test]
    fn strip_loc_comment_preserves_unquoted_value_without_hash() {
        assert_eq!(strip_loc_comment("value"), "value");
    }

    #[test]
    fn strip_loc_comment_handles_empty_quoted_value_with_comment() {
        // Space before # is kept.
        assert_eq!(strip_loc_comment(r#""" # comment"#), r#""" "#);
    }

    #[test]
    fn strip_loc_comment_strips_only_first_hash_after_closing_quote() {
        // Only the first `#` after the closing quote is the comment start.
        // Space before the first # is kept.
        assert_eq!(
            strip_loc_comment(r#""value" # comment # more"#),
            r#""value" "#
        );
    }

    #[test]
    fn strip_loc_comment_handles_empty_string() {
        assert_eq!(strip_loc_comment(""), "");
    }

    #[test]
    fn strip_loc_comment_handles_only_comment() {
        assert_eq!(strip_loc_comment("# just a comment"), "");
    }

    // ── loc_display_text (#50) ────────────────────────────────────────────
    // Extracts the value a loc line should show in the hover tooltip: the
    // quoted string with outer quotes removed and any trailing `# comment`
    // dropped, while a `#` *inside* the quotes is preserved as data.

    #[test]
    fn loc_display_text_quoted_value_strips_outer_quotes() {
        assert_eq!(loc_display_text(r#""value""#), "value");
    }

    #[test]
    fn loc_display_text_drops_trailing_comment() {
        assert_eq!(loc_display_text(r#""value" # comment"#), "value");
    }

    #[test]
    fn loc_display_text_keeps_hash_inside_quotes() {
        // The reported bug: a `#` inside the quoted value is data, not a comment.
        assert_eq!(loc_display_text(r#""value # data""#), "value # data");
    }

    #[test]
    fn loc_display_text_keeps_inner_hash_and_drops_trailing_comment() {
        assert_eq!(
            loc_display_text(r#""value # data" # comment"#),
            "value # data"
        );
    }

    #[test]
    fn loc_display_text_unquoted_value_drops_comment() {
        assert_eq!(loc_display_text("value # comment"), "value");
    }

    #[test]
    fn loc_display_text_unquoted_value_without_comment() {
        assert_eq!(loc_display_text("value"), "value");
    }

    #[test]
    fn loc_display_text_empty_quoted_value() {
        assert_eq!(loc_display_text(r#""""#), "");
    }

    #[test]
    fn loc_display_text_empty_quoted_value_with_comment() {
        assert_eq!(loc_display_text(r#""" # comment"#), "");
    }
}
