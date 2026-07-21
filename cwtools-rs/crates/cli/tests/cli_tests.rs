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
