//! Tests for LocalisationField existence checking (CW100 / CW122) wired into
//! the main validation pipeline via `validate_ast_with_loc`.

use cwtools_localization::{Game as LocGame, LocIndex, LocService};
use cwtools_parser::parser::parse_string;
use cwtools_rules::rules_converter::ast_to_ruleset;
use cwtools_string_table::string_table::StringTable;
use cwtools_validation::validate_ast_with_loc;

const CWT: &str = r#"
types = {
    type[mytype] = {
        path = "game/common/mytype"
    }
}
mytype = {
    name = localisation
    sname = localisation_synced
    iname = localisation_inline
}
"#;

fn loc_index(files: &[(&str, &str)]) -> LocIndex {
    let svc = LocService::from_files(
        files
            .iter()
            .map(|(p, t)| (p.to_string(), t.to_string()))
            .collect(),
    );
    LocIndex::build(&svc, LocGame::HOI4)
}

fn run(script: &str, idx: &LocIndex) -> Vec<cwtools_validation::ValidationError> {
    let table = StringTable::new();
    let parsed_cwt = parse_string(CWT, &table).unwrap();
    let ruleset = ast_to_ruleset(&parsed_cwt, &table);
    let parsed = parse_string(script, &table).unwrap();
    validate_ast_with_loc(
        &parsed,
        &ruleset,
        &table,
        "test.txt",
        None,
        None,
        None,
        Some(idx),
    )
}

fn cw100s(errs: &[cwtools_validation::ValidationError]) -> usize {
    errs.iter().filter(|e| e.code == Some("CW100")).count()
}

#[test]
fn unsynced_existing_key_ok() {
    let idx = loc_index(&[("a_l_english.yml", "l_english:\n my_key: \"hi\"\n")]);
    let errs = run("mytype = {\n name = my_key\n}\n", &idx);
    assert_eq!(cw100s(&errs), 0, "existing key should not warn: {:?}", errs);
}

#[test]
fn unsynced_missing_key_warns_cw100() {
    let idx = loc_index(&[("a_l_english.yml", "l_english:\n other: \"hi\"\n")]);
    let errs = run("mytype = {\n name = absent_key\n}\n", &idx);
    assert_eq!(
        cw100s(&errs),
        1,
        "missing key should warn CW100: {:?}",
        errs
    );
}

#[test]
fn inline_quoted_existing_key_warns_cw122() {
    let idx = loc_index(&[("a_l_english.yml", "l_english:\n my_key: \"hi\"\n")]);
    let errs = run("mytype = {\n iname = \"my_key\"\n}\n", &idx);
    let cw122 = errs.iter().filter(|e| e.code == Some("CW122")).count();
    assert_eq!(cw122, 1, "quoted inline existing key → CW122: {:?}", errs);
}

#[test]
fn inline_quoted_missing_key_is_skipped() {
    let idx = loc_index(&[("a_l_english.yml", "l_english:\n other: \"hi\"\n")]);
    let errs = run("mytype = {\n iname = \"absent\"\n}\n", &idx);
    assert_eq!(
        cw100s(&errs),
        0,
        "quoted+missing inline is lenient: {:?}",
        errs
    );
}

#[test]
fn synced_missing_in_a_language_warns() {
    // english + german both present; german lacks the key
    let idx = loc_index(&[
        ("a_l_english.yml", "l_english:\n my_key: \"hi\"\n"),
        ("a_l_german.yml", "l_german:\n other: \"hallo\"\n"),
    ]);
    let errs = run("mytype = {\n sname = my_key\n}\n", &idx);
    assert_eq!(
        cw100s(&errs),
        1,
        "synced key missing in german → one CW100: {:?}",
        errs
    );
}

#[test]
fn embedded_inline_command_is_skipped() {
    // A loc value with an inline `[...]` command plus a literal suffix is a
    // dynamic, runtime-substituted string (e.g. a meta_effect variable
    // `"[?ROOT.current_party_ideology_group.GetTokenKey]_subtype"`), not a literal
    // loc key. It must not warn CW100 (cwtools-vscode#25).
    let idx = loc_index(&[("a_l_english.yml", "l_english:\n other: \"hi\"\n")]);
    let errs = run(
        "mytype = {\n name = \"[GetIdeologyToken]_subtype\"\n}\n",
        &idx,
    );
    assert_eq!(
        cw100s(&errs),
        0,
        "embedded [..] command is dynamic, not a key: {:?}",
        errs
    );
}

#[test]
fn dollar_var_reference_is_skipped() {
    let idx = loc_index(&[("a_l_english.yml", "l_english:\n other: \"hi\"\n")]);
    let errs = run("mytype = {\n name = \"$SOME_VAR$\"\n}\n", &idx);
    assert_eq!(cw100s(&errs), 0, "$VAR$ refs are not key refs: {:?}", errs);
}

/// The numeric codes and severities the localization pipeline emits for the
/// scope-independent loc-entry checks must match the validation crate's catalog.
#[test]
fn loc_pipeline_codes_match_error_catalog() {
    use cwtools_localization::{LocErrorKind, loc_error_code, loc_error_severity};
    use cwtools_validation::error_codes as ec;

    let cases = [
        (
            LocErrorKind::UndefinedLocReference {
                other_key: "x".into(),
            },
            &ec::CW225_UNDEFINED_LOC_REFERENCE,
        ),
        (LocErrorKind::RecursiveLocRef, &ec::CW259_RECURSIVE_LOC_REF),
        (LocErrorKind::ReplaceMe, &ec::CW234_REPLACE_ME_LOC),
        (LocErrorKind::LocMissingQuote, &ec::CW268_LOC_MISSING_QUOTE),
        (LocErrorKind::LocInvalidChars, &ec::CW275_LOC_INVALID_CHARS),
        (
            LocErrorKind::LocKeyInvalidChars,
            &ec::CW276_LOC_KEY_INVALID_CHARS,
        ),
    ];

    for (kind, code) in cases {
        assert_eq!(
            loc_error_code(&kind),
            code.id,
            "code id mismatch for {kind:?}"
        );
        assert_eq!(
            loc_error_severity(&kind),
            code.severity,
            "severity mismatch for {kind:?}"
        );
    }
}

#[test]
fn overlay_key_resolves_missing_loc() {
    // Regression for cwtools-vscode#36: a loc key absent from the scanned index
    // but present in the live overlay (just typed into an open `.yml`) must NOT
    // warn CW100, so adding a key clears the diagnostic without a full rescan.
    use cwtools_validation::{Prepared, validate_prepared};
    use std::collections::HashSet;

    let table = StringTable::new();
    let parsed_cwt = parse_string(CWT, &table).unwrap();
    let ruleset = ast_to_ruleset(&parsed_cwt, &table);
    let parsed = parse_string("mytype = {\n name = just_added_key\n}\n", &table).unwrap();
    // Index lacks the key.
    let idx = loc_index(&[("a_l_english.yml", "l_english:\n other: \"hi\"\n")]);

    let base = Prepared {
        ruleset: &ruleset,
        table: &table,
        game: None,
        type_index: None,
        modifier_keys: None,
        loc_index: Some(&idx),
        extra_loc_keys: None,
        registry: None,
        scope_checks: false,
        var_checks: false,
    };
    // No overlay → the key is missing → CW100.
    let errs = validate_prepared(&parsed, "test.txt", &base);
    assert_eq!(
        cw100s(&errs),
        1,
        "missing key without overlay → CW100: {:?}",
        errs
    );

    // Overlay carries the (lowercased) key → resolved, no CW100.
    let mut overlay = HashSet::new();
    overlay.insert("just_added_key".to_string());
    let with_overlay = Prepared {
        extra_loc_keys: Some(&overlay),
        ..base
    };
    let errs = validate_prepared(&parsed, "test.txt", &with_overlay);
    assert_eq!(
        cw100s(&errs),
        0,
        "overlay key resolves → no CW100: {:?}",
        errs
    );
}

#[test]
fn no_loc_index_is_lenient() {
    let table = StringTable::new();
    let parsed_cwt = parse_string(CWT, &table).unwrap();
    let ruleset = ast_to_ruleset(&parsed_cwt, &table);
    let parsed = parse_string("mytype = {\n name = absent_key\n}\n", &table).unwrap();
    let errs = validate_ast_with_loc(
        &parsed, &ruleset, &table, "test.txt", None, None, None, None,
    );
    assert_eq!(cw100s(&errs), 0, "no loc loaded → accept: {:?}", errs);
}
