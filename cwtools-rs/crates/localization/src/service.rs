//! Localization service.
//!
//! Aggregates loc files across multiple directories. Entries are owned once in
//! `files`; per-language / per-key views are derived on demand.
//!
//! Mirrors F# `LocalisationManager.fs`.

use crate::commands::{Lang, LocFile};
use crate::csv_parser::parse_csv_loc_per_lang;
use crate::yaml_parser::parse_loc_text;
use cwtools_file_manager::{
    FileEncoding, is_excluded_dir, is_excluded_root_dir, is_loc_ext, read_text_with_encoding,
};
use std::path::{Path, PathBuf};

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
        let results: Vec<Result<Vec<LocFile>, (String, String)>> = files
            .into_par_iter()
            .map(|(path, text, encoding)| parse_loc_file_entry(path, text, encoding))
            .collect();
        Self::from_results(results)
    }

    /// Collect the parallel per-file parse results into the service, preserving
    /// input order for first-seen-wins semantics and diagnostics order.
    fn from_results(results: Vec<Result<Vec<LocFile>, (String, String)>>) -> Self {
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
    pub fn from_folder(folder: &Path) -> Self {
        Self::from_paths(walk_folder(folder))
    }

    /// Load from several directory trees (e.g. a mod dir plus the vanilla
    /// install). Later folders' keys join the union; duplicate keys keep the
    /// first-seen entry per language.
    pub fn from_folders(folders: &[&Path]) -> Self {
        Self::from_paths(Self::discover_files(folders))
    }

    /// Discover the on-disk loc file paths `from_folders` would parse,
    /// without reading or parsing them. For callers that only need a cheap
    /// stat-based signature over the loc tree (e.g. the LSP's quiet
    /// background rescan deciding whether to skip a full loc rebuild) —
    /// sharing this walk with `from_folders` means the two can't disagree on
    /// what counts as a loc file.
    pub fn discover_files(folders: &[&Path]) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        for folder in folders {
            paths.extend(walk_folder(folder));
        }
        paths
    }

    /// Read and parse a set of loc files in parallel. Reading (disk I/O) happens
    /// inside the parallel map alongside parsing — mirroring the CLI's
    /// `discover_and_parse` — so a large loc tree isn't read sequentially before
    /// the parse fans out.
    fn from_paths(paths: Vec<PathBuf>) -> Self {
        use rayon::prelude::*;
        let results: Vec<Result<Vec<LocFile>, (String, String)>> = paths
            .into_par_iter()
            .map(|path| {
                let path_str = path.to_string_lossy().to_string();
                match read_text_with_encoding(&path) {
                    Ok((text, enc)) => parse_loc_file_entry(path_str, text, Some(enc)),
                    Err(e) => Err((path_str, format!("IO error: {}", e))),
                }
            })
            .collect();
        Self::from_results(results)
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

/// Parse one loc file's text. CSV files (CK2/VIC2) are routed through
/// `csv_parser` (one `LocFile` per language present); everything else goes
/// through `parse_loc_text` (YAML).
fn parse_loc_file_entry(
    path: String,
    text: String,
    encoding: Option<FileEncoding>,
) -> Result<Vec<LocFile>, (String, String)> {
    let is_csv = Path::new(&path)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("csv"));
    if is_csv {
        // CSV: produce one LocFile per language present in the file.
        let entries_by_lang = parse_csv_loc_per_lang(&text, &path, None);
        let mut by_lang: std::collections::HashMap<Lang, Vec<crate::commands::LocEntry>> =
            std::collections::HashMap::new();
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
                parse_errors: Vec::new(),
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
}

/// True for a directory name the game treats as a localisation root.
fn is_loc_dir_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "localisation" || lower == "localization"
}

/// Tooling / VCS / build directories that never hold game loc. Skipped during the
/// walk so a mirror of the mod tree (e.g. a `.claude/worktrees/<wt>/localisation`,
/// a `.git` checkout, or `node_modules`) isn't loaded and double-counted. Shares
/// `FileManager`'s exclusion set so the two walkers stay consistent; any
/// dot-directory is additionally skipped, and the root-anchored `resources/`
/// exclusion applies only to a direct child of the walk root.
fn is_excluded_loc_dir(name: &str, at_root: bool) -> bool {
    name.starts_with('.') || is_excluded_dir(name) || (at_root && is_excluded_root_dir(name))
}

fn walk_folder(folder: &Path) -> Vec<PathBuf> {
    // Only files under a `localisation` (or `localization`) directory are loc —
    // that's what the game and F# load. Scanning every `.yml` in the tree pulls
    // in CI workflows, editor caches, and staging copies as bogus loc files
    // (false CW254/CW268) and wastes memory on data the game never reads.
    walk_folder_inner(folder, false, true)
}

fn walk_folder_inner(folder: &Path, in_loc: bool, at_root: bool) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(folder) else {
        return files;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let dir_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if is_excluded_loc_dir(dir_name, at_root) {
                continue;
            }
            let child_in_loc = in_loc || is_loc_dir_name(dir_name);
            files.extend(walk_folder_inner(&path, child_in_loc, false));
        } else if in_loc
            && path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(is_loc_ext)
        {
            files.push(path);
        }
    }

    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excluded_loc_dirs_skip_tooling_not_content() {
        for skip in [
            ".claude",
            ".git",
            ".vscode",
            ".idea",
            "node_modules",
            "target",
            "out",
            "dist",
            "bin",
            "obj",
        ] {
            assert!(is_excluded_loc_dir(skip, false), "{skip} should be skipped");
        }
        for keep in ["localisation", "localization", "common", "english"] {
            assert!(!is_excluded_loc_dir(keep, false), "{keep} should be walked");
        }
        // `resources` is excluded only at the walk root.
        assert!(is_excluded_loc_dir("resources", true));
        assert!(!is_excluded_loc_dir("resources", false));
    }

    #[test]
    fn from_files_parses_yaml_and_records_language() {
        let svc = LocService::from_files(vec![(
            "mod/localisation/english/test_l_english.yml".to_string(),
            r#"l_english:
 my_key:0 "value"
"#
            .to_string(),
        )]);
        assert!(svc.errors().is_empty(), "{:?}", svc.errors());
        let files = svc.files();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].lang, Some(Lang::English));
        assert_eq!(files[0].entries.len(), 1);
        assert_eq!(files[0].entries[0].key, "my_key");
        assert!(files[0].entries[0].desc.contains("value"));
    }

    #[test]
    fn from_files_merges_keys_from_multiple_files_same_language() {
        let svc = LocService::from_files(vec![
            (
                "a_l_english.yml".to_string(),
                r#"l_english:
 first:0 "A"
"#
                .to_string(),
            ),
            (
                "b_l_english.yml".to_string(),
                r#"l_english:
 second:0 "B"
 first:0 "A2"
"#
                .to_string(),
            ),
        ]);
        assert!(svc.errors().is_empty(), "{:?}", svc.errors());
        let english: Vec<&LocFile> = svc
            .files()
            .iter()
            .filter(|f| f.lang == Some(Lang::English))
            .collect();
        assert_eq!(english.len(), 2);
        let all_keys: Vec<&str> = english
            .iter()
            .flat_map(|f| f.entries.iter().map(|e| e.key.as_str()))
            .collect();
        assert!(all_keys.contains(&"first"));
        assert!(all_keys.contains(&"second"));
        assert_eq!(english[0].entries[0].desc, "\"A\"");
    }

    #[test]
    fn from_files_preserves_file_order() {
        let svc = LocService::from_files(vec![
            (
                "z_l_english.yml".to_string(),
                r#"l_english:
 z:0 "Z"
"#
                .to_string(),
            ),
            (
                "a_l_english.yml".to_string(),
                r#"l_english:
 a:0 "A"
"#
                .to_string(),
            ),
        ]);
        assert_eq!(svc.files()[0].path, "z_l_english.yml");
        assert_eq!(svc.files()[1].path, "a_l_english.yml");
    }

    #[test]
    fn from_files_reports_parse_errors_without_panicking() {
        let svc = LocService::from_files(vec![(
            "broken_l_english.yml".to_string(),
            "this is not a valid loc file\n".to_string(),
        )]);
        assert!(
            !svc.errors().is_empty(),
            "parse errors should be collected, not panic"
        );
        assert_eq!(svc.files().len(), 0);
    }

    #[test]
    fn from_files_with_encoding_records_bom_status() {
        let svc = LocService::from_files_with_encoding(vec![(
            "bom_l_english.yml".to_string(),
            r#"l_english:
 key:0 "v"
"#
            .to_string(),
            Some(cwtools_file_manager::FileEncoding::Utf8Bom),
        )]);
        assert_eq!(svc.files().len(), 1);
        assert_eq!(
            svc.files()[0].encoding,
            Some(cwtools_file_manager::FileEncoding::Utf8Bom)
        );
    }

    #[test]
    fn from_folder_skips_non_localisation_directories() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("not_loc")).unwrap();
        std::fs::create_dir_all(tmp.path().join("localisation")).unwrap();
        std::fs::write(
            tmp.path().join("not_loc").join("bad_l_english.yml"),
            r#"l_english:
 key:0 "v"
"#,
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("localisation").join("good_l_english.yml"),
            r#"l_english:
 key:0 "v"
"#,
        )
        .unwrap();

        let svc = LocService::from_folder(tmp.path());
        assert_eq!(svc.files().len(), 1);
        assert!(
            svc.files()[0]
                .path
                .ends_with("localisation/good_l_english.yml")
        );
    }

    #[test]
    fn from_folder_skips_excluded_dot_directories() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".claude").join("localisation")).unwrap();
        std::fs::write(
            tmp.path()
                .join(".claude")
                .join("localisation")
                .join("dup_l_english.yml"),
            r#"l_english:
 key:0 "v"
"#,
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("localisation")).unwrap();
        std::fs::write(
            tmp.path().join("localisation").join("good_l_english.yml"),
            r#"l_english:
 key:0 "v"
"#,
        )
        .unwrap();

        let svc = LocService::from_folder(tmp.path());
        assert_eq!(svc.files().len(), 1);
        assert!(
            svc.files()[0]
                .path
                .ends_with("localisation/good_l_english.yml")
        );
    }

    #[test]
    fn from_files_routes_csv_to_csv_parser() {
        let csv = "#CODE;English;French;German;;Spanish\nKEY_A;Hello;Bonjour;Hallo;;Hola\n";
        let svc = LocService::from_files(vec![(
            "mod/localisation/localisation.csv".to_string(),
            csv.to_string(),
        )]);
        assert!(svc.errors().is_empty(), "{:?}", svc.errors());
        let langs = svc.languages();
        assert!(langs.contains(&Lang::English), "got: {:?}", langs);
        assert!(langs.contains(&Lang::French), "got: {:?}", langs);
        assert!(
            svc.files().iter().any(|f| {
                f.path.ends_with("localisation.csv")
                    && f.lang == Some(Lang::English)
                    && f.entries
                        .iter()
                        .any(|e| e.key == "KEY_A" && e.desc == "Hello")
            }),
            "CSV should produce English LocFile with KEY_A: {:?}",
            svc.files()
        );
    }

    #[test]
    fn from_folders_merges_multiple_roots() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(a.path().join("localisation")).unwrap();
        std::fs::create_dir_all(b.path().join("localisation")).unwrap();
        std::fs::write(
            a.path().join("localisation").join("a_l_english.yml"),
            r#"l_english:
 a:0 "A"
"#,
        )
        .unwrap();
        std::fs::write(
            b.path().join("localisation").join("b_l_english.yml"),
            r#"l_english:
 b:0 "B"
"#,
        )
        .unwrap();

        let svc = LocService::from_folders(&[a.path(), b.path()]);
        let paths: Vec<&str> = svc.files().iter().map(|f| f.path.as_str()).collect();
        assert!(
            paths.iter().any(|p| p.contains("a_l_english.yml")),
            "folder a missing: {:?}",
            paths
        );
        assert!(
            paths.iter().any(|p| p.contains("b_l_english.yml")),
            "folder b missing: {:?}",
            paths
        );
    }

    #[test]
    fn languages_returns_unique_langs() {
        let svc = LocService::from_files(vec![
            (
                "a_l_english.yml".to_string(),
                r#"l_english:
 a:0 "A"
"#
                .to_string(),
            ),
            (
                "b_l_english.yml".to_string(),
                r#"l_english:
 b:0 "B"
"#
                .to_string(),
            ),
            (
                "c_l_french.yml".to_string(),
                r#"l_french:
 c:0 "C"
"#
                .to_string(),
            ),
        ]);
        let mut langs = svc.languages();
        langs.sort_by_key(|l| format!("{l}"));
        assert_eq!(langs, vec![Lang::English, Lang::French]);
    }
}
