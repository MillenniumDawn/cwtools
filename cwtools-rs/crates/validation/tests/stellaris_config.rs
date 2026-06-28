//! End-to-end test for the cwtools-stellaris-config integration.
//!
//! The fixture under `testfiles/stellaris-config/` mirrors the layout of the
//! external `cwtools-stellaris-config` repo (its `scopes.cwt` and `links.cwt`).
//! We load it through the real ruleset loader, build the runtime
//! `ScopeRegistry`, and assert that scope/link resolution matches what the
//! config declares. This is the regression net for issue #8's Stellaris slice:
//! once the hardcoded `STELLARIS_SCOPES` table is gone, the config has to be
//! the source of truth and this test pins that.

use std::path::PathBuf;

use cwtools_game::constants::Game;
use cwtools_game::scope_registry::ScopeRegistry;
use cwtools_rules::ruleset_loader::load_ruleset_from_dir;
use cwtools_string_table::string_table::StringTable;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("testfiles")
        .join("stellaris-config")
}

fn load_stellaris_ruleset() -> cwtools_rules::rules_types::RuleSet {
    let table = StringTable::new();
    let (ruleset, _errors) = load_ruleset_from_dir(&fixture_dir(), &table);
    // The fixture only carries the small subset of files we care about
    // (scopes.cwt, links.cwt, pre_triggers.cwt), so cross-file type
    // references in links.cwt surface as config-validation warnings.
    // Those are about unrelated type definitions (script_value, etc.) — the
    // scope and link inputs we test against are populated regardless.
    ruleset
}

/// Sanity: the fixture must contain a scopes.cwt and a links.cwt, otherwise
/// the rest of the assertions in this file are meaningless. Catches the case
/// where someone deletes the fixture but leaves the tests.
#[test]
fn fixture_files_exist() {
    let dir = fixture_dir();
    assert!(
        dir.join("scopes.cwt").is_file(),
        "scopes.cwt missing under {}",
        dir.display()
    );
    assert!(
        dir.join("links.cwt").is_file(),
        "links.cwt missing under {}",
        dir.display()
    );
}

/// All scopes the cwtools-stellaris-config declares must resolve through the
/// registry. Pins that `from_config` is wired up for Stellaris.
#[test]
fn config_scopes_resolve() {
    let ruleset = load_stellaris_ruleset();
    let reg =
        ScopeRegistry::from_config(&ruleset.scope_inputs, &ruleset.link_inputs, Game::Stellaris);

    // Every scope name declared in the config must resolve.
    for si in &ruleset.scope_inputs {
        assert!(
            reg.id_of(&si.name).is_some(),
            "config-declared scope `{}` did not resolve",
            si.name
        );
        for alias in &si.aliases {
            assert!(
                reg.id_of(alias).is_some(),
                "config-declared scope alias `{alias}` (of `{}`) did not resolve",
                si.name
            );
        }
    }

    // Spot-check the named scopes the rest of the engine (and the existing
    // Stellaris tests) refer to. If any of these resolve to None, scope checks
    // for those names will be silently lenient.
    for scope_name in [
        "Country",
        "Leader",
        "System",
        "Planet",
        "Ship",
        "Fleet",
        "Pop",
        "Army",
        "Species",
        "Pop Faction",
        "Sector",
        "War",
        "Megastructure",
        "Design",
        "Starbase",
        "Star",
        "Deposit",
        "Federation",
        "Alliance",
        "Trait",
        "Situation",
        "Agreement",
    ] {
        assert!(
            reg.id_of(scope_name).is_some(),
            "expected scope `{scope_name}` to resolve (config or synthesized)"
        );
    }
}

/// `from_config` treats Alliance and Federation as DIFFERENT scopes (per the
/// config), unlike the legacy hardcoded table which merged them. This pins the
/// new behaviour: each name resolves to its own id.
#[test]
fn config_alliance_and_federation_are_separate_scopes() {
    let ruleset = load_stellaris_ruleset();
    let reg =
        ScopeRegistry::from_config(&ruleset.scope_inputs, &ruleset.link_inputs, Game::Stellaris);

    let alliance = reg
        .id_of("Alliance")
        .expect("Alliance scope resolves from config");
    let federation = reg
        .id_of("Federation")
        .expect("Federation scope resolves from config");

    assert_ne!(
        alliance, federation,
        "the cwtools-stellaris-config declares Alliance and Federation as \
         separate scopes; the engine must not merge them"
    );

    // Their lowercase aliases each map to their own id.
    assert_eq!(reg.id_of("alliance"), Some(alliance));
    assert_eq!(reg.id_of("federation"), Some(federation));
}

/// All links the cwtools-stellaris-config declares must resolve. The link's
/// target scope (if any) must also resolve.
#[test]
fn config_links_resolve() {
    let ruleset = load_stellaris_ruleset();
    let reg =
        ScopeRegistry::from_config(&ruleset.scope_inputs, &ruleset.link_inputs, Game::Stellaris);

    for li in &ruleset.link_inputs {
        let key_exists = if li.prefix.is_some() {
            // Prefix links live in `prefix_links`, not `links`.
            reg.prefix_links
                .iter()
                .any(|(p, _)| p == &li.prefix.as_deref().unwrap_or("").to_ascii_lowercase())
        } else {
            reg.links.contains_key(&li.name.to_ascii_lowercase())
        };
        assert!(
            key_exists,
            "config-declared link `{}` did not register",
            li.name
        );

        if let Some(target) = &li.output_scope {
            assert!(
                reg.id_of(target).is_some(),
                "link `{}` targets unknown scope `{target}` (the config's \
                 scope list must declare every link target)",
                li.name
            );
        }
    }
}

/// Synthesized iterators (`every_/random_/any_/all_<scope>`) must be
/// generated for every scope alias the config declares.
#[test]
fn synthesized_iterators_present_for_every_scope_alias() {
    let ruleset = load_stellaris_ruleset();
    let reg =
        ScopeRegistry::from_config(&ruleset.scope_inputs, &ruleset.link_inputs, Game::Stellaris);

    for si in &ruleset.scope_inputs {
        for alias in std::iter::once(&si.name).chain(si.aliases.iter()) {
            // Only simple (ascii-alphanumeric/underscore) aliases are
            // synthesized — compound names like `planet_army` aren't.
            if !alias.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                continue;
            }
            let alias_lc = alias.to_ascii_lowercase();
            for prefix in ["every_", "random_", "any_", "all_"] {
                let key = format!("{prefix}{alias_lc}");
                assert!(
                    reg.links.contains_key(&key),
                    "expected synthesized iterator `{key}` (config scope `{alias}`)"
                );
            }
        }
    }
}
