//! Localization service.
//!
//! Aggregates loc files across multiple directories. Entries are owned once in
//! `files`; per-language / per-key views are derived on demand.
//!
//! Mirrors F# `LocalisationManager.fs`.

use crate::commands::{Lang, LocFile};
use crate::csv_parser::parse_csv_loc_per_lang;
use crate::yaml_parser::parse_loc_text;
use cwtools_file_manager::{FileEncoding, read_text_with_encoding};

/// A multi-file localization service for a single game.
///
/// Loc entries are owned exactly once, in `files`. Per-language and per-key
/// views are derived on demand (or by [`crate::loc_index::LocIndex`]) rather
/// than stored as a second copy — for large projects (Millennium Dawn ships
/// ~2M loc entries) a second owned copy dominated the heap.
pub struct LocService {
    /// Every successfully parsed loc file, in load order.
    files: Vec<LocFile>,
    /// (path, parse error) for files that failed to parse.
    errors: Vec<(String, String)>,
}

impl LocService {
    /// Create from a list of (file_path, file_text) pairs. Encoding is unknown
    /// (no CW254 check) — use [`LocService::from_folder`] when bytes are on disk.
    pub fn from_files(files: Vec<(String, String)>) -> Self {
        Self::from_files_with_encoding(files.into_iter().map(|(p, t)| (p, t, None)).collect())
    }

    /// As [`from_files`], but each file carries its detected on-disk encoding so
    /// the UTF-8-BOM rule (CW254) can be enforced.
    pub fn from_files_with_encoding(files: Vec<(String, String, Option<FileEncoding>)>) -> Self {
        use rayon::prelude::*;

        // Parsing is independent per file; run it in parallel, preserving input
        // order (`par_iter` over the indexed Vec) so first-seen-wins semantics
        // and diagnostics order are unchanged.
        //
        // CSV files (CK2/VIC2) are routed through csv_parser; everything else
        // goes through parse_loc_text (YAML).
        let results: Vec<Result<Vec<LocFile>, (String, String)>> = files
            .into_par_iter()
            .map(|(path, text, encoding)| {
                if path.ends_with(".csv") {
                    // CSV: produce one LocFile per language present in the file.
                    let entries_by_lang = parse_csv_loc_per_lang(&text, &path, None);
                    // Group by lang
                    let mut by_lang: std::collections::HashMap<
                        Lang,
                        Vec<crate::commands::LocEntry>,
                    > = std::collections::HashMap::new();
                    for (_key, lang, entry) in entries_by_lang {
                        by_lang.entry(lang).or_default().push(entry);
                    }
                    let loc_files: Vec<LocFile> = by_lang
                        .into_iter()
                        .map(|(lang, entries)| LocFile {
                            path: path.clone(),
                            language_prefix: lang.to_string(),
                            lang: Some(lang),
                            entries,
                            file_diagnostics: Vec::new(),
                            encoding,
                        })
                        .collect();
                    Ok(loc_files)
                } else {
                    match parse_loc_text(&text, &path) {
                        Ok(mut file) => {
                            file.encoding = encoding;
                            Ok(vec![file])
                        }
                        Err(e) => Err((path, e)),
                    }
                }
            })
            .collect();

        let mut parsed: Vec<LocFile> = Vec::new();
        let mut errors: Vec<(String, String)> = Vec::new();
        for r in results {
            match r {
                Ok(files) => parsed.extend(files),
                Err(e) => errors.push(e),
            }
        }

        Self {
            files: parsed,
            errors,
        }
    }

    /// Load from a directory tree (recursively).
    pub fn from_folder(folder: &std::path::Path) -> Self {
        Self::from_files_with_encoding(walk_folder(folder))
    }

    /// Load from several directory trees (e.g. a mod dir plus the vanilla
    /// install). Later folders' keys join the union; duplicate keys keep the
    /// first-seen entry per language.
    pub fn from_folders(folders: &[&std::path::Path]) -> Self {
        let mut files = Vec::new();
        for folder in folders {
            files.extend(walk_folder(folder));
        }
        Self::from_files_with_encoding(files)
    }

    /// All successfully parsed loc files (the single owner of loc entries).
    pub fn files(&self) -> &[LocFile] {
        &self.files
    }

    /// Files that failed to parse, as `(path, error)`.
    pub fn errors(&self) -> &[(String, String)] {
        &self.errors
    }

    /// Languages that actually have loc data loaded.
    pub fn languages(&self) -> Vec<Lang> {
        let mut langs: Vec<Lang> = Vec::new();
        for f in &self.files {
            if let Some(l) = f.lang
                && !langs.contains(&l)
            {
                langs.push(l);
            }
        }
        langs
    }
}

type WalkedFile = (String, String, Option<FileEncoding>);

/// True for a directory name the game treats as a localisation root.
fn is_loc_dir_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "localisation" || lower == "localization"
}

fn walk_folder(folder: &std::path::Path) -> Vec<WalkedFile> {
    // Only files under a `localisation` (or `localization`) directory are loc —
    // that's what the game and F# load. Scanning every `.yml` in the tree pulls
    // in CI workflows, editor caches, and staging copies as bogus loc files
    // (false CW254/CW268) and wastes memory on data the game never reads.
    walk_folder_inner(folder, false)
}

fn walk_folder_inner(folder: &std::path::Path, in_loc: bool) -> Vec<WalkedFile> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(folder) else {
        return files;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let child_in_loc = in_loc
                || path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(is_loc_dir_name);
            files.extend(walk_folder_inner(&path, child_in_loc));
        } else if in_loc
            && matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("yml") | Some("csv")
            )
        {
            match read_text_with_encoding(&path) {
                Ok((text, enc)) => {
                    files.push((path.to_string_lossy().to_string(), text, Some(enc)));
                }
                Err(e) => {
                    // Propagate IO errors as a failed entry so callers can report them.
                    files.push((
                        path.to_string_lossy().to_string(),
                        format!("IO error: {}", e),
                        None,
                    ));
                }
            }
        }
    }

    files
}
