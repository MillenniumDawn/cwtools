use assert_cmd::Command;
use predicates::prelude::*;
use std::path::PathBuf;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

fn cwtools() -> Command {
    let mut cmd = Command::cargo_bin("cwtools").unwrap();
    cmd.env("RUST_LOG", "");
    cmd
}

// ── Help ─────────────────────────────────────────────────────────────────────

#[test]
fn test_help_exits_with_usage() {
    cwtools()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("cwtools"))
        .stdout(predicate::str::contains("Usage"));
}

// ── Parse ────────────────────────────────────────────────────────────────────

#[test]
fn test_parse_single_file() {
    let simple = fixtures_dir().join("simple.txt");
    cwtools()
        .args(["parse", simple.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Parsed"))
        .stdout(predicate::str::contains("Leaves"))
        .stdout(predicate::str::contains("Leaves"));
}

#[test]
fn test_parse_rules_directory() {
    let rules_dir = fixtures_dir().join("rules");
    cwtools()
        .args(["parse", rules_dir.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Parsed rule directory"))
        .stdout(predicate::str::contains("Types"))
        .stdout(predicate::str::contains("Enums"));
}

#[test]
fn test_parse_missing_file_fails() {
    cwtools()
        .args(["parse", "/nonexistent/path/file.txt"])
        .assert()
        .failure();
}

// ── Discover ─────────────────────────────────────────────────────────────────

#[test]
fn test_discover_mod_directory() {
    let discover_dir = fixtures_dir().join("discover").join("mod_a");
    cwtools()
        .args(["discover", discover_dir.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Discovered and parsed"))
        .stdout(predicate::str::contains("2 files"));
}

#[test]
fn test_discover_empty_directory() {
    let tmp = tempfile::tempdir().unwrap();
    cwtools()
        .args(["discover", tmp.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Discovered and parsed 0 files"));
}

// ── Rules ────────────────────────────────────────────────────────────────────

#[test]
fn test_rules_single_file() {
    let rules_file = fixtures_dir().join("rules").join("test.cwt");
    cwtools()
        .args(["rules", rules_file.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Parsed rules file"))
        .stdout(predicate::str::contains("test_type"))
        .stdout(predicate::str::contains("test_enum"));
}

#[test]
fn test_rules_directory() {
    let rules_dir = fixtures_dir().join("rules");
    cwtools()
        .args(["rules", rules_dir.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Parsed"));
}

#[test]
fn test_rules_missing_file_fails() {
    cwtools()
        .args(["rules", "/nonexistent/rules.cwt"])
        .assert()
        .failure();
}

// ── Serialize / Deserialize ──────────────────────────────────────────────────

#[test]
fn test_serialize_and_deserialize_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let cwb = tmp.path().join("test.cwb");
    let simple = fixtures_dir().join("simple.txt");

    // Serialize
    cwtools()
        .args(["serialize", simple.to_str().unwrap(), cwb.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Serialized"));

    // Deserialize
    cwtools()
        .args(["deserialize", cwb.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Deserialized"))
        .stdout(predicate::str::contains("Leaves"));
}

#[test]
fn test_serialize_missing_input_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let cwb = tmp.path().join("out.cwb");
    cwtools()
        .args(["serialize", "/nonexistent/file.txt", cwb.to_str().unwrap()])
        .assert()
        .failure();
}

// ── Validate ─────────────────────────────────────────────────────────────────

#[test]
fn test_validate_with_rules() {
    let discover_dir = fixtures_dir().join("discover").join("mod_a");
    let rules_dir = fixtures_dir().join("rules");
    cwtools()
        .args([
            "validate",
            "--game",
            "stellaris",
            "--directory",
            discover_dir.to_str().unwrap(),
            "--rules",
            rules_dir.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Validation complete"));
}

#[test]
fn test_validate_bad_game_name_fails() {
    let discover_dir = fixtures_dir().join("discover").join("mod_a");
    let rules_dir = fixtures_dir().join("rules");
    cwtools()
        .args([
            "validate",
            "--game",
            "not_a_real_game",
            "--directory",
            discover_dir.to_str().unwrap(),
            "--rules",
            rules_dir.to_str().unwrap(),
        ])
        .assert()
        .failure();
}

#[test]
fn test_validate_json_report() {
    let discover_dir = fixtures_dir().join("discover").join("mod_a");
    let rules_dir = fixtures_dir().join("rules");
    cwtools()
        .args([
            "validate",
            "--game",
            "stellaris",
            "--directory",
            discover_dir.to_str().unwrap(),
            "--rules",
            rules_dir.to_str().unwrap(),
            "--report-type",
            "json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("["));
}

#[test]
fn test_validate_csv_report() {
    let discover_dir = fixtures_dir().join("discover").join("mod_a");
    let rules_dir = fixtures_dir().join("rules");
    cwtools()
        .args([
            "validate",
            "--game",
            "stellaris",
            "--directory",
            discover_dir.to_str().unwrap(),
            "--rules",
            rules_dir.to_str().unwrap(),
            "--report-type",
            "csv",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("file,line,severity"));
}

#[test]
fn test_validate_loc_language_valid_accepted() {
    let discover_dir = fixtures_dir().join("discover").join("mod_a");
    let rules_dir = fixtures_dir().join("rules");
    cwtools()
        .args([
            "validate",
            "--game",
            "stellaris",
            "--directory",
            discover_dir.to_str().unwrap(),
            "--rules",
            rules_dir.to_str().unwrap(),
            "--loc-language",
            "english",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Validation complete"));
}

#[test]
fn test_validate_loc_language_unknown_fails() {
    let discover_dir = fixtures_dir().join("discover").join("mod_a");
    let rules_dir = fixtures_dir().join("rules");
    cwtools()
        .args([
            "validate",
            "--game",
            "stellaris",
            "--directory",
            discover_dir.to_str().unwrap(),
            "--rules",
            rules_dir.to_str().unwrap(),
            "--loc-language",
            "klingon",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid language 'klingon'"))
        .stderr(predicate::str::contains("english"));
}

#[test]
fn test_validate_min_severity_filters_lower_severities() {
    let discover_dir = fixtures_dir().join("discover").join("mod_a");
    let rules_dir = fixtures_dir().join("rules");
    // mod_a's event triggers an Information-severity CW107; --min-severity
    // error should drop it from the report.
    cwtools()
        .args([
            "validate",
            "--game",
            "stellaris",
            "--directory",
            discover_dir.to_str().unwrap(),
            "--rules",
            rules_dir.to_str().unwrap(),
            "--min-severity",
            "error",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("CW107").not());
}

#[test]
fn test_validate_min_severity_unknown_fails() {
    let discover_dir = fixtures_dir().join("discover").join("mod_a");
    let rules_dir = fixtures_dir().join("rules");
    cwtools()
        .args([
            "validate",
            "--game",
            "stellaris",
            "--directory",
            discover_dir.to_str().unwrap(),
            "--rules",
            rules_dir.to_str().unwrap(),
            "--min-severity",
            "bogus",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid severity 'bogus'"));
}

#[test]
fn test_validate_output_file() {
    let tmp = tempfile::tempdir().unwrap();
    let report = tmp.path().join("report.txt");
    let discover_dir = fixtures_dir().join("discover").join("mod_a");
    let rules_dir = fixtures_dir().join("rules");
    cwtools()
        .args([
            "validate",
            "--game",
            "stellaris",
            "--directory",
            discover_dir.to_str().unwrap(),
            "--rules",
            rules_dir.to_str().unwrap(),
            "--output-file",
            report.to_str().unwrap(),
        ])
        .assert()
        .success();
    assert!(report.exists());
}

// ── Loc ──────────────────────────────────────────────────────────────────────

#[test]
fn test_loc_valid_directory() {
    let loc_dir = fixtures_dir().join("loc");
    cwtools()
        .args(["loc", loc_dir.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Scanning localisation"))
        .stdout(predicate::str::contains("Loc validation complete"));
}

#[test]
fn test_loc_detects_unterminated_quote() {
    // CW268 is Warning-severity, so this now exits 0 (severity-aware exit,
    // like `validate`) even though the issue is still reported.
    let loc_dir = fixtures_dir().join("loc_invalid");
    cwtools()
        .args(["loc", loc_dir.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("CW268"))
        .stdout(predicate::str::contains("missing_quote"))
        .stdout(predicate::str::contains("1 issues"));
}

#[test]
fn test_loc_information_only_succeeds() {
    // CW234 (REPLACE_ME placeholder) is Information-severity; exit 0.
    let loc_dir = fixtures_dir().join("loc_info_only");
    cwtools()
        .args(["loc", loc_dir.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("CW234"));
}

#[test]
fn test_loc_error_severity_fails() {
    // CW225 (undefined loc reference) is Error-severity; exit 1 unchanged.
    let loc_dir = fixtures_dir().join("loc_error");
    cwtools()
        .args(["loc", loc_dir.to_str().unwrap()])
        .assert()
        .failure()
        .stdout(predicate::str::contains("CW225"));
}

#[test]
fn test_loc_empty_directory() {
    let tmp = tempfile::tempdir().unwrap();
    cwtools()
        .args(["loc", tmp.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("0 entries"));
}

#[test]
fn test_loc_default_report_type_matches_explicit_cli() {
    // The default (no --report-type) must render exactly like --report-type
    // cli, byte for byte — report/hash parity must not touch the default path.
    let loc_dir = fixtures_dir().join("loc_invalid");
    let default_out = cwtools()
        .args(["loc", loc_dir.to_str().unwrap()])
        .output()
        .unwrap();
    let explicit_out = cwtools()
        .args(["loc", loc_dir.to_str().unwrap(), "--report-type", "cli"])
        .output()
        .unwrap();
    assert_eq!(default_out.stdout, explicit_out.stdout);
    assert_eq!(default_out.status.code(), explicit_out.status.code());
}

#[test]
fn test_loc_json_report() {
    let loc_dir = fixtures_dir().join("loc_invalid");
    cwtools()
        .args(["loc", loc_dir.to_str().unwrap(), "--report-type", "json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"code\":\"CW268\""))
        .stdout(predicate::str::contains("\"hash\":"));
}

#[test]
fn test_loc_csv_report() {
    let loc_dir = fixtures_dir().join("loc_invalid");
    cwtools()
        .args(["loc", loc_dir.to_str().unwrap(), "--report-type", "csv"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "file,line,severity,code,message,hash",
        ))
        .stdout(predicate::str::contains("CW268"));
}

#[test]
fn test_loc_output_file() {
    let tmp = tempfile::tempdir().unwrap();
    let report = tmp.path().join("report.txt");
    let loc_dir = fixtures_dir().join("loc_invalid");
    cwtools()
        .args([
            "loc",
            loc_dir.to_str().unwrap(),
            "--output-file",
            report.to_str().unwrap(),
        ])
        .assert()
        .success();
    let contents = std::fs::read_to_string(&report).unwrap();
    assert!(contents.contains("CW268"));
}

#[test]
fn test_loc_hash_write_and_ignore_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let hashes = tmp.path().join("hashes.txt");
    let loc_dir = fixtures_dir().join("loc_invalid");

    // First run: exits 0 (CW268 is Warning-severity) and writes the baseline.
    cwtools()
        .args([
            "loc",
            loc_dir.to_str().unwrap(),
            "--output-hashes",
            hashes.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("CW268"));
    let baseline = std::fs::read_to_string(&hashes).unwrap();
    assert_eq!(baseline.lines().count(), 1, "one surviving diagnostic hash");

    // Second run with that baseline as --ignore-hashes: the diagnostic is
    // suppressed from the report entirely.
    cwtools()
        .args([
            "loc",
            loc_dir.to_str().unwrap(),
            "--ignore-hashes",
            hashes.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("CW268").not())
        .stdout(predicate::str::contains("0 issues"));
}

#[test]
fn test_loc_ignore_hashes_filters_error_before_exit_code() {
    // CW225 (undefined loc reference) is Error-severity and normally fails
    // the run. Baselining its hash must suppress it from BOTH the report and
    // the exit-code count, same placement as `validate`.
    let tmp = tempfile::tempdir().unwrap();
    let hashes = tmp.path().join("hashes.txt");
    let loc_dir = fixtures_dir().join("loc_error");

    cwtools()
        .args([
            "loc",
            loc_dir.to_str().unwrap(),
            "--output-hashes",
            hashes.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stdout(predicate::str::contains("CW225"));

    cwtools()
        .args([
            "loc",
            loc_dir.to_str().unwrap(),
            "--ignore-hashes",
            hashes.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("CW225").not());
}

// ── Fix ──────────────────────────────────────────────────────────────────────

/// A temp mod with one `common/` file carrying an empty `if` (CW121, fixable).
fn fix_mod() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let common = tmp.path().join("common");
    std::fs::create_dir_all(&common).unwrap();
    std::fs::write(common.join("hint.txt"), "x = { if = { } }\n").unwrap();
    tmp
}

#[test]
fn test_fix_dry_run_previews_without_writing() {
    let tmp = fix_mod();
    let rules_dir = fixtures_dir().join("rules");
    let file = tmp.path().join("common").join("hint.txt");
    cwtools()
        .args([
            "fix",
            "--game",
            "stellaris",
            "--directory",
            tmp.path().to_str().unwrap(),
            "--rules",
            rules_dir.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Dry run"))
        .stdout(predicate::str::contains("@@"));
    // Dry run must not touch the file.
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "x = { if = { } }\n"
    );
}

#[test]
fn test_fix_apply_writes_and_is_idempotent() {
    let tmp = fix_mod();
    let rules_dir = fixtures_dir().join("rules");
    let file = tmp.path().join("common").join("hint.txt");
    cwtools()
        .args([
            "fix",
            "--game",
            "stellaris",
            "--directory",
            tmp.path().to_str().unwrap(),
            "--rules",
            rules_dir.to_str().unwrap(),
            "--apply",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Applied"));
    // The empty if is gone.
    let after = std::fs::read_to_string(&file).unwrap();
    assert_eq!(after, "x = { }\n", "empty if should be removed");
    // A second run finds nothing left to fix.
    cwtools()
        .args([
            "fix",
            "--game",
            "stellaris",
            "--directory",
            tmp.path().to_str().unwrap(),
            "--rules",
            rules_dir.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("0 fix(es)"));
}

#[test]
fn test_fix_code_filter_excludes_unmatched() {
    let tmp = fix_mod();
    let rules_dir = fixtures_dir().join("rules");
    // Filtering to a code that isn't present leaves nothing to fix.
    cwtools()
        .args([
            "fix",
            "--game",
            "stellaris",
            "--directory",
            tmp.path().to_str().unwrap(),
            "--rules",
            rules_dir.to_str().unwrap(),
            "--code",
            "CW999",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("0 fix(es)"));
}

// ── Error handling ───────────────────────────────────────────────────────────

#[test]
fn test_unknown_engine_fails() {
    cwtools()
        .args(["--engine", "fortran", "parse", "somefile"])
        .assert()
        .failure();
}

#[test]
fn test_no_subcommand_fails() {
    cwtools().assert().failure();
}
