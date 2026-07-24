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
fn test_help_exits_with_usage() {
    cwtools()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("cwtools"))
        .stdout(predicate::str::contains("Usage"));
}

#[test]
fn test_version_flag_prints_crate_version() {
    // Same source as `cwtools-server --version` (CARGO_PKG_VERSION), so the two
    // binaries can't report different versions.
    cwtools()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn test_version_short_flag_prints_crate_version() {
    cwtools()
        .arg("-V")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
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

// ── Automatic base-game cache ────────────────────────────────────────────────
//
// `--vanilla` keeps its index under the OS cache dir (XDG_CACHE_HOME here) so a
// repeat run doesn't re-parse the install; `--no-vanilla-cache` opts out.

/// Run `validate --vanilla <fixture>` with `cache` as the cache home, and return
/// the number of cache files that exist afterwards.
fn validate_with_cache_home(cache_home: &std::path::Path, extra: &[&str]) -> usize {
    let discover_dir = fixtures_dir().join("discover").join("mod_a");
    let rules_dir = fixtures_dir().join("rules");
    let mut cmd = cwtools();
    cmd.env("XDG_CACHE_HOME", cache_home).args([
        "validate",
        "--game",
        "stellaris",
        "--directory",
        discover_dir.to_str().unwrap(),
        "--rules",
        rules_dir.to_str().unwrap(),
        "--vanilla",
        discover_dir.to_str().unwrap(),
    ]);
    cmd.args(extra).assert().success();
    let dir = cache_home.join("cwtools");
    std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "cwv"))
                .count()
        })
        .unwrap_or(0)
}

#[test]
fn test_validate_vanilla_writes_and_reuses_the_cache() {
    let cache = tempfile::tempdir().unwrap();
    assert_eq!(
        validate_with_cache_home(cache.path(), &[]),
        1,
        "the first --vanilla run should write a cache"
    );
    assert_eq!(
        validate_with_cache_home(cache.path(), &[]),
        1,
        "a repeat run reuses that cache rather than writing another"
    );
    assert_eq!(
        validate_with_cache_home(cache.path(), &["--refresh-vanilla-cache"]),
        1,
        "--refresh-vanilla-cache overwrites in place"
    );
}

#[test]
fn test_validate_no_vanilla_cache_writes_nothing() {
    let cache = tempfile::tempdir().unwrap();
    assert_eq!(
        validate_with_cache_home(cache.path(), &["--no-vanilla-cache"]),
        0,
        "--no-vanilla-cache must not touch the cache dir"
    );
}

// ── Empty inputs ─────────────────────────────────────────────────────────────
//
// A run that validated nothing must not look like a clean run: exit 4 names the
// empty input, exit 3 a path that doesn't resolve, and `--allow-empty` opts back
// into the old permissive behavior.

#[test]
fn test_validate_empty_rules_dir_fails() {
    let empty_rules = tempfile::tempdir().unwrap();
    let discover_dir = fixtures_dir().join("discover").join("mod_a");
    cwtools()
        .args([
            "validate",
            "--game",
            "stellaris",
            "--directory",
            discover_dir.to_str().unwrap(),
            "--rules",
            empty_rules.path().to_str().unwrap(),
        ])
        .assert()
        .failure()
        .code(4)
        .stderr(predicate::str::contains("--rules loaded 0 types"))
        .stderr(predicate::str::contains(
            empty_rules.path().to_str().unwrap(),
        ));
}

#[test]
fn test_validate_empty_rules_dir_allowed_with_flag() {
    let empty_rules = tempfile::tempdir().unwrap();
    let discover_dir = fixtures_dir().join("discover").join("mod_a");
    cwtools()
        .args([
            "validate",
            "--game",
            "stellaris",
            "--directory",
            discover_dir.to_str().unwrap(),
            "--rules",
            empty_rules.path().to_str().unwrap(),
            "--allow-empty",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Validation complete"));
}

#[test]
fn test_validate_empty_directory_fails() {
    let empty_mod = tempfile::tempdir().unwrap();
    let rules_dir = fixtures_dir().join("rules");
    cwtools()
        .args([
            "validate",
            "--game",
            "stellaris",
            "--directory",
            empty_mod.path().to_str().unwrap(),
            "--rules",
            rules_dir.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .code(4)
        .stderr(predicate::str::contains("--directory contains no files"))
        .stderr(predicate::str::contains(empty_mod.path().to_str().unwrap()));
}

#[test]
fn test_validate_empty_directory_allowed_with_flag() {
    let empty_mod = tempfile::tempdir().unwrap();
    let rules_dir = fixtures_dir().join("rules");
    cwtools()
        .args([
            "validate",
            "--game",
            "stellaris",
            "--directory",
            empty_mod.path().to_str().unwrap(),
            "--rules",
            rules_dir.to_str().unwrap(),
            "--allow-empty",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Validation complete"));
}

#[test]
fn test_validate_missing_directory_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("no_such_mod");
    let rules_dir = fixtures_dir().join("rules");
    cwtools()
        .args([
            "validate",
            "--game",
            "stellaris",
            "--directory",
            missing.to_str().unwrap(),
            "--rules",
            rules_dir.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .code(3)
        .stderr(predicate::str::contains(missing.to_str().unwrap()));
}

#[test]
fn test_validate_missing_directory_not_excused_by_allow_empty() {
    // --allow-empty covers a deliberately empty run, never a path that isn't there.
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("no_such_mod");
    let rules_dir = fixtures_dir().join("rules");
    cwtools()
        .args([
            "validate",
            "--game",
            "stellaris",
            "--directory",
            missing.to_str().unwrap(),
            "--rules",
            rules_dir.to_str().unwrap(),
            "--allow-empty",
        ])
        .assert()
        .failure()
        .code(3);
}

#[test]
fn test_fix_empty_rules_dir_fails() {
    let empty_rules = tempfile::tempdir().unwrap();
    let tmp = fix_mod();
    cwtools()
        .args([
            "fix",
            "--game",
            "stellaris",
            "--directory",
            tmp.path().to_str().unwrap(),
            "--rules",
            empty_rules.path().to_str().unwrap(),
        ])
        .assert()
        .failure()
        .code(4)
        .stderr(predicate::str::contains("--rules loaded 0 types"));
}

#[test]
fn test_fix_missing_directory_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("no_such_mod");
    let rules_dir = fixtures_dir().join("rules");
    cwtools()
        .args([
            "fix",
            "--game",
            "stellaris",
            "--directory",
            missing.to_str().unwrap(),
            "--rules",
            rules_dir.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .code(3)
        .stderr(predicate::str::contains(missing.to_str().unwrap()));
}

#[test]
fn test_loc_missing_directory_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("no_such_loc");
    cwtools()
        .args(["loc", missing.to_str().unwrap()])
        .assert()
        .failure()
        .code(3)
        .stderr(predicate::str::contains(missing.to_str().unwrap()));
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
fn test_loc_empty_directory_fails() {
    // A loc scan that found no files is not a clean run (a typo'd path used to
    // report "0 entries" and exit 0).
    let tmp = tempfile::tempdir().unwrap();
    cwtools()
        .args(["loc", tmp.path().to_str().unwrap()])
        .assert()
        .failure()
        .code(4)
        .stderr(predicate::str::contains("no localisation files found"))
        .stderr(predicate::str::contains(tmp.path().to_str().unwrap()));
}

#[test]
fn test_loc_empty_directory_allowed_with_flag() {
    let tmp = tempfile::tempdir().unwrap();
    cwtools()
        .args(["loc", tmp.path().to_str().unwrap(), "--allow-empty"])
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

#[test]
fn test_validate_hash_baseline_survives_inserted_line() {
    // The digest is content-derived, so inserting a line above a baselined
    // diagnostic must not resurface it as new.
    let tmp = fix_mod();
    let rules_dir = fixtures_dir().join("rules");
    let file = tmp.path().join("common").join("hint.txt");
    let hashes = tmp.path().join("hashes.txt");
    let validate = |extra: [&str; 2]| {
        let mut cmd = cwtools();
        cmd.args([
            "validate",
            "--game",
            "stellaris",
            "--directory",
            tmp.path().to_str().unwrap(),
            "--rules",
            rules_dir.to_str().unwrap(),
        ])
        .args(extra);
        cmd
    };

    validate(["--output-hashes", hashes.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("CW121"));

    // Push the diagnostic down a line; its text is untouched.
    std::fs::write(&file, "# a new comment\nx = { if = { } }\n").unwrap();

    validate(["--ignore-hashes", hashes.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("CW121").not());
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

// ── Config file (cwtools.toml) ───────────────────────────────────────────────
//
// Discovery walks up from --directory (or the CWD), so every test here anchors
// on a tempdir: the tree above it holds no cwtools.toml.

/// A mod root whose only diagnostic is the CW107 from `mod_a`'s event, with
/// `cwtools.toml` written at `root/cwtools.toml`.
fn config_mod(body: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let events = tmp.path().join("mod").join("events");
    std::fs::create_dir_all(&events).unwrap();
    std::fs::copy(
        fixtures_dir().join("discover/mod_a/events/test.txt"),
        events.join("test.txt"),
    )
    .unwrap();
    std::fs::write(tmp.path().join("cwtools.toml"), body).unwrap();
    tmp
}

/// The `game`/`directory`/`rules` triple as a config body, with `directory`
/// written relative so the resolve-against-the-config rule is under test.
fn config_body() -> String {
    format!(
        "game = \"stellaris\"\ndirectory = \"mod\"\nrules = {:?}\n",
        fixtures_dir().join("rules").to_str().unwrap()
    )
}

#[test]
fn test_config_supplies_game_directory_and_rules() {
    let tmp = config_mod(&config_body());
    cwtools()
        .arg("validate")
        .current_dir(tmp.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("Validation complete"))
        .stdout(predicate::str::contains("CW107"))
        .stderr(predicate::str::contains("Using config"))
        .stderr(predicate::str::contains("applied: game, directory, rules"));
}

#[test]
fn test_config_is_discovered_by_walking_up_from_the_directory() {
    let tmp = config_mod(&config_body());
    // Run from a directory well below the config, with no --config.
    let deep = tmp.path().join("mod").join("events");
    cwtools()
        .arg("validate")
        .current_dir(&deep)
        .assert()
        .success()
        .stderr(predicate::str::contains("Using config"));
}

/// A CI job runs from wherever the runner puts it; a relative `rules =` must
/// still resolve.
#[test]
fn test_config_relative_paths_resolve_against_the_config_file() {
    let tmp = config_mod("game = \"stellaris\"\ndirectory = \"mod\"\nrules = \"rules\"\n");
    // Copy the rules fixture next to the config so `rules = "rules"` resolves.
    let rules = tmp.path().join("rules");
    std::fs::create_dir_all(&rules).unwrap();
    std::fs::copy(
        fixtures_dir().join("rules").join("test.cwt"),
        rules.join("test.cwt"),
    )
    .unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    cwtools()
        .args(["validate", "--config"])
        .arg(tmp.path().join("cwtools.toml"))
        .current_dir(elsewhere.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("Validation complete"));
}

#[test]
fn test_flags_override_config_values() {
    // The config names a game the fixture rules aren't for; the flag wins, and
    // the overridden key is not listed as applied.
    let tmp = config_mod(&config_body().replace("stellaris", "eu4"));
    cwtools()
        .args(["validate", "--game", "stellaris"])
        .current_dir(tmp.path())
        .assert()
        .success()
        .stderr(predicate::str::contains("Validating stellaris files"))
        .stderr(predicate::str::contains("applied: directory, rules"));
}

#[test]
fn test_explicit_config_overrides_discovery() {
    let tmp = config_mod("game = \"eu4\"\n");
    let chosen = tmp.path().join("ci.toml");
    std::fs::write(&chosen, config_body()).unwrap();
    cwtools()
        .args(["validate", "--config"])
        .arg(&chosen)
        .current_dir(tmp.path())
        .assert()
        .success()
        .stderr(predicate::str::contains("ci.toml"))
        .stderr(predicate::str::contains("Validating stellaris files"));
}

#[test]
fn test_config_covers_the_cache_flags() {
    let tmp = config_mod(&format!("{}no-vanilla-cache = true\n", config_body()));
    cwtools()
        .arg("validate")
        .current_dir(tmp.path())
        .assert()
        .success()
        .stderr(predicate::str::contains("no-vanilla-cache"));
}

#[test]
fn test_config_supplies_the_code_filters() {
    let tmp = config_mod(&format!("{}ignore-codes = [\"CW107\"]\n", config_body()));
    cwtools()
        .arg("validate")
        .current_dir(tmp.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("CW107").not())
        .stderr(predicate::str::contains("ignore-codes"));
}

#[test]
fn test_malformed_config_fails_loudly_naming_the_file_and_line() {
    let tmp = config_mod("game = \"stellaris\"\ngmae = \"stellaris\"\n");
    cwtools()
        .arg("validate")
        .current_dir(tmp.path())
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("cwtools.toml:2"))
        .stderr(predicate::str::contains("unknown key `gmae`"))
        // Never a silent fallback to defaults.
        .stdout(predicate::str::contains("Validation complete").not());
}

#[test]
fn test_config_with_a_bad_value_fails() {
    let tmp = config_mod("game = \"hoi5\"\n");
    cwtools()
        .arg("validate")
        .current_dir(tmp.path())
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("unknown game `hoi5`"));
}

#[test]
fn test_missing_explicit_config_fails() {
    let tmp = tempfile::tempdir().unwrap();
    cwtools()
        .args(["validate", "--config"])
        .arg(tmp.path().join("nope.toml"))
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("no such config file"));
}

#[test]
fn test_missing_required_setting_is_a_usage_error() {
    let tmp = tempfile::tempdir().unwrap();
    cwtools()
        .args(["validate", "--directory"])
        .arg(tmp.path())
        .args(["--rules", fixtures_dir().join("rules").to_str().unwrap()])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("--game <GAME>"))
        .stderr(predicate::str::contains("cwtools.toml"));
}

#[test]
fn test_loc_reads_the_config_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let loc = tmp.path().join("mod").join("localisation");
    std::fs::create_dir_all(&loc).unwrap();
    std::fs::copy(
        fixtures_dir().join("loc_error/localisation/l_english.yml"),
        loc.join("l_english.yml"),
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("cwtools.toml"),
        "directory = \"mod\"\nignore-codes = [\"CW225\"]\n",
    )
    .unwrap();
    // CW225 is the only (Error-severity) diagnostic, so suppressing it also
    // flips the exit code.
    cwtools()
        .arg("loc")
        .current_dir(tmp.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("CW225").not());
}

// ── Defaults are unchanged ───────────────────────────────────────────────────

/// A command line that worked before the config/CI work must still produce the
/// same bytes on stdout, the same exit code, and no extra stderr chatter.
#[test]
fn test_validate_default_report_type_matches_explicit_cli() {
    let mod_dir = fixtures_dir().join("discover").join("mod_a");
    let rules_dir = fixtures_dir().join("rules");
    let args = [
        "validate",
        "--game",
        "stellaris",
        "--directory",
        mod_dir.to_str().unwrap(),
        "--rules",
        rules_dir.to_str().unwrap(),
    ];
    let default_out = cwtools().args(args).output().unwrap();
    let explicit_out = cwtools()
        .args(args)
        .args(["--report-type", "cli"])
        .output()
        .unwrap();
    assert_eq!(default_out.stdout, explicit_out.stdout);
    assert_eq!(default_out.stderr, explicit_out.stderr);
    assert_eq!(default_out.status.code(), explicit_out.status.code());

    let stdout = String::from_utf8(default_out.stdout).unwrap();
    assert!(stdout.starts_with("\n  "), "grouped by file: {stdout:?}");
    assert!(stdout.contains("    [Information] [CW107] "), "{stdout:?}");
    assert!(
        stdout.ends_with("\nValidation complete: 0 errors, 0 warnings\n"),
        "{stdout:?}"
    );
    assert_eq!(default_out.status.code(), Some(0));
    // No config file anywhere above the fixtures, so no config chatter.
    let stderr = String::from_utf8(default_out.stderr).unwrap();
    assert!(!stderr.contains("Using config"), "{stderr:?}");
}

#[test]
fn test_validate_default_run_has_no_code_filter() {
    // Empty --ignore-code/--only-code lists must keep every diagnostic.
    let mod_dir = fixtures_dir().join("discover").join("mod_a");
    let rules_dir = fixtures_dir().join("rules");
    cwtools()
        .args([
            "validate",
            "--game",
            "stellaris",
            "--directory",
            mod_dir.to_str().unwrap(),
            "--rules",
            rules_dir.to_str().unwrap(),
            "--report-type",
            "csv",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(",CW107,"));
}

// ── Report formats: github / sarif ───────────────────────────────────────────

#[test]
fn test_validate_github_report() {
    let mod_dir = fixtures_dir().join("discover").join("mod_a");
    let rules_dir = fixtures_dir().join("rules");
    let out = cwtools()
        .args([
            "validate",
            "--game",
            "stellaris",
            "--directory",
            mod_dir.to_str().unwrap(),
            "--rules",
            rules_dir.to_str().unwrap(),
            "--report-type",
            "github",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    // CW107 is Information-severity, which GitHub renders as a notice.
    assert!(stdout.starts_with("::notice file="), "{stdout:?}");
    assert!(stdout.contains(",line=3,col="), "{stdout:?}");
    assert!(stdout.contains(",title=CW107::"), "{stdout:?}");
    // One workflow command per diagnostic, on one physical line each.
    assert_eq!(stdout.lines().count(), 1, "{stdout:?}");
}

/// GitHub resolves annotation paths against the checkout root, which a step's
/// `working-directory:` does not move.
#[test]
fn test_github_report_paths_are_relative_to_the_workspace() {
    let mod_dir = fixtures_dir().join("discover").join("mod_a");
    let rules_dir = fixtures_dir().join("rules");
    let out = cwtools()
        .env("GITHUB_WORKSPACE", mod_dir.to_str().unwrap())
        .current_dir(std::env::temp_dir())
        .args([
            "validate",
            "--game",
            "stellaris",
            "--directory",
            mod_dir.to_str().unwrap(),
            "--rules",
            rules_dir.to_str().unwrap(),
            "--report-type",
            "github",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("file=events/test.txt,"), "{stdout:?}");
}

#[test]
fn test_validate_sarif_report_is_well_formed() {
    let mod_dir = fixtures_dir().join("discover").join("mod_a");
    let rules_dir = fixtures_dir().join("rules");
    let out = cwtools()
        .args([
            "validate",
            "--game",
            "stellaris",
            "--directory",
            mod_dir.to_str().unwrap(),
            "--rules",
            rules_dir.to_str().unwrap(),
            "--report-type",
            "sarif",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("\"version\": \"2.1.0\""), "{stdout:?}");
    assert!(stdout.contains("sarif-schema-2.1.0.json"), "{stdout:?}");
    assert!(stdout.contains("\"name\": \"cwtools\""), "{stdout:?}");
    // The rule definition comes from the shared error-code catalog.
    assert!(stdout.contains("\"id\": \"CW107\""), "{stdout:?}");
    assert!(
        stdout.contains("\"name\": \"EventEveryTick\""),
        "{stdout:?}"
    );
    assert!(stdout.contains("\"defaultConfiguration\""), "{stdout:?}");
    assert!(stdout.contains("\"ruleIndex\": 0"), "{stdout:?}");
    assert!(stdout.contains("\"level\": \"note\""), "{stdout:?}");
    assert!(stdout.contains("\"uriBaseId\": \"SRCROOT\""), "{stdout:?}");
    assert!(stdout.contains("\"partialFingerprints\""), "{stdout:?}");
    assert_balanced_json(&stdout);
}

/// Cheap structural check that the hand-built JSON closes everything it opens
/// and quotes are balanced outside strings.
fn assert_balanced_json(text: &str) {
    let (mut depth, mut in_str, mut escaped) = (0i32, false, false);
    for c in text.chars() {
        match c {
            _ if escaped => escaped = false,
            '\\' if in_str => escaped = true,
            '"' => in_str = !in_str,
            '{' | '[' if !in_str => depth += 1,
            '}' | ']' if !in_str => depth -= 1,
            _ => {}
        }
        assert!(depth >= 0, "unbalanced json: {text}");
    }
    assert_eq!(depth, 0, "unbalanced json: {text}");
    assert!(!in_str, "unterminated string: {text}");
}

#[test]
fn test_loc_github_and_sarif_reports() {
    let loc_dir = fixtures_dir().join("loc_invalid");
    cwtools()
        .args(["loc", loc_dir.to_str().unwrap(), "--report-type", "github"])
        .assert()
        .success()
        .stdout(predicate::str::contains("::warning file="))
        .stdout(predicate::str::contains("title=CW268::"));

    let out = cwtools()
        .args(["loc", loc_dir.to_str().unwrap(), "--report-type", "sarif"])
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("\"ruleId\": \"CW268\""), "{stdout:?}");
    // A redirected machine report must be the whole of stdout: the "Scanning
    // localisation in …" banner moves to stderr so `> out.sarif` stays valid.
    assert!(stdout.starts_with("{\n"), "{stdout:?}");
    assert!(
        String::from_utf8(out.stderr)
            .unwrap()
            .contains("Scanning localisation")
    );
    assert_balanced_json(&stdout);
}

/// Only the two new formats divert status lines. The report types that existed
/// before keep every line exactly where it was, including the ones whose stdout
/// was already a mix of banner and report.
#[test]
fn test_only_the_new_formats_divert_status_lines() {
    let loc_dir = fixtures_dir().join("loc_invalid");
    for fmt in ["cli", "csv", "json"] {
        cwtools()
            .args(["loc", loc_dir.to_str().unwrap(), "--report-type", fmt])
            .assert()
            .success()
            .stdout(predicate::str::starts_with("Scanning localisation in "));
    }
    // A machine report written to a file doesn't own stdout either.
    let tmp = tempfile::tempdir().unwrap();
    cwtools()
        .args([
            "loc",
            loc_dir.to_str().unwrap(),
            "--report-type",
            "sarif",
            "--output-file",
            tmp.path().join("r.sarif").to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::starts_with("Scanning localisation in "));
}

/// Diagnostic hashes key on the file string, so `directory = "."` in a config
/// must produce the same paths — and therefore the same baseline — as passing
/// the directory on the command line.
#[test]
fn test_config_directory_dot_keeps_baseline_hashes_stable() {
    let tmp = config_mod("directory = \".\"\n");
    let mod_dir = tmp.path().join("mod");
    let rules_dir = fixtures_dir().join("rules");
    let hashes = |args: &[&str], cwd: &std::path::Path| {
        let out = tmp.path().join("h.txt");
        let mut cmd = cwtools();
        cmd.arg("validate")
            .args([
                "--game",
                "stellaris",
                "--rules",
                rules_dir.to_str().unwrap(),
                "--output-hashes",
                out.to_str().unwrap(),
            ])
            .args(args)
            .current_dir(cwd)
            .assert()
            .success();
        std::fs::read_to_string(&out).unwrap()
    };
    let by_flag = hashes(&["--directory", mod_dir.to_str().unwrap()], tmp.path());
    let by_config = hashes(&[], &mod_dir);
    assert!(!by_flag.trim().is_empty(), "the fixture emits CW107");
    assert_eq!(
        by_flag, by_config,
        "config `directory = \".\"` must not shift the digests"
    );
}

/// A shared config carries keys for every subcommand; the ones this command
/// won't act on are named rather than quietly dropped.
#[test]
fn test_config_warns_about_keys_the_command_ignores() {
    let tmp = tempfile::tempdir().unwrap();
    let loc = tmp.path().join("mod").join("localisation");
    std::fs::create_dir_all(&loc).unwrap();
    std::fs::copy(
        fixtures_dir().join("loc/localisation/l_english.yml"),
        loc.join("l_english.yml"),
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("cwtools.toml"),
        "directory = \"mod\"\ngame = \"hoi4\"\nloc-languages = [\"english\"]\n",
    )
    .unwrap();
    cwtools()
        .arg("loc")
        .current_dir(tmp.path())
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "sets game, loc-languages, which `loc` does not read",
        ));
}

#[test]
fn test_unknown_report_type_fails() {
    let mod_dir = fixtures_dir().join("discover").join("mod_a");
    let rules_dir = fixtures_dir().join("rules");
    cwtools()
        .args([
            "validate",
            "--game",
            "stellaris",
            "--directory",
            mod_dir.to_str().unwrap(),
            "--rules",
            rules_dir.to_str().unwrap(),
            "--report-type",
            "sarrif",
        ])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("invalid report type 'sarrif'"))
        .stderr(predicate::str::contains("cli, csv, json, github, sarif"));
}

// ── Per-code suppression ─────────────────────────────────────────────────────

#[test]
fn test_validate_ignore_code_drops_the_code() {
    let mod_dir = fixtures_dir().join("discover").join("mod_a");
    let rules_dir = fixtures_dir().join("rules");
    cwtools()
        .args([
            "validate",
            "--game",
            "stellaris",
            "--directory",
            mod_dir.to_str().unwrap(),
            "--rules",
            rules_dir.to_str().unwrap(),
            // Lowercase: matched case-insensitively, like the editor setting.
            "--ignore-code",
            "cw107",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("CW107").not())
        .stdout(predicate::str::contains(
            "Validation complete: 0 errors, 0 warnings",
        ));
}

#[test]
fn test_validate_only_code_keeps_just_that_code() {
    let mod_dir = fixtures_dir().join("discover").join("mod_a");
    let rules_dir = fixtures_dir().join("rules");
    let rules = rules_dir.to_str().unwrap().to_string();
    let dir = mod_dir.to_str().unwrap().to_string();
    let base = ["validate", "--game", "stellaris", "--directory"];
    cwtools()
        .args(base)
        .arg(&dir)
        .args(["--rules", &rules, "--only-code", "CW107"])
        .assert()
        .success()
        .stdout(predicate::str::contains("CW107"));
    cwtools()
        .args(base)
        .arg(&dir)
        .args(["--rules", &rules, "--only-code", "CW121"])
        .assert()
        .success()
        .stdout(predicate::str::contains("CW107").not());
}

#[test]
fn test_ignore_code_beats_only_code() {
    let mod_dir = fixtures_dir().join("discover").join("mod_a");
    let rules_dir = fixtures_dir().join("rules");
    cwtools()
        .args([
            "validate",
            "--game",
            "stellaris",
            "--directory",
            mod_dir.to_str().unwrap(),
            "--rules",
            rules_dir.to_str().unwrap(),
            "--only-code",
            "CW107",
            "--ignore-code",
            "CW107",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("CW107").not());
}

#[test]
fn test_unknown_ignore_code_fails() {
    let mod_dir = fixtures_dir().join("discover").join("mod_a");
    let rules_dir = fixtures_dir().join("rules");
    cwtools()
        .args([
            "validate",
            "--game",
            "stellaris",
            "--directory",
            mod_dir.to_str().unwrap(),
            "--rules",
            rules_dir.to_str().unwrap(),
            "--ignore-code",
            "CW9999",
        ])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("unknown diagnostic code 'CW9999'"));
}

#[test]
fn test_loc_ignore_code_changes_the_exit_code() {
    // CW225 is the only diagnostic in the fixture and it's Error-severity, so
    // suppressing it turns exit 1 into exit 0.
    let loc_dir = fixtures_dir().join("loc_error");
    cwtools()
        .args(["loc", loc_dir.to_str().unwrap()])
        .assert()
        .failure()
        .code(1);
    cwtools()
        .args(["loc", loc_dir.to_str().unwrap(), "--ignore-code", "CW225"])
        .assert()
        .success()
        .stdout(predicate::str::contains("0 issues"));
}

#[test]
fn test_loc_min_severity_filters() {
    let loc_dir = fixtures_dir().join("loc_invalid");
    cwtools()
        .args(["loc", loc_dir.to_str().unwrap(), "--min-severity", "error"])
        .assert()
        .success()
        .stdout(predicate::str::contains("CW268").not());
}

// ── Parse errors ─────────────────────────────────────────────────────────────

#[test]
fn test_parse_reports_errors_and_exits_nonzero() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("bad.txt");
    std::fs::write(&file, "a = { b = ").unwrap();
    cwtools()
        .args(["parse", file.to_str().unwrap()])
        .assert()
        .failure()
        .code(1)
        // The summary still goes to stdout unchanged.
        .stdout(predicate::str::contains("Parsed:"))
        .stderr(predicate::str::contains("parse error(s)"))
        .stderr(predicate::str::contains("has no value after '='"))
        .stderr(predicate::str::contains("unclosed clause"));
}

#[test]
fn test_parse_reports_the_clause_depth_limit() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("deep.txt");
    let body = format!("root = {}1{}\n", "{ a = ".repeat(300), " }".repeat(300));
    std::fs::write(&file, body).unwrap();
    cwtools()
        .args(["parse", file.to_str().unwrap()])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("clause nesting deeper than"));
}

#[test]
fn test_parse_clean_file_reports_nothing_extra() {
    let simple = fixtures_dir().join("simple.txt");
    let out = cwtools()
        .args(["parse", simple.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(
        !String::from_utf8(out.stderr)
            .unwrap()
            .contains("parse error"),
        "a clean parse must stay quiet"
    );
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
