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

// ── Help / version ───────────────────────────────────────────────────────────

#[test]
fn test_help_exits_zero() {
    cwtools().arg("--help").assert().success();
}

#[test]
fn test_help_shows_version_info() {
    cwtools()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("cwtools"));
}

#[test]
fn test_help_contains_usage() {
    cwtools()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Usage"));
}

#[test]
fn test_subcommand_help_parse() {
    cwtools().args(["parse", "--help"]).assert().success();
}

#[test]
fn test_subcommand_help_discover() {
    cwtools().args(["discover", "--help"]).assert().success();
}

#[test]
fn test_subcommand_help_serialize() {
    cwtools().args(["serialize", "--help"]).assert().success();
}

#[test]
fn test_subcommand_help_deserialize() {
    cwtools().args(["deserialize", "--help"]).assert().success();
}

#[test]
fn test_subcommand_help_rules() {
    cwtools().args(["rules", "--help"]).assert().success();
}

#[test]
fn test_subcommand_help_validate() {
    cwtools().args(["validate", "--help"]).assert().success();
}

#[test]
fn test_subcommand_help_loc() {
    cwtools().args(["loc", "--help"]).assert().success();
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
