//! The CI report formats: GitHub Actions workflow commands and SARIF 2.1.0.
//! The `cli`/`csv`/`json` renderers stay in `main.rs` next to the row helpers
//! they share.

use crate::Diag;
use crate::codes;
use cwtools_error_codes::ErrorCode;
use cwtools_validation::ErrorSeverity;
use std::path::{Path, PathBuf};

/// `--report-type`. The three original spellings render exactly as they did
/// before; `github` and `sarif` are the CI additions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReportType {
    Cli,
    Csv,
    Json,
    Github,
    Sarif,
}

impl ReportType {
    /// The spelling the flag uses, for the "Wrote … report to …" line.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ReportType::Cli => "cli",
            ReportType::Csv => "csv",
            ReportType::Json => "json",
            ReportType::Github => "github",
            ReportType::Sarif => "sarif",
        }
    }
}

/// Parse a `--report-type` value, for clap's `value_parser`. An unrecognised
/// format is an error: silently falling back to the text report would leave a
/// typo'd CI job publishing nothing.
pub(crate) fn parse_report_type(s: &str) -> Result<ReportType, String> {
    match s {
        "cli" => Ok(ReportType::Cli),
        "csv" => Ok(ReportType::Csv),
        "json" => Ok(ReportType::Json),
        "github" => Ok(ReportType::Github),
        "sarif" => Ok(ReportType::Sarif),
        _ => Err(format!(
            "invalid report type '{s}': valid values are cli, csv, json, github, sarif"
        )),
    }
}

/// The directory CI paths are reported relative to. Both formats are resolved
/// against the checkout root by the service consuming them, and a step with a
/// `working-directory:` doesn't move that root — so prefer `GITHUB_WORKSPACE`
/// when the runner set it, and fall back to the process CWD.
pub(crate) fn report_root() -> PathBuf {
    match std::env::var_os("GITHUB_WORKSPACE") {
        Some(w) if !w.is_empty() => PathBuf::from(w),
        _ => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    }
}

/// A diagnostic's file as a CI report should write it: relative to `base` when
/// it sits underneath (so an annotation lands on the PR diff), else the
/// absolute path. The bool says which one came back.
fn locate(file: &str, base: &Path) -> (String, bool) {
    let abs = std::path::absolute(file).unwrap_or_else(|_| PathBuf::from(file));
    match abs.strip_prefix(base) {
        Ok(rel) => (slashed(rel), true),
        Err(_) => (slashed(&abs), false),
    }
}

/// Forward slashes regardless of host: both formats are consumed by services
/// that treat `\` as a literal character, not a separator.
fn slashed(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

// ── GitHub Actions ───────────────────────────────────────────────────────────

fn github_level(s: ErrorSeverity) -> &'static str {
    match s {
        ErrorSeverity::Error => "error",
        ErrorSeverity::Warning => "warning",
        ErrorSeverity::Information | ErrorSeverity::Hint => "notice",
    }
}

/// Escape a workflow-command message. The runner percent-decodes `%25`/`%0D`/
/// `%0A`, so an unescaped newline would end the command and dump the rest of
/// the message into the log as plain text.
fn escape_data(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '%' => out.push_str("%25"),
            '\r' => out.push_str("%0D"),
            '\n' => out.push_str("%0A"),
            c => out.push(c),
        }
    }
    out
}

/// Escape a workflow-command property value. `:` and `,` separate the command's
/// own fields, so a file name containing either has to be encoded as well.
fn escape_property(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in escape_data(s).chars() {
        match c {
            ':' => out.push_str("%3A"),
            ',' => out.push_str("%2C"),
            c => out.push(c),
        }
    }
    out
}

/// One `::error file=…,line=…,col=…::message` line (trailing newline included).
/// Whole-file diagnostics carry line 0; they're clamped to 1 because an
/// annotation without a real line attaches to the run instead of the file.
pub(crate) fn github_row(d: &Diag, base: &Path) -> String {
    let (file, _) = locate(&d.file, base);
    let mut props = format!(
        "file={},line={},col={}",
        escape_property(&file),
        d.line.max(1),
        d.col.max(1)
    );
    if !d.code.is_empty() {
        props.push_str(",title=");
        props.push_str(&escape_property(d.code));
    }
    format!(
        "::{} {}::{}\n",
        github_level(d.severity),
        props,
        escape_data(&d.message)
    )
}

// ── SARIF 2.1.0 ──────────────────────────────────────────────────────────────

const SARIF_SCHEMA: &str = "https://raw.githubusercontent.com/oasis-tcs/sarif-spec/master/Schemata/sarif-schema-2.1.0.json";
const INFORMATION_URI: &str = "https://github.com/MillenniumDawn/cwtools";
const HELP_URI: &str =
    "https://github.com/MillenniumDawn/cwtools/blob/main/cwtools-rs/docs/ERROR_CODES.md";

fn sarif_level(s: ErrorSeverity) -> &'static str {
    match s {
        ErrorSeverity::Error => "error",
        ErrorSeverity::Warning => "warning",
        ErrorSeverity::Information | ErrorSeverity::Hint => "note",
    }
}

/// Percent-encode a path for a SARIF URI. Paradox installs live in paths like
/// `Hearts of Iron IV`, and a raw space is not a legal URI character.
/// `allow_colon` is for absolute URIs only: a bare `:` in the first segment of
/// a relative reference reads as a scheme.
fn uri_encode(path: &str, allow_colon: bool) -> String {
    let mut out = String::with_capacity(path.len());
    for b in path.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(b as char);
            }
            b':' if allow_colon => out.push(':'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn file_uri(path: &str) -> String {
    let encoded = uri_encode(path, true);
    if encoded.starts_with('/') {
        format!("file://{encoded}")
    } else {
        format!("file:///{encoded}")
    }
}

/// A JSON string literal, quotes included.
fn s(value: &str) -> String {
    format!("\"{}\"", crate::json_escape(value))
}

/// The SARIF 2.1.0 document for `diags` (trailing newline included). `base` is
/// the run's source root: locations under it are emitted relative to
/// `%SRCROOT%`, so the report resolves in any checkout of the same tree.
///
/// `tool.driver.rules` is generated from the shared error-code catalog, and
/// carries only the codes this run actually reported so every `ruleIndex`
/// resolves.
pub(crate) fn sarif_report(diags: &[&Diag], base: &Path) -> String {
    // Carry the catalog entry itself, not just the id: every rule in the array
    // then has a definition by construction, so the comma accounting can't be
    // thrown off by a lookup that fails on the second pass.
    let mut rules: Vec<&'static (&'static str, ErrorCode)> =
        diags.iter().filter_map(|d| codes::entry(d.code)).collect();
    rules.sort_unstable_by_key(|(_, c)| c.id);
    rules.dedup_by_key(|(_, c)| c.id);

    // id -> position in `rules`, built once: the per-result lookup below is
    // otherwise a linear scan of the rule array for every diagnostic.
    let rule_index: std::collections::HashMap<String, usize> = rules
        .iter()
        .enumerate()
        .map(|(i, (_, c))| (c.id.to_ascii_lowercase(), i))
        .collect();

    let mut out = String::new();
    out.push_str("{\n");
    out.push_str(&format!("  \"$schema\": {},\n", s(SARIF_SCHEMA)));
    out.push_str("  \"version\": \"2.1.0\",\n");
    out.push_str("  \"runs\": [\n");
    out.push_str("    {\n");
    out.push_str("      \"tool\": {\n");
    out.push_str("        \"driver\": {\n");
    out.push_str("          \"name\": \"cwtools\",\n");
    out.push_str(&format!(
        "          \"version\": {},\n",
        s(env!("CARGO_PKG_VERSION"))
    ));
    out.push_str(&format!(
        "          \"informationUri\": {},\n",
        s(INFORMATION_URI)
    ));
    out.push_str("          \"rules\": [\n");
    for (i, rule) in rules.iter().enumerate() {
        out.push_str(&sarif_rule(rule, i + 1 == rules.len()));
    }
    out.push_str("          ]\n");
    out.push_str("        }\n");
    out.push_str("      },\n");
    out.push_str("      \"originalUriBaseIds\": {\n");
    // A base URI must end in exactly one '/'. Trimming them all would turn the
    // filesystem root's `file:///` into `file:/`.
    let root = file_uri(&slashed(base));
    let root = match root.strip_suffix('/') {
        Some(_) => root,
        None => format!("{root}/"),
    };
    out.push_str(&format!(
        "        \"SRCROOT\": {{ \"uri\": {} }}\n",
        s(&root)
    ));
    out.push_str("      },\n");
    // Parser columns are character counts, not UTF-16 code units.
    out.push_str("      \"columnKind\": \"unicodeCodePoints\",\n");
    out.push_str("      \"results\": [\n");
    for (i, d) in diags.iter().enumerate() {
        out.push_str(&sarif_result(d, base, &rule_index, i + 1 == diags.len()));
    }
    out.push_str("      ]\n");
    out.push_str("    }\n");
    out.push_str("  ]\n");
    out.push_str("}\n");
    out
}

fn sarif_rule((const_name, code): &(&str, ErrorCode), last: bool) -> String {
    // The template's `{}` are substitution points, not text a reader wants.
    // A few entries are nothing but a pass-through `{}` (the message is built at
    // the emit site), leaving the const's name as the only description.
    let description = code.message_template.replace("{}", "…");
    let description = if description.trim_matches(['…', ' ']).is_empty() {
        codes::rule_summary(const_name)
    } else {
        description
    };
    format!(
        "            {{\n\
         \x20             \"id\": {},\n\
         \x20             \"name\": {},\n\
         \x20             \"shortDescription\": {{ \"text\": {} }},\n\
         \x20             \"defaultConfiguration\": {{ \"level\": {} }},\n\
         \x20             \"helpUri\": {}\n\
         \x20           }}{}\n",
        s(code.id),
        s(&codes::rule_name(const_name)),
        s(&description),
        s(sarif_level(code.severity)),
        s(HELP_URI),
        if last { "" } else { "," }
    )
}

fn sarif_result(
    d: &Diag,
    base: &Path,
    rule_index: &std::collections::HashMap<String, usize>,
    last: bool,
) -> String {
    let (path, relative) = locate(&d.file, base);
    let uri = if relative {
        s(&uri_encode(&path, false))
    } else {
        s(&file_uri(&path))
    };
    let base_id = if relative {
        ", \"uriBaseId\": \"SRCROOT\""
    } else {
        ""
    };

    let mut out = String::from("        {\n");
    if !d.code.is_empty() {
        out.push_str(&format!("          \"ruleId\": {},\n", s(d.code)));
        // Case-insensitively, to match how the rules were collected.
        if let Some(idx) = rule_index.get(&d.code.to_ascii_lowercase()) {
            out.push_str(&format!("          \"ruleIndex\": {idx},\n"));
        }
    }
    out.push_str(&format!(
        "          \"level\": {},\n",
        s(sarif_level(d.severity))
    ));
    out.push_str(&format!(
        "          \"message\": {{ \"text\": {} }},\n",
        s(&d.message)
    ));
    out.push_str("          \"locations\": [\n");
    out.push_str("            {\n");
    out.push_str("              \"physicalLocation\": {\n");
    out.push_str(&format!(
        "                \"artifactLocation\": {{ \"uri\": {uri}{base_id} }},\n"
    ));
    out.push_str(&format!(
        "                \"region\": {{ \"startLine\": {}, \"startColumn\": {} }}\n",
        d.line.max(1),
        d.col.max(1)
    ));
    out.push_str("              }\n");
    out.push_str("            }\n");
    out.push_str("          ],\n");
    out.push_str(&format!(
        "          \"partialFingerprints\": {{ \"cwtoolsDiagHash/v1\": {} }}\n",
        s(&d.hash)
    ));
    out.push_str(if last { "        }\n" } else { "        },\n" });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diag(file: &str, line: u32, col: u32, code: &'static str, message: &str) -> Diag {
        Diag {
            file: file.into(),
            severity: ErrorSeverity::Error,
            code,
            message: message.to_string(),
            line,
            col,
            hash: "0123456789abcdef".to_string(),
            legacy_hash: "fedcba9876543210".to_string(),
        }
    }

    /// An absolute path spelled the host's way. On Windows a leading `/` names
    /// the current drive's root, so a base and a file that both start with one
    /// still relate to each other, but neither renders without the drive.
    fn abs(tail: &str) -> String {
        if cfg!(windows) {
            format!("C:/{tail}")
        } else {
            format!("/{tail}")
        }
    }

    /// `file_uri` of an `abs` path, up to and including the first slash.
    const URI: &str = if cfg!(windows) {
        "file:///C:/"
    } else {
        "file:///"
    };

    #[test]
    fn report_type_round_trips_its_spelling() {
        for name in ["cli", "csv", "json", "github", "sarif"] {
            assert_eq!(parse_report_type(name).unwrap().as_str(), name);
        }
    }

    #[test]
    fn unknown_report_type_is_rejected() {
        let e = parse_report_type("sarrif").unwrap_err();
        assert!(e.contains("sarrif") && e.contains("sarif"), "got: {e}");
    }

    #[test]
    fn github_row_renders_the_workflow_command() {
        let base = PathBuf::from(abs("repo"));
        let d = diag(
            &abs("repo/common/x.txt"),
            12,
            5,
            "CW282",
            "redundant default",
        );
        assert_eq!(
            github_row(&d, &base),
            "::error file=common/x.txt,line=12,col=5,title=CW282::redundant default\n"
        );
    }

    #[test]
    fn github_row_maps_severity_to_the_three_annotation_levels() {
        let base = Path::new("/repo");
        let mut d = diag("/repo/x.txt", 1, 1, "CW100", "m");
        for (sev, want) in [
            (ErrorSeverity::Error, "::error "),
            (ErrorSeverity::Warning, "::warning "),
            (ErrorSeverity::Information, "::notice "),
            (ErrorSeverity::Hint, "::notice "),
        ] {
            d.severity = sev;
            assert!(github_row(&d, base).starts_with(want), "{sev:?}");
        }
    }

    /// A raw newline would terminate the command and swallow the rest.
    #[test]
    fn github_row_encodes_newlines_and_percent_in_the_message() {
        let base = Path::new("/repo");
        let d = diag("/repo/x.txt", 1, 1, "CW100", "one\r\ntwo 50% off");
        let row = github_row(&d, base);
        assert!(row.contains("::one%0D%0Atwo 50%25 off\n"), "got: {row}");
        assert_eq!(row.matches('\n').count(), 1, "one physical line: {row}");
    }

    #[test]
    fn github_row_encodes_separators_in_the_file_property() {
        let base = PathBuf::from(abs("repo"));
        let d = diag(&abs("repo/od,d:name.txt"), 1, 1, "", "m");
        let row = github_row(&d, &base);
        assert!(row.contains("file=od%2Cd%3Aname.txt,line=1"), "got: {row}");
        assert!(!row.contains("title="), "no code, no title: {row}");
    }

    /// Whole-file diagnostics report line 0, which GitHub can't anchor.
    #[test]
    fn github_row_clamps_line_zero() {
        let base = Path::new("/repo");
        let d = diag("/repo/x.yml", 0, 0, "", "bad file");
        assert!(github_row(&d, base).contains("line=1,col=1"));
    }

    #[test]
    fn paths_outside_the_root_stay_absolute() {
        let base = PathBuf::from(abs("repo"));
        let file = abs("elsewhere/x.txt");
        let d = diag(&file, 1, 1, "CW100", "m");
        let row = github_row(&d, &base);
        assert!(
            row.contains(&format!("file={},line=", escape_property(&file))),
            "got: {row}"
        );
    }

    #[test]
    fn sarif_has_the_2_1_0_envelope() {
        let out = sarif_report(&[], Path::new("/repo"));
        assert!(out.contains("\"version\": \"2.1.0\""));
        assert!(out.contains("sarif-schema-2.1.0.json"));
        assert!(out.contains("\"name\": \"cwtools\""));
        assert!(out.contains(&format!("\"version\": \"{}\"", env!("CARGO_PKG_VERSION"))));
        assert!(out.contains("\"rules\": [\n          ]"), "got: {out}");
        assert!(out.contains("\"results\": [\n      ]"), "got: {out}");
    }

    #[test]
    fn sarif_rules_come_from_the_error_code_catalog() {
        let d = diag("/repo/x.txt", 3, 2, "CW113", "missing file");
        let out = sarif_report(&[&d], Path::new("/repo"));
        assert!(out.contains("\"id\": \"CW113\""), "got: {out}");
        assert!(out.contains("\"name\": \"MissingFile\""), "got: {out}");
        // shortDescription is the catalog's message template.
        assert!(out.contains("\"shortDescription\""), "got: {out}");
        assert!(out.contains("\"defaultConfiguration\""), "got: {out}");
        assert!(out.contains("\"ruleIndex\": 0"), "got: {out}");
    }

    /// Pass-through templates ("{}") would render as a useless description.
    #[test]
    fn sarif_describes_pass_through_templates_by_name() {
        let d = diag("/repo/x.txt", 1, 1, "CW240", "value is wrong");
        let out = sarif_report(&[&d], Path::new("/repo"));
        assert!(out.contains("\"text\": \"Unexpected value\""), "got: {out}");
    }

    #[test]
    fn sarif_rule_indexes_are_sorted_and_shared() {
        let ds = [
            diag("/repo/a.txt", 1, 1, "CW282", "b"),
            diag("/repo/b.txt", 2, 1, "CW113", "a"),
            diag("/repo/c.txt", 3, 1, "CW282", "c"),
        ];
        let out = sarif_report(&ds.iter().collect::<Vec<_>>(), Path::new("/repo"));
        // CW113 sorts first, so it is rule 0 and CW282 is rule 1.
        assert_eq!(out.matches("\"ruleIndex\": 0").count(), 1);
        assert_eq!(out.matches("\"ruleIndex\": 1").count(), 2);
        assert_eq!(out.matches("\"id\": \"CW113\"").count(), 1);
    }

    /// The catalog resolves a code case-insensitively, so a diagnostic carrying
    /// a non-canonical spelling still contributes a rule — and must still find
    /// its index. Keying the lookup on the raw spelling would drop the
    /// `ruleIndex` for exactly these rows.
    #[test]
    fn sarif_rule_index_resolves_a_non_canonical_code_spelling() {
        let d = diag("/repo/x.txt", 3, 2, "cw113", "missing file");
        let out = sarif_report(&[&d], Path::new("/repo"));
        assert!(out.contains("\"id\": \"CW113\""), "got: {out}");
        assert!(out.contains("\"ruleIndex\": 0"), "got: {out}");
    }

    #[test]
    fn sarif_locations_are_relative_to_the_source_root() {
        let d = diag(&abs("repo/common/x.txt"), 7, 3, "CW100", "m");
        let out = sarif_report(&[&d], Path::new(&abs("repo")));
        assert!(
            out.contains("\"uri\": \"common/x.txt\", \"uriBaseId\": \"SRCROOT\""),
            "got: {out}"
        );
        assert!(
            out.contains(&format!("\"SRCROOT\": {{ \"uri\": \"{URI}repo/\" }}")),
            "got: {out}"
        );
        assert!(
            out.contains("\"startLine\": 7, \"startColumn\": 3"),
            "got: {out}"
        );
    }

    /// A base URI keeps exactly one trailing slash; the filesystem root is the
    /// case where trimming them all leaves the scheme mangled as `file:/`.
    #[test]
    fn sarif_root_uri_survives_the_filesystem_root() {
        let out = sarif_report(&[], Path::new("/"));
        assert!(
            out.contains("\"SRCROOT\": { \"uri\": \"file:///\" }"),
            "got: {out}"
        );
    }

    /// `{}` are substitution points, not text a reader wants in a description.
    #[test]
    fn sarif_description_replaces_template_placeholders() {
        let d = diag("/repo/x.txt", 1, 1, "CW100", "m");
        let out = sarif_report(&[&d], Path::new("/repo"));
        assert!(
            out.contains("\"text\": \"Localisation key … is not defined for …\""),
            "got: {out}"
        );
        assert!(!out.contains("{}"), "no raw placeholders: {out}");
    }

    #[test]
    fn sarif_encodes_spaces_in_uris() {
        let d = diag(&abs("games/Hearts of Iron IV/x.txt"), 1, 1, "CW100", "m");
        let out = sarif_report(&[&d], Path::new(&abs("repo")));
        assert!(
            out.contains(&format!(
                "\"uri\": \"{URI}games/Hearts%20of%20Iron%20IV/x.txt\""
            )),
            "got: {out}"
        );
        assert!(!out.contains("uriBaseId"), "outside the root: {out}");
    }

    #[test]
    fn sarif_omits_the_rule_id_when_a_diagnostic_has_no_code() {
        let d = diag("/repo/x.yml", 0, 0, "", "could not parse");
        let out = sarif_report(&[&d], Path::new("/repo"));
        assert!(!out.contains("ruleId"), "got: {out}");
        assert!(
            out.contains("\"startLine\": 1, \"startColumn\": 1"),
            "got: {out}"
        );
    }

    #[test]
    fn sarif_carries_the_diagnostic_hash_as_a_fingerprint() {
        let d = diag("/repo/x.txt", 1, 1, "CW100", "m");
        let out = sarif_report(&[&d], Path::new("/repo"));
        assert!(
            out.contains("\"cwtoolsDiagHash/v1\": \"0123456789abcdef\""),
            "got: {out}"
        );
    }

    #[test]
    fn sarif_escapes_json_in_messages() {
        let d = diag("/repo/x.txt", 1, 1, "CW100", "he said \"no\"\nthen left");
        let out = sarif_report(&[&d], Path::new("/repo"));
        assert!(
            out.contains(r#""text": "he said \"no\"\nthen left""#),
            "got: {out}"
        );
    }
}
