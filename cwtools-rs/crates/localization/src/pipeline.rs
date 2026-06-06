//! Project-level loc-file validation.
//!
//! Runs the scope-independent loc-entry checks (`validate_loc_file`) over every
//! loaded loc file and normalizes the results to the F# numeric error codes
//! (CW225/CW234/CW259/CW268/CW275), plus the per-file name/header checks
//! (CW254/CW255/CW256/CW257). Scope-dependent command checks
//! (CW226/CW260/CW266) run at the config reference site, not here, because they
//! need the scope of the referencing field.

use crate::commands::{Game, Lang, LocFile};
use crate::service::LocService;
use crate::validation::{HARDCODED_LOC, LocErrorKind, validate_loc_file};
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

/// The F# numeric code for a scope-independent loc-entry error.
pub fn loc_error_code(kind: &LocErrorKind) -> &'static str {
    match kind {
        LocErrorKind::UndefinedLocReference { .. } => "CW225",
        LocErrorKind::RecursiveLocRef => "CW259",
        LocErrorKind::ReplaceMe => "CW234",
        LocErrorKind::LocMissingQuote => "CW268",
        LocErrorKind::LocInvalidChars => "CW275",
    }
}

/// The severity for a scope-independent loc-entry error.
pub fn loc_error_severity(kind: &LocErrorKind) -> LocSeverity {
    match kind {
        LocErrorKind::UndefinedLocReference { .. } | LocErrorKind::RecursiveLocRef => {
            LocSeverity::Error
        }
        LocErrorKind::ReplaceMe => LocSeverity::Information,
        LocErrorKind::LocMissingQuote | LocErrorKind::LocInvalidChars => LocSeverity::Warning,
    }
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
            "Localisation key {} contains unexpected characters, and may not render correctly",
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
            "CW256",
            LocSeverity::Error,
            "Localisation file should start with \"l_language:\" on the first line (or a comment)"
                .to_string(),
        ),
        LangHeaderDiagnostic::MissingLocFileLang { .. } => (
            "CW255",
            LocSeverity::Error,
            "Localisation file name should contain (and ideally end with) \"l_language.yml\""
                .to_string(),
        ),
        LangHeaderDiagnostic::LocFileLangMismatch {
            filename_lang,
            header_lang,
            ..
        } => (
            "CW257",
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

/// Validate every loaded loc file and return normalized diagnostics.
pub fn validate_loc_project(service: &LocService, _game: Game) -> Vec<LocDiagnostic> {
    // Union of keys across all languages, to resolve `$ref$` existence.
    // Borrowed from the service's single owned copy — no second copy of any loc
    // file is ever materialized (a full clone OOMs on large projects like MD).
    let mut union: HashSet<String> = HashSet::new();
    for file in service.files() {
        for e in &file.entries {
            union.insert(e.key.to_lowercase());
        }
    }

    // Each file validates independently against the read-only key union, so the
    // per-file pass runs in parallel. `par_iter` over the indexed `files` slice
    // collects in input order — output matches the sequential version.
    use rayon::prelude::*;
    let union_ref = &union;
    service
        .files()
        .par_iter()
        .flat_map_iter(|file| {
            let lang = file.lang;
            let path = &file.path;
            let mut out: Vec<LocDiagnostic> = Vec::new();

            // CW255/256/257: file name vs language header.
            if let Some(d) = lang_header_diagnostic(file) {
                out.push(d);
            }

            // CW254: localisation files must be UTF-8 with BOM. Only enforced
            // when the on-disk encoding is known (the directory-loading path).
            if matches!(
                file.encoding,
                Some(cwtools_file_manager::FileEncoding::Utf8NoBom)
                    | Some(cwtools_file_manager::FileEncoding::NonUtf8)
            ) {
                out.push(LocDiagnostic {
                    file: path.clone(),
                    line: 1,
                    col: 1,
                    code: "CW254",
                    severity: LocSeverity::Error,
                    message: "Localisation files must be UTF-8 BOM, this file is not".to_string(),
                });
            }

            for err in validate_loc_file(file, union_ref, HARDCODED_LOC) {
                out.push(LocDiagnostic {
                    file: path.clone(),
                    line: err.line,
                    col: err.col,
                    code: loc_error_code(&err.kind),
                    severity: loc_error_severity(&err.kind),
                    message: loc_error_message(&err.kind, &err.key, lang),
                });
            }
            out.into_iter()
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
) -> Vec<LocDiagnostic> {
    let Ok(file) = parse_loc_text(text, path) else {
        return Vec::new();
    };
    let lang = file.lang;
    let mut out: Vec<LocDiagnostic> = Vec::new();

    // CW255/256/257: file name vs language header.
    if let Some(d) = lang_header_diagnostic(&file) {
        out.push(d);
    }

    for err in validate_loc_file(&file, union, HARDCODED_LOC) {
        out.push(LocDiagnostic {
            file: path.to_string(),
            line: err.line,
            col: err.col,
            code: loc_error_code(&err.kind),
            severity: loc_error_severity(&err.kind),
            message: loc_error_message(&err.kind, &err.key, lang),
        });
    }
    out
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
}
