//! "Object is missing its localisation" check (CW100).
//!
//! A type can declare which loc keys each of its instances must have via a
//! `localisation = { ## required name = "$" … }` block (the `$` is the instance
//! name, with an optional prefix/suffix). For every instance defined in a file
//! this flags any `## required` loc key that no loc file provides, so a modder
//! can see at a glance which objects lack localisation. Mirrors the old cwtools
//! "object has no localisation" warning.

use cwtools_index::{NormalizedPath, TypeInstance, check_path_dir_norm};
use cwtools_parser::fix::SuggestedFix;
use cwtools_rules::rules_types::{RuleSet, TypeDefinition};

use crate::ValidationError;
use cwtools_error_codes as error_codes;

/// Whether a type declares a loc key this check can flag: `## required`, not
/// `## optional`, and derived from the instance name rather than a child field.
fn has_required_name_loc(td: &TypeDefinition) -> bool {
    td.localisation
        .iter()
        .any(|loc| loc.required && !loc.optional && loc.explicit_field.is_none())
}

/// Flag indexed instances whose `## required` localisation keys are not
/// provided by any loc file. `loc_exists(key_lower)` reports whether a
/// (lowercased) loc key exists across the indexed languages. Only keys built
/// from the instance name (`prefix$suffix`) are checked; `explicit_field` forms
/// (loc key taken from a child field's value) are skipped for now.
pub fn check_missing_localisation(
    instances: &[(&str, &TypeInstance)],
    logical_path: &str,
    file_path: &crate::FilePath,
    ruleset: &RuleSet,
    loc_exists: impl Fn(&str) -> bool,
) -> Vec<ValidationError> {
    // Only a type whose path covers this file can contribute instances here, so
    // unless one of those declares a `## required` name-derived loc key the whole
    // instance walk is dead work — which is most files (events, gfx, history, …).
    let np = NormalizedPath::new(logical_path);
    let relevant: Vec<&TypeDefinition> = ruleset
        .types
        .iter()
        .filter(|td| has_required_name_loc(td) && check_path_dir_norm(&td.path_options, &np))
        .collect();
    if relevant.is_empty() {
        return Vec::new();
    }

    let mut errors = Vec::new();

    for td in relevant {
        for &(type_name, inst) in instances {
            if type_name != td.name.as_str() {
                continue;
            }
            for loc in &td.localisation {
                // Only required, name-derived keys (`prefix$suffix`). `optional`
                // and field-derived (`explicit_field`) forms are not flagged.
                if !loc.required || loc.optional || loc.explicit_field.is_some() {
                    continue;
                }
                let expected = format!("{}{}{}", loc.prefix, inst.name, loc.suffix);
                if !loc_exists(&expected.to_ascii_lowercase()) {
                    let fix = SuggestedFix::create_loc_key(
                        format!("Create localisation key {expected}"),
                        &expected,
                    );
                    errors.push(
                        ValidationError::from_code(
                            &error_codes::CW100_MISSING_LOCALISATION,
                            file_path,
                            inst.location.line,
                            inst.location.col,
                            &[&expected, &inst.name],
                        )
                        .with_fix(fix),
                    );
                }
            }
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    use cwtools_index::collect_type_instances;
    use cwtools_parser::parser::parse_string;
    use cwtools_rules::rules_converter::ast_to_ruleset;
    use cwtools_string_table::string_table::StringTable;

    const RULES: &str = r#"
types = {
    type[thing] = {
        path = "game/common/things"
        localisation = {
            ## required
            name = "$"
            ## required
            desc = "$_desc"
        }
    }
}
thing = { x = scalar }
"#;

    fn run_at(logical_path: &str, script: &str, has: &[&str]) -> Vec<ValidationError> {
        let table = StringTable::new();
        let parsed_cwt = parse_string(RULES, &table).unwrap();
        let ruleset = ast_to_ruleset(&parsed_cwt, &table);
        let parsed = parse_string(script, &table).unwrap();
        let present: std::collections::HashSet<String> =
            has.iter().map(|s| s.to_ascii_lowercase()).collect();
        let per_type = collect_type_instances(&ruleset, &parsed, logical_path, &table);
        let instances: Vec<(&str, &TypeInstance)> = per_type
            .iter()
            .flat_map(|(type_name, values)| {
                values
                    .iter()
                    .map(move |instance| (type_name.as_str(), instance))
            })
            .collect();
        check_missing_localisation(
            &instances,
            logical_path,
            &logical_path.into(),
            &ruleset,
            |k| present.contains(k),
        )
    }

    fn run(script: &str, has: &[&str]) -> Vec<ValidationError> {
        run_at("common/things/test.txt", script, has)
    }

    #[test]
    fn flags_instance_missing_required_loc() {
        // `my_thing` has its name loc but not `my_thing_desc`.
        let errs = run("my_thing = { x = yes }\n", &["my_thing"]);
        let msgs: Vec<&str> = errs.iter().map(|e| e.message.as_str()).collect();
        assert_eq!(
            errs.len(),
            1,
            "expected one missing-loc warning, got: {:?}",
            msgs
        );
        assert!(errs[0].message.contains("my_thing_desc"), "got: {:?}", msgs);
        assert_eq!(errs[0].code, Some("CW100"));

        // The fix carries the missing key for the LSP's "create localisation
        // key" action, not a span edit — there's no existing text to replace.
        let fix = errs[0].fix.as_ref().expect("CW100 carries a fix");
        assert_eq!(fix.create_loc_key.as_deref(), Some("my_thing_desc"));
        assert!(fix.edits.is_empty());
        assert_eq!(fix.title, "Create localisation key my_thing_desc");
    }

    #[test]
    fn clean_when_no_loc_bearing_type_owns_the_path() {
        // No type declaring a required loc key covers `events/`, so the file is
        // skipped whole — same instance name, nothing flagged.
        let errs = run_at("events/test.txt", "my_thing = { x = yes }\n", &[]);
        assert!(
            errs.is_empty(),
            "got: {:?}",
            errs.iter().map(|e| &e.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn clean_when_all_required_loc_present() {
        let errs = run("my_thing = { x = yes }\n", &["my_thing", "my_thing_desc"]);
        assert!(
            errs.is_empty(),
            "got: {:?}",
            errs.iter().map(|e| &e.message).collect::<Vec<_>>()
        );
    }
}
