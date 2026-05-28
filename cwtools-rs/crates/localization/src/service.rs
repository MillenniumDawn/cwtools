//! Localization service.
//!
//! Aggregates loc files across multiple directories and provides
//! a per-language API (`LocApi`).
//!
//! Mirrors F# `LocalisationManager.fs`.

use crate::commands::{key_to_language, Lang, LocApi, LocEntry, LocFile};
use crate::yaml_parser::parse_loc_text;
use std::collections::HashMap;

/// A multi-file localization service for a single game.
pub struct LocService {
    /// lang → API
    apis: HashMap<Lang, LocApi>,
    /// file path → parse result (for error reporting)
    results: Vec<(String, Result<LocFile, String>)>,
}

impl LocService {
    /// Create from a list of (file_path, file_text) pairs.
    pub fn from_files(files: Vec<(String, String)>) -> Self {
        let mut by_lang: HashMap<Lang, HashMap<String, LocEntry>> = HashMap::new();
        let mut results: Vec<(String, Result<LocFile, String>)> = Vec::new();

        for (path, text) in files {
            match parse_loc_text(&text, &path) {
                Ok(file) => {
                    // Track the result
                    results.push((path.clone(), Ok(file.clone())));

                    // Merge entries into per-language map
                    if let Some(lang) = file.lang {
                        let map = by_lang.entry(lang).or_default();
                        for e in file.entries {
                            map.insert(e.key.clone(), e);
                        }
                    }
                }
                Err(e) => {
                    results.push((path, Err(e)));
                }
            }
        }

        let apis = by_lang
            .into_iter()
            .map(|(k, v)| (k, LocApi::new(v)))
            .collect();

        Self { apis, results }
    }

    /// Load from a directory tree (recursively).
    pub fn from_folder(folder: &std::path::Path) -> Self {
        // Find all .yml / .csv files
        let files = walk_folder(folder);
        Self::from_files(files)
    }

    /// Get the API for a specific language.
    pub fn api(&self, lang: Lang) -> Option<&LocApi> {
        self.apis.get(&lang)
    }

    /// Get all parse results (for diagnostics).
    pub fn results(&self) -> &[(String, Result<LocFile, String>)] {
        &self.results
    }
}

fn walk_folder(folder: &std::path::Path) -> Vec<(String, String)> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(folder) else {
        return files;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(walk_folder(&path));
        } else {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            if ext == "yml" || ext == "csv" {
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                files.push((path.to_string_lossy().to_string(), text));
            }
        }
    }

    files
}
