//! Read-only loc-key index consumed by config validation.
//!
//! Built once per validation run from a [`LocService`], it answers the
//! questions the config-side `LocalisationField` check needs:
//! * does this key exist in any language? (synced=false)
//! * which languages-with-data are missing this key? (synced=true)
//! * what is the parsed loc entry for this key? (scope-aware command checks)
//!
//! All keys are stored lowercased to match F#'s case-insensitive comparison.

use crate::commands::{Game, Lang, LocEntry};
use crate::service::LocService;
use std::collections::{HashMap, HashSet};

/// Per-language loc-key index plus a representative parsed entry per key.
#[derive(Debug, Clone, Default)]
pub struct LocIndex {
    /// language -> lowercased key set
    per_language: HashMap<Lang, HashSet<String>>,
    /// union of all keys across every language
    union: HashSet<String>,
    /// languages the project actually ships loc data for
    languages_with_data: Vec<Lang>,
    /// lowercased key -> a representative parsed entry (English preferred), kept
    /// ONLY for keys whose representative actually has `[command]` chains — the
    /// sole consumer is the scope-aware command check. Keeping a full entry per
    /// key would re-clone all ~2M loc entries; almost none carry commands.
    entries: HashMap<String, LocEntry>,
}

impl LocIndex {
    /// Build from a loaded [`LocService`]. `game` is accepted for symmetry with
    /// the rest of the API (language restriction already happened at parse time).
    pub fn build(service: &LocService, game: Game) -> Self {
        Self::build_scoped(service, game, None)
    }

    /// As [`build`], but restrict the "missing translation" check to a chosen
    /// set of languages. With `langs = Some([English])`, an english-targeted mod
    /// won't be told every key is missing in french/german/… that the loaded
    /// vanilla install happens to ship. `langs = None` keeps all languages with
    /// data (the previous behavior). The key `union` (existence resolution) is
    /// never restricted, so config `$ref$` checks still resolve any loaded key.
    pub fn build_scoped(service: &LocService, _game: Game, langs: Option<&[Lang]>) -> Self {
        let mut per_language: HashMap<Lang, HashSet<String>> = HashMap::new();
        let mut union: HashSet<String> = HashSet::new();
        let mut entries: HashMap<String, LocEntry> = HashMap::new();

        for file in service.files() {
            let Some(lang) = file.lang else { continue };
            let set = per_language.entry(lang).or_default();
            for entry in &file.entries {
                let lower = entry.key.to_lowercase();
                set.insert(lower.clone());
                union.insert(lower.clone());

                // Representative entry for command validation only — skip keys
                // with no commands so the map stays tiny.
                if entry.commands.is_empty() && entry.jomini_commands.is_empty() {
                    continue;
                }
                // Prefer the English entry; otherwise keep the first seen.
                match entries.get(&lower) {
                    Some(_) if lang != Lang::English => {}
                    _ => {
                        entries.insert(lower, entry.clone());
                    }
                }
            }
        }

        let mut languages_with_data = service.languages();
        if let Some(set) = langs {
            languages_with_data.retain(|l| set.contains(l));
        }
        Self {
            per_language,
            union,
            languages_with_data,
            entries,
        }
    }

    /// Merge cached per-language key sets (the vanilla-cache restore path):
    /// keys join the union + per-language sets, and languages new to the index
    /// join `languages_with_data` subject to the same `langs` scoping as
    /// [`build_scoped`]. No `entries` are added — cached keys carry no parsed
    /// loc values (the command check only applies to content we validate).
    pub fn merge_cached_keys(
        &mut self,
        per_language: Vec<(Lang, Vec<String>)>,
        langs: Option<&[Lang]>,
    ) {
        for (lang, keys) in per_language {
            let set = self.per_language.entry(lang).or_default();
            for k in keys {
                self.union.insert(k.clone());
                set.insert(k);
            }
            let allowed = langs.map(|ls| ls.contains(&lang)).unwrap_or(true);
            if allowed && !self.languages_with_data.contains(&lang) {
                self.languages_with_data.push(lang);
            }
        }
    }

    /// synced=false: the key exists in at least one language.
    pub fn exists_any(&self, key_lower: &str) -> bool {
        self.union.contains(key_lower)
    }

    /// synced=true: languages that have loc data but are missing this key.
    ///
    /// Only languages the project actually ships are considered, so an
    /// english-only mod never reports "missing in french/german/...".
    pub fn missing_synced_languages(&self, key_lower: &str) -> Vec<Lang> {
        self.languages_with_data
            .iter()
            .copied()
            .filter(|lang| {
                self.per_language
                    .get(lang)
                    .map(|set| !set.contains(key_lower))
                    .unwrap_or(true)
            })
            .collect()
    }

    /// The representative parsed entry for a key (for command validation).
    pub fn entry(&self, key_lower: &str) -> Option<&LocEntry> {
        self.entries.get(key_lower)
    }

    /// Languages with loc data.
    pub fn languages_with_data(&self) -> &[Lang] {
        &self.languages_with_data
    }

    /// The union of all loc keys (lowercased), for single-file `$ref$` checks.
    pub fn union(&self) -> &HashSet<String> {
        &self.union
    }
}

/// Extract per-language lowercased key sets from a loaded [`LocService`] —
/// the shape the vanilla cache stores (language display name -> keys).
pub fn per_language_keys(service: &LocService) -> Vec<(String, Vec<String>)> {
    let mut per: HashMap<Lang, HashSet<String>> = HashMap::new();
    for file in service.files() {
        let Some(lang) = file.lang else { continue };
        let set = per.entry(lang).or_default();
        for entry in &file.entries {
            set.insert(entry.key.to_lowercase());
        }
    }
    per.into_iter()
        .map(|(lang, keys)| (lang.to_string(), keys.into_iter().collect()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::Game;
    use crate::service::LocService;

    fn service_from(files: &[(&str, &str)]) -> LocService {
        LocService::from_files(
            files
                .iter()
                .map(|(p, t)| (p.to_string(), t.to_string()))
                .collect(),
        )
    }

    #[test]
    fn exists_any_is_case_insensitive() {
        let svc = service_from(&[("a_l_english.yml", "l_english:\n MY_Key: \"hi\"\n")]);
        let idx = LocIndex::build(&svc, Game::HOI4);
        assert!(idx.exists_any("my_key"));
        assert!(!idx.exists_any("absent"));
    }

    #[test]
    fn synced_only_flags_languages_with_data() {
        // english + german present; german is missing KEY_B
        let svc = service_from(&[
            (
                "a_l_english.yml",
                "l_english:\n key_a: \"a\"\n key_b: \"b\"\n",
            ),
            ("a_l_german.yml", "l_german:\n key_a: \"a\"\n"),
        ]);
        let idx = LocIndex::build(&svc, Game::HOI4);
        // key_a present in both -> no missing
        assert!(idx.missing_synced_languages("key_a").is_empty());
        // key_b only in english -> german missing
        let missing = idx.missing_synced_languages("key_b");
        assert_eq!(missing, vec![Lang::German]);
        // a project that ships no french never reports french missing
        assert!(!missing.contains(&Lang::French));
    }

    #[test]
    fn build_scoped_restricts_missing_check_to_chosen_languages() {
        // english + german present, key_b missing in german.
        let svc = service_from(&[
            (
                "a_l_english.yml",
                "l_english:\n key_a: \"a\"\n key_b: \"b\"\n",
            ),
            ("a_l_german.yml", "l_german:\n key_a: \"a\"\n"),
        ]);
        // Scoped to english only: german is not a language-with-data, so the
        // missing-translation check no longer flags key_b.
        let idx = LocIndex::build_scoped(&svc, Game::HOI4, Some(&[Lang::English]));
        assert!(idx.missing_synced_languages("key_b").is_empty());
        assert_eq!(idx.languages_with_data(), &[Lang::English]);
        // Existence still resolves against every loaded language.
        assert!(idx.exists_any("key_a"));
    }
}
