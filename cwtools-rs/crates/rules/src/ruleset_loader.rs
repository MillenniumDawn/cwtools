use crate::post_process::post_process;
use crate::rules_converter::ast_to_ruleset;
use crate::rules_types::RuleSet;
use cwtools_parser::parser::parse_string;
use cwtools_string_table::string_table::StringTable;
use std::path::Path;

/// Recursively collect all `*.cwt` files under `dir`.
fn collect_cwt_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_cwt_files(&path, out);
            } else if path
                .extension()
                .map(|e| e.eq_ignore_ascii_case("cwt"))
                .unwrap_or(false)
            {
                out.push(path);
            }
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
    dst.values.extend(src.values);
}

/// Walk `dir` for `*.cwt` files, parse each with `table`, convert via
/// `ast_to_ruleset`, and merge all results into one `RuleSet`.
///
/// Returns `(ruleset, errors)`. Errors are non-fatal: files that fail to read
/// or parse are skipped and their messages collected.
pub fn load_ruleset_from_dir(dir: &Path, table: &StringTable) -> (RuleSet, Vec<String>) {
    let mut cwt_files = Vec::new();
    collect_cwt_files(dir, &mut cwt_files);

    let mut combined = RuleSet::new();
    let mut errors = Vec::new();

    for path in &cwt_files {
        match std::fs::read_to_string(path) {
            Ok(content) => match parse_string(&content, table) {
                Ok(parsed) => {
                    let ruleset = ast_to_ruleset(&parsed, table);
                    merge_ruleset(&mut combined, ruleset);
                }
                Err(e) => {
                    errors.push(format!("parse error in {}: {}", path.display(), e));
                }
            },
            Err(e) => {
                errors.push(format!("read error for {}: {}", path.display(), e));
            }
        }
    }

    // Run the post-processing pipeline once all files have been merged so that
    // cross-file single_alias references are fully resolved.
    post_process(&mut combined);

    (combined, errors)
}
