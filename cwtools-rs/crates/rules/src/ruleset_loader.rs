use crate::post_process::post_process;
#[cfg(test)]
use crate::rules_converter::ast_to_ruleset;
use crate::rules_converter::ast_to_ruleset_raw;
use crate::rules_types::RuleSet;
use cwtools_parser::ast::ParseError;
use cwtools_parser::parser::parse_string;
use cwtools_string_table::string_table::StringTable;
use std::path::Path;

/// A non-fatal error from loading a `.cwt` rules directory: a file that failed
/// to read or parse. Carries the source location so the LSP can publish a
/// diagnostic on the offending file and reveal where the rules broke.
#[derive(Debug, Clone)]
pub struct RuleParseError {
    pub file: std::path::PathBuf,
    /// 1-based line. `1` for read errors or parse errors without a position.
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
/// Returns `(ruleset, errors)`. Errors are non-fatal: files that fail to read
/// or parse are skipped and their messages collected.
pub fn load_ruleset_from_dir(dir: &Path, table: &StringTable) -> (RuleSet, Vec<RuleParseError>) {
    let mut cwt_files = Vec::new();
    collect_cwt_files(dir, &mut cwt_files);

    let mut combined = RuleSet::new();
    let mut errors = Vec::new();
    // Retain each parsed AST so the structural reference check can re-walk them
    // against the fully-merged RuleSet (cross-file definitions must all be in).
    let mut asts: Vec<(std::path::PathBuf, cwtools_parser::ast::ParsedFile)> = Vec::new();

    for path in &cwt_files {
        match std::fs::read_to_string(path) {
            Ok(content)
                if path
                    .file_name()
                    .is_some_and(|n| n.eq_ignore_ascii_case("folders.cwt")) =>
            {
                combined.folders.extend(parse_folders_list(&content));
            }
            Ok(content) => match parse_string(&content, table) {
                Ok(parsed) => {
                    let ruleset = ast_to_ruleset_raw(&parsed, table);
                    merge_ruleset(&mut combined, ruleset);
                    asts.push((path.clone(), parsed));
                }
                Err(e) => {
                    let (line, col, message) = match e {
                        ParseError::Pos(_, l, c, m) => (l, c, m),
                        ParseError::General(m) => (1, 0, m),
                    };
                    errors.push(RuleParseError {
                        file: path.clone(),
                        line,
                        col,
                        message: format!("parse error: {}", message),
                    });
                }
            },
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
    // cross-file single_alias references are fully resolved.
    post_process(&mut combined);

    // Build alias lookup indexes last — alias names/order are stable after this.
    combined.reindex();

    // Structural validation: now that every definition is merged and indexed,
    // flag references to undefined types/enums/aliases in the rule config.
    errors.extend(crate::config_validation::validate_ruleset_references(
        &asts, &combined, table,
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
        let links = parse_string("links = { character = { from_data = yes } }", &table).unwrap();
        let mut a = ast_to_ruleset(&links, &table);

        let other =
            parse_string("types = { type[evt] = { path = \"game/events\" } }", &table).unwrap();
        let b = ast_to_ruleset(&other, &table);

        merge_ruleset(&mut a, b);
        assert!(
            a.scope_links.contains("character"),
            "scope_links lost during merge"
        );
    }
}
