//! Project-level loc-file validation.
//!
//! Runs the scope-independent loc-entry checks (`validate_loc_file`) over every
//! loaded loc file and normalizes the results to the F# numeric error codes
//! (CW001/CW225/CW234/CW259/CW268/CW275/CW276), plus the per-file name/header checks
//! (CW254/CW255/CW256/CW257). Scope-dependent command checks
//! (CW226/CW260/CW266) run at the config reference site, not here, because they
//! need the scope of the referencing field.

use crate::commands::{Game, Lang, LocFile};
use crate::service::LocService;
use crate::validation::{LocErrorKind, hardcoded_loc_set, validate_loc_file_with_hardcoded};
use crate::yaml_parser::{LangHeaderDiagnostic, check_loc_file_lang, parse_loc_text};
use std::collections::HashSet;

/// Severity of a loc diagnostic. Mirrors the validation crate's `ErrorSeverity`
/// without taking a dependency on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocSeverity {
    Error,
    Warning,
    Information,
}

/// A normalized loc diagnostic ready to be surfaced as a `ValidationError` or an
/// LSP `Diagnostic`. `line`/`col` are 1-based.
#[derive(Debug, Clone, PartialEq)]
pub struct LocDiagnostic {
    pub file: String,
    pub line: usize,
    pub col: usize,
    pub code: &'static str,
    pub severity: LocSeverity,
    pub message: String,
}

/// Single source of truth for a scope-independent loc-entry error's code and
/// severity (matching the F# `ErrorCodes` mapping). Splitting this from the
/// message means the code / severity accessors don't build (and discard) a
/// formatted `String`, and the emission path formats each message exactly once.
fn loc_error_code_severity(kind: &LocErrorKind) -> (&'static str, LocSeverity) {
    match kind {
        LocErrorKind::UndefinedLocReference { .. } => ("CW225", LocSeverity::Error),
        LocErrorKind::RecursiveLocRef => ("CW259", LocSeverity::Error),
        LocErrorKind::ReplaceMe => ("CW234", LocSeverity::Information),
        LocErrorKind::LocMissingQuote => ("CW268", LocSeverity::Warning),
        LocErrorKind::LocInvalidChars => ("CW275", LocSeverity::Warning),
        LocErrorKind::LocKeyInvalidChars => ("CW276", LocSeverity::Warning),
    }
}

/// Code, severity, and human-readable message for a loc-entry error, built in one
/// pass so the emission path (`build_diagnostics`) formats the message once.
fn loc_error_parts(
    kind: &LocErrorKind,
    key: &str,
    lang: Option<Lang>,
) -> (&'static str, LocSeverity, String) {
    let (code, severity) = loc_error_code_severity(kind);
    (code, severity, loc_error_message(kind, key, lang))
}

/// The F# numeric code for a scope-independent loc-entry error.
pub fn loc_error_code(kind: &LocErrorKind) -> &'static str {
    loc_error_code_severity(kind).0
}

/// The severity for a scope-independent loc-entry error.
pub fn loc_error_severity(kind: &LocErrorKind) -> LocSeverity {
    loc_error_code_severity(kind).1
}

/// Build the human-readable message, matching the F# `ErrorCodes` text.
fn loc_error_message(kind: &LocErrorKind, key: &str, lang: Option<Lang>) -> String {
    let lang_label = lang
        .map(|l| l.to_string())
        .unwrap_or_else(|| "?".to_string());
    match kind {
        LocErrorKind::UndefinedLocReference { other_key } => format!(
            "Localisation key \"{}\" references \"{}\" which doesn't exist in {}",
            key, other_key, lang_label
        ),
        LocErrorKind::RecursiveLocRef => "This localisation string refers to itself".to_string(),
        LocErrorKind::ReplaceMe => {
            format!(
                "Localisation key {} is a placeholder for {}",
                key, lang_label
            )
        }
        LocErrorKind::LocMissingQuote => format!(
            "Localisation key {} doesn't start and end with double quotes",
            key
        ),
        LocErrorKind::LocInvalidChars => format!(
            "Localisation value for {} contains unexpected characters, and may not render correctly",
            key
        ),
        LocErrorKind::LocKeyInvalidChars => format!(
            "Localisation key {} contains invalid characters (spaces or special characters are not allowed)",
            key
        ),
    }
}

/// F# `STLLang` case name, used to reproduce the CW257 message (`%A`).
fn lang_fsharp_name(lang: Lang) -> &'static str {
    match lang {
        Lang::English => "English",
        Lang::French => "French",
        Lang::German => "German",
        Lang::Spanish => "Spanish",
        Lang::Russian => "Russian",
        Lang::Polish => "Polish",
        Lang::BrazPor => "Braz_Por",
        Lang::SimpChinese => "Chinese",
        Lang::Japanese => "Japanese",
        Lang::Korean => "Korean",
        Lang::Turkish => "Turkish",
        Lang::Default => "Default",
    }
}

/// Per-file name/header language check (CW255/CW256/CW257).
///
/// Mirrors F# `STLLocalisationString.checkLocFileName`: a loc file's name must
/// carry a recognised `l_xxx` tag, the first line must be a recognised
/// `l_xxx:` header, and the two must agree.
fn lang_header_diagnostic(file: &LocFile) -> Option<LocDiagnostic> {
    let (code, severity, message): (&'static str, LocSeverity, String) = match check_loc_file_lang(
        &file.path,
        &file.language_prefix,
    )? {
        LangHeaderDiagnostic::MissingLocFileLangHeader { .. } => (
            cwtools_error_codes::CW256_MISSING_LOC_FILE_LANG_HEADER.id,
            LocSeverity::Error,
            "Localisation file should start with \"l_language:\" on the first line (or a comment)"
                .to_string(),
        ),
        LangHeaderDiagnostic::MissingLocFileLang { .. } => (
            cwtools_error_codes::CW255_MISSING_LOC_FILE_LANG.id,
            LocSeverity::Error,
            "Localisation file name should contain (and ideally end with) \"l_language.yml\""
                .to_string(),
        ),
        LangHeaderDiagnostic::LocFileLangMismatch {
            filename_lang,
            header_lang,
            ..
        } => (
            cwtools_error_codes::CW257_LOC_FILE_LANG_MISMATCH.id,
            LocSeverity::Error,
            format!(
                "Localisation file's name has language {} doesn't match the header language {}",
                lang_fsharp_name(filename_lang),
                lang_fsharp_name(header_lang)
            ),
        ),
    };
    Some(LocDiagnostic {
        file: file.path.clone(),
        line: 1,
        col: 1,
        code,
        severity,
        message,
    })
}

/// Build the per-file diagnostics for one parsed loc file, in the fixed F# order:
/// CW255/256/257 (lang header) → CW254 (encoding) → CW001 (parse errors) →
/// CW225/234/259/268/275 (loc-entry checks).
///
/// `file_path` is the path used for every diagnostic's `file` field (the project
/// path passes `&file.path`; the single-file path passes its `path` argument —
/// both are the same string the file was parsed under).
///
/// `emit_cw254` controls the one DELIBERATE divergence between the two callers:
/// the project (directory-loading) path knows the on-disk encoding and passes
/// `true` only when the file is `Utf8NoBom`/`NonUtf8`; the single-file text path
/// has no on-disk bytes to inspect and always passes `false`, so it never emits
/// CW254. Do not flip this without changing the corpus.
fn build_diagnostics(
    file: &LocFile,
    file_path: &str,
    union: &HashSet<String>,
    extra_valid_refs: &HashSet<String>,
    hardcoded: &HashSet<String>,
    emit_cw254: bool,
) -> Vec<LocDiagnostic> {
    let lang = file.lang;
    let mut out: Vec<LocDiagnostic> = Vec::new();

    // CW255/256/257: file name vs language header.
    if let Some(d) = lang_header_diagnostic(file) {
        out.push(d);
    }

    // CW254: localisation files must be UTF-8 with BOM. Only enforced when the
    // on-disk encoding is known (the directory-loading path); the caller has
    // already resolved that condition into `emit_cw254`.
    if emit_cw254 {
        out.push(LocDiagnostic {
            file: file_path.to_string(),
            line: 1,
            col: 1,
            code: cwtools_error_codes::CW254_WRONG_ENCODING.id,
            severity: LocSeverity::Error,
            message: "Localisation files must be UTF-8 BOM, this file is not".to_string(),
        });
    }

    // CW001: line-level parse errors collected during lenient recovery.
    for pe in &file.parse_errors {
        out.push(LocDiagnostic {
            file: file_path.to_string(),
            line: pe.line,
            col: 1,
            code: cwtools_error_codes::CW001_PARSE_ERROR.id,
            severity: LocSeverity::Error,
            message: cwtools_error_codes::CW001_PARSE_ERROR.format(&[pe.message.as_str()]),
        });
    }

    for err in validate_loc_file_with_hardcoded(file, union, extra_valid_refs, hardcoded) {
        let (code, severity, message) = loc_error_parts(&err.kind, &err.key, lang);
        out.push(LocDiagnostic {
            file: file_path.to_string(),
            line: err.line,
            col: err.col,
            code,
            severity,
            message,
        });
    }
    out
}

/// Whether CW254 (wrong encoding) should fire for a file, given its detected
/// on-disk encoding. Only the directory-loading path populates `encoding`; the
/// text-only path leaves it `None`, which is correctly treated as "don't fire".
fn should_emit_cw254(file: &LocFile) -> bool {
    matches!(
        file.encoding,
        Some(cwtools_file_manager::FileEncoding::Utf8NoBom)
            | Some(cwtools_file_manager::FileEncoding::NonUtf8)
    )
}

/// Validate every loaded loc file and return normalized diagnostics.
pub fn validate_loc_project(service: &LocService, game: Game) -> Vec<LocDiagnostic> {
    validate_loc_project_scoped(service, game, None, &HashSet::new())
}

/// As [`validate_loc_project`], but only emit per-file diagnostics for files
/// whose language is in `langs` (when `Some`). Files with no detectable language
/// are always validated (they may be malformed). Every file still contributes to
/// the key `union`, so `$ref$` existence resolves against all loaded languages.
/// `langs = None` validates every file (the previous behavior).
///
/// `extra_valid_refs` are additional lowercased names a `$ref$` may resolve to
/// besides loc keys — game-definition registries the engine resolves in loc
/// context (modifiers, ideas). A ref matching one of these is treated as
/// defined, suppressing CW225. Pass `&HashSet::new()` for none.
pub fn validate_loc_project_scoped(
    service: &LocService,
    game: Game,
    langs: Option<&[Lang]>,
    extra_valid_refs: &HashSet<String>,
) -> Vec<LocDiagnostic> {
    use rayon::prelude::*;

    // Union of keys across all languages, to resolve `$ref$` existence.
    // Borrowed from the service's single owned copy — no second copy of any loc
    // file is ever materialized (a full clone OOMs on large projects like MD).
    // Built in parallel: on large projects (~2M entries) the sequential
    // lowercase+insert dominated. Same case-folding (`to_lowercase`) as before;
    // the resulting set is identical regardless of insert order.
    let union: HashSet<String> = service
        .files()
        .par_iter()
        .flat_map_iter(|file| file.entries.iter().map(|e| e.key.to_lowercase()))
        .collect();
    validate_loc_project_with_union(service, game, langs, &union, extra_valid_refs)
}

/// As [`validate_loc_project_scoped`], but reuses a caller-owned key `union`
/// instead of rebuilding it. The [`crate::LocIndex`] already holds the lowercased
/// union (with any merged vanilla-cache keys); passing it by reference avoids a
/// third full materialization of the ~2M-key universe per run. When the union
/// carries cached vanilla keys it's a superset, but a `$ref$` found in it only
/// triggers the recursion check on a self-reference (the entry's own key, always
/// present regardless), so the emitted diagnostics are unchanged.
pub fn validate_loc_project_with_union(
    service: &LocService,
    _game: Game,
    langs: Option<&[Lang]>,
    union: &HashSet<String>,
    extra_valid_refs: &HashSet<String>,
) -> Vec<LocDiagnostic> {
    use rayon::prelude::*;

    // Each file validates independently against the read-only key union, so the
    // per-file pass runs in parallel. `par_iter` over the indexed `files` slice
    // collects in input order — output matches the sequential version.
    // Lowercased hardcoded-loc set, built once and shared read-only across the
    // per-file parallel pass (was re-lowercased + re-collected per file).
    let hardcoded = hardcoded_loc_set();
    service
        .files()
        .par_iter()
        .filter(|file| match langs {
            // None language can't be scoped out — keep validating it.
            Some(set) => file.lang.map(|l| set.contains(&l)).unwrap_or(true),
            None => true,
        })
        .flat_map_iter(|file| {
            // Directory-loading path: CW254 fires when the detected on-disk
            // encoding is missing/wrong BOM.
            build_diagnostics(
                file,
                &file.path,
                union,
                extra_valid_refs,
                hardcoded,
                should_emit_cw254(file),
            )
            .into_iter()
        })
        .collect()
}

/// Validate a single loc file's text against a precomputed key union. Used by
/// the LSP to lint a `.yml`/`.csv` file on open/change without rebuilding the
/// whole service. Returns an empty vec if the text can't be parsed as loc.
pub fn validate_loc_file_text(
    text: &str,
    path: &str,
    union: &HashSet<String>,
    extra_valid_refs: &HashSet<String>,
) -> Vec<LocDiagnostic> {
    let Ok(file) = parse_loc_text(text, path) else {
        return Vec::new();
    };
    // Text-only path: no on-disk bytes to inspect, so CW254 never fires here.
    // This is the deliberate divergence from the project path — do not change it.
    build_diagnostics(
        &file,
        path,
        union,
        extra_valid_refs,
        hardcoded_loc_set(),
        false,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use cwtools_file_manager::FileEncoding;

    fn service_from(files: &[(&str, &str)]) -> LocService {
        LocService::from_files(
            files
                .iter()
                .map(|(p, t)| (p.to_string(), t.to_string()))
                .collect(),
        )
    }

    #[test]
    fn undefined_ref_maps_to_cw225() {
        let svc = service_from(&[(
            "a_l_english.yml",
            "l_english:\n key1: \"Hello $undefined_key$\"\n",
        )]);
        let diags = validate_loc_project(&svc, Game::HOI4);
        let cw225: Vec<_> = diags.iter().filter(|d| d.code == "CW225").collect();
        assert_eq!(cw225.len(), 1, "got: {:?}", diags);
        assert_eq!(cw225[0].severity, LocSeverity::Error);
        assert!(cw225[0].message.contains("english"));
    }

    #[test]
    fn filename_without_lang_maps_to_cw255() {
        // Valid header, but the file name carries no l_xxx tag.
        let svc = service_from(&[("events.yml", "l_english:\n key1: \"hi\"\n")]);
        let diags = validate_loc_project(&svc, Game::HOI4);
        let cw255: Vec<_> = diags.iter().filter(|d| d.code == "CW255").collect();
        assert_eq!(cw255.len(), 1, "got: {:?}", diags);
        assert_eq!(cw255[0].severity, LocSeverity::Error);
    }

    #[test]
    fn unrecognised_header_maps_to_cw256() {
        // File name has a lang tag, but the header language is unknown.
        let svc = service_from(&[("events_l_english.yml", "l_klingon:\n key1: \"hi\"\n")]);
        let diags = validate_loc_project(&svc, Game::HOI4);
        let cw256: Vec<_> = diags.iter().filter(|d| d.code == "CW256").collect();
        assert_eq!(cw256.len(), 1, "got: {:?}", diags);
        assert_eq!(cw256[0].severity, LocSeverity::Error);
    }

    #[test]
    fn name_header_mismatch_maps_to_cw257() {
        // File name says english, header says french.
        let svc = service_from(&[("events_l_english.yml", "l_french:\n key1: \"hi\"\n")]);
        let diags = validate_loc_project(&svc, Game::HOI4);
        let cw257: Vec<_> = diags.iter().filter(|d| d.code == "CW257").collect();
        assert_eq!(cw257.len(), 1, "got: {:?}", diags);
        assert!(
            cw257[0].message.contains("English") && cw257[0].message.contains("French"),
            "message: {}",
            cw257[0].message
        );
    }

    #[test]
    fn matching_name_and_header_no_lang_diag() {
        let svc = service_from(&[("events_l_english.yml", "l_english:\n key1: \"hi\"\n")]);
        let diags = validate_loc_project(&svc, Game::HOI4);
        assert!(
            diags
                .iter()
                .all(|d| !matches!(d.code, "CW255" | "CW256" | "CW257")),
            "got: {:?}",
            diags
        );
    }

    #[test]
    fn replace_me_maps_to_cw234_info() {
        let svc = service_from(&[("a_l_english.yml", "l_english:\n key1: \"REPLACE_ME\"\n")]);
        let diags = validate_loc_project(&svc, Game::HOI4);
        let cw234: Vec<_> = diags.iter().filter(|d| d.code == "CW234").collect();
        assert_eq!(cw234.len(), 1, "got: {:?}", diags);
        assert_eq!(cw234[0].severity, LocSeverity::Information);
    }

    #[test]
    fn invalid_chars_message_attributes_problem_to_value() {
        // The offending characters live in the loc VALUE, not the key; the message
        // must say so (CW275). A zero-width space (U+200B) is genuine invisible junk
        // that stays flagged even after the allow-list is widened for real scripts.
        let svc = service_from(&[(
            "a_l_english.yml",
            "l_english:\n bad_loc_entry: \"hello\u{200b}world\"\n",
        )]);
        let diags = validate_loc_project(&svc, Game::HOI4);
        let cw275: Vec<_> = diags.iter().filter(|d| d.code == "CW275").collect();
        assert_eq!(cw275.len(), 1, "got: {:?}", diags);
        let msg = &cw275[0].message;
        assert!(
            msg.contains("value") && msg.contains("bad_loc_entry"),
            "CW275 should attribute the bad characters to the value of the entry, got: {msg}"
        );
    }

    fn service_with_encoding(files: &[(&str, &str, Option<FileEncoding>)]) -> LocService {
        LocService::from_files_with_encoding(
            files
                .iter()
                .map(|(p, t, e)| (p.to_string(), t.to_string(), *e))
                .collect(),
        )
    }

    #[test]
    fn no_bom_maps_to_cw254_error() {
        let svc = service_with_encoding(&[(
            "a_l_english.yml",
            "l_english:\n key1: \"hi\"\n",
            Some(FileEncoding::Utf8NoBom),
        )]);
        let cw254: Vec<_> = validate_loc_project(&svc, Game::HOI4)
            .into_iter()
            .filter(|d| d.code == "CW254")
            .collect();
        assert_eq!(cw254.len(), 1, "missing-BOM file should warn CW254");
        assert_eq!(cw254[0].severity, LocSeverity::Error);
    }

    #[test]
    fn non_utf8_maps_to_cw254_error() {
        let svc = service_with_encoding(&[(
            "a_l_english.yml",
            "l_english:\n key1: \"hi\"\n",
            Some(FileEncoding::NonUtf8),
        )]);
        assert_eq!(
            validate_loc_project(&svc, Game::HOI4)
                .iter()
                .filter(|d| d.code == "CW254")
                .count(),
            1
        );
    }

    #[test]
    fn bom_present_no_cw254() {
        let svc = service_with_encoding(&[(
            "a_l_english.yml",
            "l_english:\n key1: \"hi\"\n",
            Some(FileEncoding::Utf8Bom),
        )]);
        assert!(
            validate_loc_project(&svc, Game::HOI4)
                .iter()
                .all(|d| d.code != "CW254"),
            "UTF-8 BOM file should not warn CW254"
        );
    }

    #[test]
    fn unknown_encoding_no_cw254() {
        // The text-only path (LSP edits, tests) can't see bytes — no CW254.
        let svc =
            service_with_encoding(&[("a_l_english.yml", "l_english:\n key1: \"hi\"\n", None)]);
        assert!(
            validate_loc_project(&svc, Game::HOI4)
                .iter()
                .all(|d| d.code != "CW254")
        );
    }

    #[test]
    fn malformed_line_emits_cw001_and_rest_parses() {
        // A line with no ':' separator triggers CW001 at the recovery point.
        // The surrounding valid entries must still parse (parser remains lenient).
        let text = "l_english:\n good_key: \"valid\"\nthis line has no colon at all\n another_key: \"also valid\"\n";
        let svc = service_from(&[("a_l_english.yml", text)]);
        let diags = validate_loc_project(&svc, Game::HOI4);

        let cw001: Vec<_> = diags.iter().filter(|d| d.code == "CW001").collect();
        assert_eq!(
            cw001.len(),
            1,
            "exactly one CW001 for one bad line: {:?}",
            diags
        );
        assert_eq!(cw001[0].severity, LocSeverity::Error);
        assert_eq!(cw001[0].line, 3, "bad line is line 3");

        // The good entries still parse — no spurious CW225/CW100 from the bad line.
        assert!(
            diags.iter().all(|d| d.code != "CW225"),
            "no CW225 from recovered parse: {:?}",
            diags
        );
    }

    #[test]
    fn unterminated_string_maps_to_cw268() {
        // Regression: opening quote with no closing quote was falsely reported as
        // balanced because the truncation reduced effective to a single `"`.
        let svc = service_from(&[(
            "a_l_english.yml",
            "l_english:\n missing_quote:0 \"unclosed\n",
        )]);
        let diags = validate_loc_project(&svc, Game::HOI4);
        let cw268: Vec<_> = diags.iter().filter(|d| d.code == "CW268").collect();
        assert_eq!(
            cw268.len(),
            1,
            "unterminated string should emit CW268: {:?}",
            diags
        );
        assert_eq!(cw268[0].severity, LocSeverity::Warning);
    }

    #[test]
    fn key_with_space_maps_to_cw276() {
        let svc = service_from(&[("a_l_english.yml", "l_english:\n \"bad key\": \"value\"\n")]);
        let diags = validate_loc_project(&svc, Game::HOI4);
        let cw276: Vec<_> = diags.iter().filter(|d| d.code == "CW276").collect();
        assert_eq!(
            cw276.len(),
            1,
            "key with space should emit CW276: {:?}",
            diags
        );
        assert_eq!(cw276[0].severity, LocSeverity::Warning);
        assert!(
            cw276[0].message.contains("bad key") || cw276[0].message.contains("\"bad key\""),
            "message should reference the key: {}",
            cw276[0].message
        );
    }

    #[test]
    fn well_formed_file_no_cw001() {
        let svc = service_from(&[("a_l_english.yml", "l_english:\n key1: \"hi\"\n")]);
        assert!(
            validate_loc_project(&svc, Game::HOI4)
                .iter()
                .all(|d| d.code != "CW001"),
            "well-formed file must not emit CW001"
        );
    }
}
