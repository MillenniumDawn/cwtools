use crate::post_process::post_process;
#[cfg(test)]
use crate::rules_converter::ast_to_ruleset;
use crate::rules_converter::{ast_to_ruleset_raw, validate_comment_directives};
use crate::rules_types::{CwtDefKind, RuleSet};
use cwtools_file_manager::file_manager::{ScanBudget, ScanBytes, read_text_capped};
use cwtools_parser::parser::parse_string;
use cwtools_string_table::string_table::StringTable;
use std::path::Path;

/// A non-fatal error from loading a `.cwt` rules directory: a file that failed
/// to read, or whose rules didn't hold up. Carries the source location so the
/// LSP can publish a diagnostic on the offending file and reveal where the
/// rules broke.
#[derive(Debug, Clone)]
pub struct RuleParseError {
    pub file: std::path::PathBuf,
    /// 1-based line. `1` for read errors and anything else without a position.
    pub line: u32,
    pub col: u16,
    pub message: String,
}

impl std::fmt::Display for RuleParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}:{}: {}",
            self.file.display(),
            self.line,
            self.col,
            self.message
        )
    }
}

fn directory_read_error(dir: &Path, error: std::io::Error) -> RuleParseError {
    RuleParseError {
        file: dir.to_path_buf(),
        line: 1,
        col: 0,
        message: format!("read directory error: {error}"),
    }
}

/// Place an unexpanded `single_alias` reference on its own definition, so the
/// diagnostic lands where the fix goes. Falls back to the rules directory when
/// the definition has no recorded position (nothing defines it, or the ruleset
/// was built by hand).
fn alias_expansion_error(
    dir: &Path,
    ruleset: &RuleSet,
    error: crate::post_process::AliasExpansionError,
) -> RuleParseError {
    let position = ruleset
        .def_positions
        .iter()
        .find(|p| p.kind == CwtDefKind::SingleAlias && p.name == error.name);
    RuleParseError {
        file: position.map_or_else(|| dir.to_path_buf(), |p| p.file.clone()),
        line: position.map_or(1, |p| p.line),
        col: position.map_or(0, |p| p.col),
        message: error.message,
    }
}

/// Recursively collect all `*.cwt` files under `dir`. Symlinks and non-regular
/// files are rejected outright (see `file_manager::walk_dir_generic`), and the
/// walk stops once `remaining_files` reaches 0.
fn collect_cwt_files(
    dir: &Path,
    out: &mut Vec<std::path::PathBuf>,
    errors: &mut Vec<RuleParseError>,
    remaining_files: &mut usize,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            errors.push(directory_read_error(dir, error));
            return;
        }
    };

    for entry in entries {
        if *remaining_files == 0 {
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                errors.push(directory_read_error(dir, error));
                continue;
            }
        };
        let Ok(ft) = entry.file_type() else { continue };
        let path = entry.path();
        if ft.is_symlink() || !(ft.is_dir() || ft.is_file()) {
            continue;
        }
        if ft.is_dir() {
            collect_cwt_files(&path, out, errors, remaining_files);
        } else if path
            .extension()
            .map(|e| e.eq_ignore_ascii_case("cwt"))
            .unwrap_or(false)
        {
            *remaining_files -= 1;
            out.push(path);
        }
    }
}

/// Merge `src` into `dst`, extending all collections.
pub fn merge_ruleset(dst: &mut RuleSet, src: RuleSet) {
    dst.types.extend(src.types);
    dst.enums.extend(src.enums);
    dst.aliases.extend(src.aliases);
    dst.single_aliases.extend(src.single_aliases);
    dst.complex_enums.extend(src.complex_enums);
    dst.root_rules.extend(src.root_rules);
    for (name, vals) in src.values {
        dst.values.entry(name).or_default().extend(vals);
    }
    dst.modifiers.extend(src.modifiers);
    dst.modifier_categories.extend(src.modifier_categories);
    dst.scope_links.extend(src.scope_links);
    dst.scope_inputs.extend(src.scope_inputs);
    dst.link_inputs.extend(src.link_inputs);
    dst.folders.extend(src.folders);
}

/// Parse a `folders.cwt`: one folder name per line, `#` comments and blank
/// lines skipped. Not Paradox-script syntax, so it bypasses the rules
/// converter entirely.
fn parse_folders_list(content: &str) -> Vec<String> {
    content
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.to_string())
        .collect()
}

/// Walk `dir` for `*.cwt` files, parse each with `table`, convert via
/// `ast_to_ruleset`, and merge all results into one `RuleSet`.
///
/// Returns `(ruleset, errors)`. Errors are non-fatal: a file that fails to read
/// is skipped and its message collected.
pub fn load_ruleset_from_dir(
    dir: &Path,
    table: &StringTable,
    budget: ScanBudget,
) -> (RuleSet, Vec<RuleParseError>) {
    let mut cwt_files = Vec::new();
    let mut errors = Vec::new();
    let mut remaining = budget.max_files;
    collect_cwt_files(dir, &mut cwt_files, &mut errors, &mut remaining);

    let mut combined = RuleSet::new();
    // Lightweight reference candidates collected from each AST while it is alive.
    // The AST itself is dropped as soon as it is converted; only these positioned
    // (kind, name) records are retained for the post-merge resolution pass, so we
    // no longer pin every parsed `.cwt` file simultaneously.
    let mut ref_candidates: Vec<crate::config_validation::RefCandidate> = Vec::new();
    let bytes = ScanBytes::new();

    for path in &cwt_files {
        match read_text_capped(path, budget.max_file_size) {
            Ok((content, n)) => {
                if !bytes.try_reserve(n, budget.max_bytes) {
                    errors.push(RuleParseError {
                        file: path.clone(),
                        line: 1,
                        col: 0,
                        message: "scan byte budget exceeded".to_string(),
                    });
                    continue;
                }
                if path
                    .file_name()
                    .is_some_and(|n| n.eq_ignore_ascii_case("folders.cwt"))
                {
                    combined.folders.extend(parse_folders_list(&content));
                } else {
                    let parsed = parse_string(&content, table);
                    errors.extend(validate_comment_directives(&parsed, path));
                    let ruleset = ast_to_ruleset_raw(&parsed, table);
                    merge_ruleset(&mut combined, ruleset);
                    crate::config_validation::collect_reference_candidates(
                        path,
                        &parsed,
                        table,
                        &mut ref_candidates,
                    );
                    crate::config_validation::collect_definition_positions(
                        path,
                        &parsed,
                        table,
                        &mut combined.def_positions,
                    );
                }
            }
            Err(e) => {
                errors.push(RuleParseError {
                    file: path.clone(),
                    line: 1,
                    col: 0,
                    message: format!("read error: {}", e),
                });
            }
        }
    }

    // Run the post-processing pipeline once all files have been merged so that
    // cross-file single_alias references are fully resolved. Anything expansion
    // refused (a cycle, a chain past the depth limit, the node budget) comes
    // back as a diagnostic on the definition it names.
    let refused = post_process(&mut combined);
    errors.extend(
        refused
            .into_iter()
            .map(|error| alias_expansion_error(dir, &combined, error)),
    );

    // Build alias lookup indexes last — alias names/order are stable after this.
    combined.reindex();

    // Structural validation: now that every definition is merged and indexed,
    // resolve the references collected during conversion against the merged set,
    // flagging any pointing at an undefined type/enum/single_alias.
    errors.extend(crate::config_validation::resolve_reference_candidates(
        &ref_candidates,
        &combined,
    ));

    (combined, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// merge_ruleset must carry scope_links across files. links.cwt is a separate
    /// file from the type/alias files, so dropping scope_links here silently breaks
    /// from-data scope-link recognition (e.g. `character = { ... }`) for the whole
    /// merged ruleset.
    #[test]
    fn merge_preserves_scope_links() {
        let table = StringTable::new();
        let links = parse_string("links = { character = { from_data = yes } }", &table);
        let mut a = ast_to_ruleset(&links, &table);

        let other = parse_string("types = { type[evt] = { path = \"game/events\" } }", &table);
        let b = ast_to_ruleset(&other, &table);

        merge_ruleset(&mut a, b);
        assert!(
            a.scope_links.contains("character"),
            "scope_links lost during merge"
        );
    }

    /// The rules walk must reject symlinks: a symlink can point outside the
    /// rules dir or into a cycle.
    #[cfg(unix)]
    #[test]
    fn collect_cwt_files_rejects_symlinks() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("real.cwt"), "types = { }\n").unwrap();
        symlink(tmp.path().join("real.cwt"), tmp.path().join("link.cwt")).unwrap();

        let mut files = Vec::new();
        let mut errors = Vec::new();
        let mut remaining = ScanBudget::default().max_files;
        collect_cwt_files(tmp.path(), &mut files, &mut errors, &mut remaining);
        assert!(errors.is_empty());
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"real.cwt".to_string()));
        assert!(
            !names.contains(&"link.cwt".to_string()),
            "symlinked .cwt file must be rejected: {names:?}"
        );
    }

    /// A `single_alias` expansion the post-processor refuses has to reach the
    /// caller like any other rules problem, on the definition it names rather
    /// than on the directory, so the editor can point at the line to fix.
    #[test]
    fn load_ruleset_from_dir_reports_unexpanded_single_alias() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("cycle.cwt"),
            "single_alias[loop_a] = {\n    x = single_alias_right[loop_b]\n}\n\
             single_alias[loop_b] = {\n    y = single_alias_right[loop_a]\n}\n",
        )
        .unwrap();

        let table = StringTable::new();
        let (_, errors) = load_ruleset_from_dir(tmp.path(), &table, ScanBudget::default());

        let cycle = errors
            .iter()
            .find(|e| e.message.contains("reference cycle"))
            .unwrap_or_else(|| panic!("no cycle diagnostic in {errors:?}"));
        assert!(cycle.file.ends_with("cycle.cwt"), "error: {cycle}");
        assert_eq!(cycle.line, 1, "error: {cycle}");
    }

    /// A `.cwt` file over the per-file cap must be skipped, not read to EOF.
    #[test]
    fn load_ruleset_from_dir_skips_over_limit_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("ok.cwt"), "types = { }\n").unwrap();
        std::fs::write(tmp.path().join("huge.cwt"), "x".repeat(200)).unwrap();

        let table = StringTable::new();
        let budget = ScanBudget {
            max_file_size: 50,
            ..ScanBudget::default()
        };
        let (ruleset, errors) = load_ruleset_from_dir(tmp.path(), &table, budget);
        assert!(
            !errors.iter().any(|e| e.file.ends_with("ok.cwt")),
            "ok.cwt must parse clean: {errors:?}"
        );
        assert!(
            errors.iter().any(|e| e.file.ends_with("huge.cwt")),
            "over-cap file must be reported as a read error: {errors:?}"
        );
        let _ = ruleset;
    }
}
