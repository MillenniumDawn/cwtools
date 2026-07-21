//! Shared validation driver.
//!
//! The full pipeline is: load rules -> discover/parse files -> build the
//! `TypeIndex` (+ var index + vanilla index) -> expand modifier keys -> build the
//! loc index -> build the `ScopeRegistry` -> validate. The reusable primitives
//! for this live in the shared crates ([`index_game_dir`],
//! `cwtools_validation::{build_scope_registry_arc, Prepared, validate_prepared}`);
//! both the CLI and the LSP call those directly so the sequence isn't
//! reimplemented and can't drift the way it did before.
//!
//! [`Session`] bundles those primitives into the CLI's batch model: load
//! everything from disk once into immutable-after-load state, then validate the
//! whole set ([`Session::validate_all`]). The LSP does NOT use `Session` — its
//! index is mutable and incremental (single files are re-indexed on each edit,
//! behind an `RwLock`, with no whole-workspace re-parse), which doesn't fit
//! `Session`'s load-once/immutable ownership. Instead the LSP holds its own
//! workspace state and builds a [`Prepared`] from the same shared primitives per
//! validation.
//!
//! Loc-file diagnostics (CW225 etc.) need the parsed `LocService`, so the session
//! keeps it resident and serves both the loc-key index and the project lint from
//! the one service, matching the CLI's prior behavior.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cwtools_file_manager::file_manager::{
    DirectoryType, FileManager, FileManagerConfig, classify_directory,
};
use cwtools_game::constants::Game;
use cwtools_game::scope_registry::ScopeRegistry;
use cwtools_index::{
    TypeIndex, collect_set_variable_names, collect_type_instances, index_discovered_files,
    variable_defining_effects,
};
use cwtools_localization::{Lang, LocDiagnostic, LocIndex, LocService};
use cwtools_parser::ast::{ParseError, ParsedFile};
use cwtools_parser::parser::parse_string;
use cwtools_rules::rules_converter::ast_to_ruleset;
use cwtools_rules::rules_types::RuleSet;
use cwtools_rules::ruleset_loader::load_ruleset_from_dir;
use cwtools_string_table::string_table::StringTable;
use cwtools_validation::{
    ErrorSeverity, Prepared, ValidationError, build_modifier_keys, build_scope_registry_arc,
    checks_from_env, validate_prepared,
};

/// A parsed workspace/mod file: its on-disk path, mod-relative logical path, and AST.
pub struct ParsedSource {
    pub path: PathBuf,
    pub logical_path: String,
    pub parsed: ParsedFile,
}

/// How to load the rules: a single `.cwt` file or a directory of them.
pub enum RulesInput {
    Dir(PathBuf),
    File(PathBuf),
}

impl RulesInput {
    /// Classify a path as a rules dir or rules file.
    pub fn from_path(path: PathBuf) -> Self {
        if path.is_dir() {
            RulesInput::Dir(path)
        } else {
            RulesInput::File(path)
        }
    }
}

/// Inputs to [`Session::load`].
pub struct SessionConfig<'a> {
    /// Engine the rules/files are written for.
    pub game: Game,
    /// Rules source (file or directory of `.cwt`).
    pub rules: RulesInput,
    /// Mod/workspace root to validate.
    pub directory: PathBuf,
    /// Base-game install indexed for reference resolution (never validated).
    pub vanilla: Option<PathBuf>,
    /// Pre-generated vanilla data to merge (from a `--vanilla-cache`). When
    /// present the `vanilla` dir is NOT walked — type instances, loc keys, file
    /// paths, and variable names all come from the cache.
    pub vanilla_cache: Option<cwtools_index::vanilla_cache::VanillaCacheData>,
    /// Extra filename globs to skip during discovery (on top of engine defaults).
    pub ignore_files: &'a [String],
    /// Extra directory globs to skip during discovery.
    pub ignore_dirs: &'a [String],
    /// Languages to scope loc validation to. `None` = every language with data.
    pub loc_languages: Option<Vec<Lang>>,
    /// Optional sink for rules-load warnings (so the CLI can print them on stderr).
    pub on_rules_warning: Option<&'a mut dyn FnMut(String)>,
}

/// The immutable-after-load engine state for the batch (CLI) path.
///
/// Built once by [`Session::load`]; thereafter read-only for validation. The CLI
/// builds one per run. The LSP does not use this (its index is mutable and
/// re-indexed per edit); it builds [`Prepared`] from the shared primitives
/// directly. See the module docs.
pub struct Session {
    game: Game,
    rules_table: StringTable,
    ruleset: RuleSet,
    type_index: TypeIndex,
    modifier_keys: HashSet<String>,
    loc_service: LocService,
    loc_index: LocIndex,
    loc_languages: Option<Vec<Lang>>,
    registry: Option<Arc<ScopeRegistry>>,
    directory: PathBuf,
}

impl Session {
    /// Run the full load pipeline: parse rules, discover/parse mod files, build the
    /// type/var/vanilla indexes, expand modifier keys, build the loc index, and
    /// prebuild the scope registry. Returns a ready-to-validate session.
    pub fn load(config: SessionConfig) -> SessionWithFiles {
        let SessionConfig {
            game,
            rules,
            directory,
            vanilla,
            vanilla_cache,
            ignore_files,
            ignore_dirs,
            loc_languages,
            on_rules_warning,
        } = config;

        // Rules share their StringTable with the game files so interned ids match.
        let rules_table = StringTable::new();
        let ruleset = load_rules(&rules, &rules_table, on_rules_warning).unwrap_or_else(|e| {
            eprintln!("error: {}", e);
            RuleSet::new()
        });

        // Discover + parse mod files using the SAME string table. Layer the
        // user-supplied ignore globs on top of the engine defaults.
        let mut fm_config = search_config_for(&directory);
        apply_config_folders(&mut fm_config, &ruleset.folders);
        if !ignore_files.is_empty() {
            fm_config
                .exclude_patterns
                .extend(ignore_files.iter().cloned());
        }
        if !ignore_dirs.is_empty() {
            fm_config
                .exclude_dir_patterns
                .extend(ignore_dirs.iter().cloned());
        }

        // Auto-detect a workspace of mods: a directory that is not itself a mod
        // root but whose `mod/`(or `mods/`) folder holds `.mod` descriptors. Such
        // a target expands to every referenced mod, layered by load order
        // (later-resolved mod wins a shared logical path; `replace_path`
        // suppresses lower-priority files). A plain single mod root takes the
        // exact single-root discovery path below, unchanged.
        let is_multi_mod = classify_directory(&directory) == DirectoryType::MultipleMod;
        if is_multi_mod && !ruleset.folders.is_empty() {
            // The workspace root itself lacks the game folders (they live inside
            // each mod), so `apply_config_folders`' root check never fires. Use the
            // ruleset's folder layout for the per-mod discovery.
            fm_config.include_dirs = ruleset.folders.clone();
        }

        let mut manager = FileManager::with_string_table(fm_config, rules_table.clone());
        let discovered = if is_multi_mod {
            manager.discover_and_parse_multi_mod()
        } else {
            manager.discover_and_parse()
        };
        let (files, discovery_failed) = match discovered {
            Ok(f) => (f, false),
            Err(e) => {
                eprintln!("error: discovery failed for {}: {}", directory.display(), e);
                (Vec::new(), true)
            }
        };

        // Take ownership of each parsed AST once. The TypeIndex build and the
        // validation pass both borrow this set, so nothing is parsed twice.
        let parsed: Vec<ParsedSource> = files
            .into_iter()
            .map(|f| ParsedSource {
                path: f.path,
                logical_path: f.logical_path,
                parsed: ParsedFile {
                    arena: f.arena,
                    root_children: f.root_children,
                    errors: f.errors,
                },
            })
            .collect();

        // Cross-file TypeIndex from the already-parsed arenas. Per-file
        // collection runs in parallel (each call is pure &-borrows); merge is
        // sequential in the original file order so TypeIndex.merge call order
        // is identical to the sequential version (goto-def "first match" and
        // duplicate-name refcounts are order-sensitive).
        use rayon::prelude::*;
        type PerFileResult = (
            HashMap<String, Vec<cwtools_index::TypeInstance>>,
            Vec<String>,
            HashMap<String, Vec<String>>,
        );
        let var_effects = variable_defining_effects(&ruleset);
        let per_file: Vec<PerFileResult> = parsed
            .par_iter()
            .map(|src| {
                let mut instances =
                    collect_type_instances(&ruleset, &src.parsed, &src.logical_path, &rules_table);
                // Subtype-qualified membership (`equipment.naval_equip` …) so
                // `<type.subtype>` references resolve. Archetypes self-determine
                // from direct discriminators; referencing variants resolve through
                // them at validation time.
                for (k, v) in cwtools_validation::collect_subtype_instances(
                    &ruleset,
                    &src.parsed,
                    &src.logical_path,
                    &rules_table,
                ) {
                    instances.entry(k).or_default().extend(v);
                }
                let mut var_names: Vec<String> = Vec::new();
                collect_set_variable_names(&src.parsed, &rules_table, &var_effects, &mut var_names);
                // Value-set members defined in mod files (flags, character tokens,
                // …). The vanilla path collects these too; without this mod-defined
                // members are missing from `value_set_values`, so a from-data scope
                // link can't recognise its own instances (e.g. a `generate_character`
                // token used as `<token> = { ... }`). Completion-only otherwise.
                let value_sets = cwtools_index::dynamic_values::collect_value_set_members(
                    &ruleset,
                    &src.parsed,
                    &rules_table,
                );
                (instances, var_names, value_sets)
            })
            .collect();

        let mut type_index = TypeIndex::new();
        for (src, (instances, var_names, value_sets)) in parsed.iter().zip(per_file) {
            let file_uri = src.path.to_str().unwrap_or("");
            type_index.merge(file_uri, instances);
            for n in &var_names {
                type_index.var_index.add_name(n);
            }
            type_index.value_set_values.merge_file(file_uri, value_sets);
        }

        // Vanilla data: from a pre-generated cache when given (skips walking the
        // install entirely — types, loc keys, file paths, and variables all come
        // from the cache), otherwise by indexing the install. Vanilla files
        // populate the indexes (so a mod can reference base-game content without
        // "not a known instance" errors) but are never validated themselves.
        let has_vanilla_data = vanilla.is_some() || vanilla_cache.is_some();
        let mut cached_loc_keys: Option<Vec<(String, Vec<String>)>> = None;
        if let Some(cache) = vanilla_cache {
            // Merge each cached instance under its real source file (not a shared
            // sentinel) so cross-file reference resolution keeps the base-game
            // provenance the cache stored.
            type_index.merge_with_uris(cache.per_type);
            for n in &cache.var_names {
                type_index.var_index.add_name(n);
            }
            // File index (mod + cached vanilla paths) for `filepath` checks
            // (CW113), same coverage as a live vanilla walk.
            type_index.file_index.add_root(&directory);
            type_index.file_index.add_paths(cache.file_paths);
            // Dynamic values (complex-enum / value_set members) collected from the
            // vanilla walk; without this a cache hit silently drops them.
            type_index.complex_enum_values.merge_file(
                "<vanilla-cache>",
                cache.complex_enum_values.into_iter().collect(),
            );
            type_index.value_set_values.merge_file(
                "<vanilla-cache>",
                cache.value_set_values.into_iter().collect(),
            );
            cached_loc_keys = Some(cache.loc_keys);
        } else if let Some(vanilla_dir) = &vanilla {
            let vanilla_index = index_game_dir(vanilla_dir, &ruleset, &rules_table, &var_effects);
            type_index.var_index.merge(&vanilla_index.var_index);
            for (type_name, entries) in vanilla_index.map {
                let per_type = HashMap::from([(
                    type_name,
                    entries.into_iter().map(|(_, inst)| inst).collect(),
                )]);
                type_index.merge("<vanilla>", per_type);
            }
            // File index (mod + vanilla) for `filepath` checks (CW113). Only when
            // vanilla is present: mod files commonly reference base-game assets.
            type_index.file_index.add_root(&directory);
            type_index.file_index.add_root(vanilla_dir);
        }

        // Mark the index as complete when vanilla data was loaded (either from a
        // directory or a pre-generated cache).  This lets CW500 type-reference
        // checks fire without false positives on mod-only validation.
        if has_vanilla_data {
            type_index.complete = true;
        }

        // Modifier names valid in `alias_name[modifier]` slots. Templated entries
        // are expanded against the type index, one per instance.
        let modifier_keys = build_modifier_keys(&ruleset, &type_index);

        // Loc: mod directory plus the vanilla install (so config referencing
        // base-game loc keys doesn't false-positive). Only mod-path loc files are
        // reported by the caller. With a vanilla cache, the cached key sets stand
        // in for walking the install's loc files.
        let mut loc_dirs: Vec<&Path> = vec![directory.as_path()];
        if cached_loc_keys.is_none()
            && let Some(v) = &vanilla
        {
            loc_dirs.push(v.as_path());
        }
        let loc_service = LocService::from_folders(&loc_dirs);
        let mut loc_index = LocIndex::build_scoped(&loc_service, loc_languages.as_deref());
        if let Some(keys) = cached_loc_keys {
            let typed: Vec<(Lang, Vec<String>)> = keys
                .into_iter()
                .filter_map(|(name, ks)| Lang::from_name(&name).map(|l| (l, ks)))
                .collect();
            loc_index.merge_cached_keys(typed, loc_languages.as_deref());
        }

        // Per-run scope registry, shared (cheaply cloned) across every file.
        let registry = build_scope_registry_arc(&ruleset, Some(game));

        Session {
            game,
            rules_table,
            ruleset,
            type_index,
            modifier_keys,
            loc_service,
            loc_index,
            loc_languages,
            registry,
            directory,
        }
        .with_parsed_cache(parsed, discovery_failed)
    }

    /// Attach the parsed mod-file set the batch path validates over.
    fn with_parsed_cache(
        self,
        parsed: Vec<ParsedSource>,
        discovery_failed: bool,
    ) -> SessionWithFiles {
        SessionWithFiles {
            session: self,
            parsed,
            discovery_failed,
        }
    }

    /// Bundle this session's prebuilt state into a [`Prepared`] for validation.
    fn prepared(&self) -> Prepared<'_> {
        let (scope_checks, var_checks) = checks_from_env();
        Prepared {
            ruleset: &self.ruleset,
            table: &self.rules_table,
            game: Some(self.game),
            type_index: Some(&self.type_index),
            modifier_keys: Some(&self.modifier_keys),
            loc_index: Some(&self.loc_index),
            // The CLI/driver loads loc from disk; no live-edit overlay.
            extra_loc_keys: None,
            registry: self.registry.as_ref(),
            scope_checks,
            var_checks,
        }
    }

    /// Loc-project diagnostics (CW225/CW234/CW259/CW268/CW275) for the workspace,
    /// scoped to this session's loc languages. Resolves references against the full
    /// mod+vanilla union; the caller filters to mod-path files.
    pub fn loc_project_diagnostics(&self) -> Vec<LocDiagnostic> {
        let extra = self.loc_extra_valid_refs();
        // Reuse the prebuilt loc-index union (with cached vanilla keys already
        // merged) rather than rebuilding the ~2M-key set here.
        cwtools_localization::validate_loc_project_with_union(
            &self.loc_service,
            self.loc_languages.as_deref(),
            self.loc_index.union(),
            &extra,
        )
    }

    /// Names a loc `$ref$` may resolve to besides loc keys: the engine resolves
    /// `$modifier$` / `$idea$` / `$dynamic_modifier$` embeds and `$some_variable$`
    /// substitutions against those registries. Lowercased to match the loc union's
    /// case-insensitive lookup. Cached vanilla keys are resolved via the loc-index
    /// union passed to `validate_loc_project_with_union`, so they don't need to be
    /// duplicated into this set.
    pub fn loc_extra_valid_refs(&self) -> HashSet<String> {
        let mut extra = self.modifier_keys.clone();
        extra.extend(self.type_index.loc_bindable_names());
        extra
    }

    /// The mod/workspace root this session was loaded for.
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// The shared rules string table.
    pub fn string_table(&self) -> &StringTable {
        &self.rules_table
    }

    /// The loaded ruleset.
    pub fn ruleset(&self) -> &RuleSet {
        &self.ruleset
    }

    /// The workspace + vanilla type index.
    pub fn type_index(&self) -> &TypeIndex {
        &self.type_index
    }

    /// The expanded modifier-key set.
    pub fn modifier_keys(&self) -> &HashSet<String> {
        &self.modifier_keys
    }

    /// The loc-key index (workspace + vanilla).
    pub fn loc_index(&self) -> &LocIndex {
        &self.loc_index
    }

    /// The prebuilt scope registry, if a game is set.
    pub fn registry(&self) -> Option<&Arc<ScopeRegistry>> {
        self.registry.as_ref()
    }
}

/// A [`Session`] plus the parsed mod-file set, returned by [`Session::load`]. The
/// batch path ([`Self::validate_all`]) needs the files resident; the LSP, which
/// supplies its own ASTs per file, derefs to the inner [`Session`].
pub struct SessionWithFiles {
    session: Session,
    parsed: Vec<ParsedSource>,
    /// True when the initial `discover_and_parse` failed; callers should treat
    /// this as a hard error (log already printed) and exit nonzero.
    pub discovery_failed: bool,
}

impl std::ops::Deref for SessionWithFiles {
    type Target = Session;
    fn deref(&self) -> &Session {
        &self.session
    }
}

impl SessionWithFiles {
    /// Validate every parsed mod file in parallel, in input order. Returns one
    /// entry per file as `(path, diagnostics)`. The per-run shared state (scope
    /// registry) is built ONCE and reused across the batch.
    pub fn validate_all(&self) -> Vec<(PathBuf, Vec<ValidationError>)> {
        use rayon::prelude::*;

        let prepared = self.session.prepared();
        self.parsed
            .par_iter()
            .map(|src| {
                let file_str = src.path.to_str().unwrap_or("").to_string();
                let mut errors = parse_errors_to_validation(&src.parsed.errors, &file_str);
                errors.extend(validate_prepared(&src.parsed, &file_str, &prepared));
                // CW100: objects defined here whose `## required` localisation
                // keys aren't provided by any loc file.
                if let Some(loc) = prepared.loc_index {
                    errors.extend(cwtools_validation::missing_loc::check_missing_localisation(
                        &src.parsed,
                        &src.logical_path,
                        &file_str,
                        prepared.ruleset,
                        prepared.table,
                        |k| loc.exists_any(k),
                    ));
                }
                (src.path.clone(), errors)
            })
            .collect()
    }

    /// The parsed mod-file set (for profiling/inspection by the caller).
    pub fn parsed_files(&self) -> &[ParsedSource] {
        &self.parsed
    }
}

/// Convert parse errors from a partially-parsed file into `ValidationError`s so
/// they appear in the CLI report (and count toward the exit-1 threshold).
fn parse_errors_to_validation(errors: &[ParseError], file_path: &str) -> Vec<ValidationError> {
    errors
        .iter()
        .map(|e| {
            let (line, col, msg) = match e {
                ParseError::Pos(l, c, m) => (*l, *c, m.clone()),
                ParseError::General(m) => (0, 0, m.clone()),
            };
            ValidationError {
                message: msg,
                severity: ErrorSeverity::Error,
                line,
                col,
                file: file_path.to_string(),
                code: None,
                fix: None,
                end: None,
            }
        })
        .collect()
}

/// Walk a vanilla install for the cache's aux payload: per-language loc keys
/// and the file-path set (CW113), plus the variable names and dynamic values
/// (complex-enum + value_set members) from an already-built vanilla index.
/// Shared by the CLI `cache-vanilla` command, the CLI stale-rebuild path, and
/// the LSP's cache writer so every cache carries the full payload.
pub fn build_vanilla_cache_aux(
    vanilla_dir: &Path,
    index: &TypeIndex,
) -> cwtools_index::vanilla_cache::VanillaCacheAux {
    let loc_service = LocService::from_folders(&[vanilla_dir]);
    let loc_keys = cwtools_localization::loc_index::per_language_keys(&loc_service);
    let mut file_index = cwtools_index::FileIndex::new();
    file_index.add_root(vanilla_dir);
    cwtools_index::vanilla_cache::VanillaCacheAux {
        loc_keys,
        file_paths: file_index.paths().cloned().collect(),
        var_names: index.var_index.names().cloned().collect(),
        complex_enum_values: index.complex_enum_values.export(),
        value_set_values: index.value_set_values.export(),
    }
}

/// Build a `TypeIndex` from every script file under `dir` (a base-game install).
/// Files are parsed and indexed for reference resolution; they are never validated.
///
/// This unifies what used to be two copies (the CLI's `index_game_dir` and the
/// LSP's `index_vanilla_dir`) onto the CLI's broader, corpus-verified
/// `search_config_for` discovery config.
pub fn index_game_dir(
    dir: &Path,
    ruleset: &RuleSet,
    table: &StringTable,
    var_effects: &HashSet<String>,
) -> TypeIndex {
    let mut config = search_config_for(dir);
    apply_config_folders(&mut config, &ruleset.folders);
    let mut mgr = FileManager::with_string_table(config, table.clone());
    let files = match mgr.discover_and_parse() {
        Ok(f) => f,
        Err(_) => return TypeIndex::new(),
    };
    index_discovered_files(
        files,
        ruleset,
        table,
        Some(var_effects),
        Some(cwtools_validation::collect_subtype_instances),
    )
}

/// Override the engine's built-in folder list with the config's `folders.cwt`
/// when the ruleset ships one and the target looks like a game/mod root (it
/// contains at least one of the listed folders). This wins over the leaf-dir
/// heuristic in `search_config_for`: a mod root with loose .txt files at the
/// top level (Changelog.txt etc.) would otherwise be scanned whole-tree,
/// pulling in non-script dirs the config never asks for.
fn apply_config_folders(config: &mut FileManagerConfig, folders: &[String]) {
    if folders.is_empty() {
        return;
    }
    if folders.iter().any(|f| config.root.join(f).is_dir()) {
        config.include_dirs = folders.to_vec();
    }
}

/// Decide whether to search a directory directly (as a leaf directory containing
/// script files) or as a mod root with standard subfolders. Shared by mod and
/// vanilla discovery so both entry points index the same way.
pub fn search_config_for(directory: &Path) -> FileManagerConfig {
    let known_script_folders = [
        "common",
        "events",
        "history",
        "interface",
        "decisions",
        "missions",
        "gfx",
        "sound",
        "music",
        "static_modifiers",
        "buildings",
        "technologies",
        "ethics",
        "policies",
        "ship_sizes",
        "pop_faction",
        "starbases_consolidated",
        "traits",
        "edicts",
        "traditions",
        "ascension_perks",
        "governments",
        "country_types",
        "bypass",
        "dlc_list",
        "subject_types",
        "casus_belli",
        "war_goals",
        "bombardment_stances",
        "armies",
        "deposits",
        "planet_classes",
        "tile_blockers",
        "species_rights",
        "observation_station_missions",
        "star_classes",
        "ambient_objects",
        "name_lists",
        "notification_modifier",
        "component_tags",
        "event_chains",
        "personalities",
        "global_ship_designs",
        "graphical_cultures",
        "species_archetypes",
        "resources",
        "species_classes",
        "buildable_pops",
        "opinion_modifiers",
        "leader_class_enum",
        "asteroid_belt",
        "solar_system_initializers",
        "fallen_empires",
    ];
    let dir_name = directory.file_name().and_then(|n| n.to_str()).unwrap_or("");

    // If this directory itself contains script files, search it directly.
    let script_exts = cwtools_file_manager::file_manager::SCRIPT_EXTENSIONS;
    let has_script_files = std::fs::read_dir(directory)
        .ok()
        .is_some_and(|mut entries| {
            entries.any(|e| {
                if let Ok(entry) = e {
                    entry
                        .path()
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .is_some_and(|ext| script_exts.contains(&ext))
                } else {
                    false
                }
            })
        });

    if known_script_folders.contains(&dir_name) || dir_name.ends_with(".txt") || has_script_files {
        FileManagerConfig {
            root: directory.to_path_buf(),
            include_dirs: vec![".".into()],
            ..Default::default()
        }
    } else {
        FileManagerConfig {
            root: directory.to_path_buf(),
            ..Default::default()
        }
    }
}

/// Load a `RuleSet` from a `.cwt` file or a directory of `.cwt` files. Directory
/// load warnings are sent to `on_warning` if provided; a rules *file* that can't
/// be read or parsed is an `Err` (the caller decides whether that's fatal).
pub fn load_rules(
    rules: &RulesInput,
    table: &StringTable,
    on_warning: Option<&mut dyn FnMut(String)>,
) -> Result<RuleSet, String> {
    match rules {
        RulesInput::Dir(dir) => {
            let (ruleset, errors) = load_ruleset_from_dir(dir, table);
            if let Some(sink) = on_warning {
                for err in &errors {
                    sink(err.to_string());
                }
            }
            Ok(ruleset)
        }
        RulesInput::File(file) => {
            let rules_str = std::fs::read_to_string(file)
                .map_err(|e| format!("could not read rules {}: {}", file.display(), e))?;
            parse_string(&rules_str, table)
                .map(|parsed| ast_to_ruleset(&parsed, table))
                .map_err(|e| format!("could not parse rules {}: {}", file.display(), e))
        }
    }
}
