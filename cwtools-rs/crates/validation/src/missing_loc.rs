//! "Object is missing its localisation" check (CW100).
//!
//! A type can declare which loc keys each of its instances must have via a
//! `localisation = { ## required name = "$" … }` block (the `$` is the instance
//! name, with an optional prefix/suffix). For every instance defined in a file
//! this flags any `## required` loc key that no loc file provides, so a modder
//! can see at a glance which objects lack localisation. Mirrors the old cwtools
//! "object has no localisation" warning.

use cwtools_index::collect_type_instances;
use cwtools_parser::ast::ParsedFile;
use cwtools_rules::rules_types::RuleSet;
use cwtools_string_table::string_table::StringTable;

use crate::ValidationError;
use crate::error_codes;

/// Flag instances in `ast` whose `## required` localisation keys are not provided
/// by any loc file. `loc_exists(key_lower)` reports whether a (lowercased) loc key
/// exists across the indexed languages. Only keys built from the instance name
/// (`prefix$suffix`) are checked; `explicit_field` forms (loc key taken from a
/// child field's value) are skipped for now.
pub fn check_missing_localisation(
    ast: &ParsedFile,
    logical_path: &str,
    file_path: &str,
    ruleset: &RuleSet,
    table: &StringTable,
    loc_exists: impl Fn(&str) -> bool,
) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    let instances = collect_type_instances(ruleset, ast, logical_path, table);

    for td in &ruleset.types {
        if td.localisation.is_empty() {
            continue;
        }
        let Some(insts) = instances.get(&td.name) else {
            continue;
        };
        for inst in insts {
            for loc in &td.localisation {
                // Only required, name-derived keys (`prefix$suffix`). `optional`
                // and field-derived (`explicit_field`) forms are not flagged.
                if !loc.required || loc.optional || loc.explicit_field.is_some() {
                    continue;
                }
                let expected = format!("{}{}{}", loc.prefix, inst.name, loc.suffix);
                if !loc_exists(&expected.to_ascii_lowercase()) {
                    errors.push(ValidationError::from_code(
                        &error_codes::CW100_MISSING_LOCALISATION,
                        file_path,
                        inst.location.line,
                        inst.location.col,
                        &[&expected, &inst.name],
                    ));
                }
            }
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    use cwtools_parser::parser::parse_string;
    use cwtools_rules::rules_converter::ast_to_ruleset;

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

    fn run(script: &str, has: &[&str]) -> Vec<ValidationError> {
        let table = StringTable::new();
        let parsed_cwt = parse_string(RULES, &table).unwrap();
        let ruleset = ast_to_ruleset(&parsed_cwt, &table);
        let parsed = parse_string(script, &table).unwrap();
        let present: std::collections::HashSet<String> =
            has.iter().map(|s| s.to_ascii_lowercase()).collect();
        check_missing_localisation(
            &parsed,
            "common/things/test.txt",
            "common/things/test.txt",
            &ruleset,
            &table,
            |k| present.contains(k),
        )
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
