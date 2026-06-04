use cwtools_parser::ast::{Arena, Child};
use cwtools_parser::parser::parse_string;
use cwtools_string_table::string_table::StringTable;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// How the file should be treated during discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileKind {
    /// Paradox script (.txt / .gui / .gfx) — parsed into an AST.
    Script,
    /// Localisation (.yml / .csv) — not script-parsed, stored separately.
    Localisation,
    /// Binary / asset file (.dds, .png, .tga, .wav, .lua, .mesh, .shader, etc.)
    /// — existence is noted but the file is not read.
    Resource,
}

/// Classify a file by its extension, matching F# FileManager.fs:215-273.
pub fn classify_extension(path: &Path) -> FileKind {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "txt" | "gui" | "gfx" => FileKind::Script,
        "yml" | "yaml" | "csv" => FileKind::Localisation,
        _ => FileKind::Resource,
    }
}

#[derive(Debug, Error)]
pub enum FileError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Parse error: {0}")]
    Parse(String),
    #[error("Pattern error: {0}")]
    Pattern(String),
}

/// A discovered script file with its parsed AST.
pub struct ParsedFile {
    /// Absolute path on disk.
    pub path: PathBuf,
    /// Game-relative logical path (e.g. `common/scripted_effects/foo.txt`).
    pub logical_path: String,
    pub arena: Arena,
    pub root_children: Vec<Child>,
}

/// Paradox `.mod` descriptor fields.
#[derive(Debug, Clone)]
pub struct ModDescriptor {
    pub name: String,
    pub path: Option<String>,
    pub replace_paths: Vec<String>,
}

/// Directory classification mirroring F# `DirectoryType`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectoryType {
    Vanilla,
    Mod,
    MultipleMod,
    Unknown,
}

/// Configuration for file discovery.
pub struct FileManagerConfig {
    /// Root directory to search.
    pub root: PathBuf,
    /// Subdirectories to include (e.g., "common", "events").
    pub include_dirs: Vec<String>,
    /// Glob patterns for files (e.g., "*.txt").
    pub file_patterns: Vec<String>,
    /// Patterns to exclude (filename-level).
    pub exclude_patterns: Vec<String>,
    /// Directory names to skip entirely.
    pub exclude_dirs: Vec<String>,
    /// Skip files larger than this (bytes). 0 = no limit.
    pub max_file_size: u64,
}

impl Default for FileManagerConfig {
    fn default() -> Self {
        Self {
            root: PathBuf::from("."),
            include_dirs: vec![
                "common".into(),
                "events".into(),
                "history".into(),
                "gfx".into(),
                "interface".into(),
                "decisions".into(),
                "missions".into(),
            ],
            file_patterns: vec!["*.txt".into()],
            exclude_patterns: vec![],
            exclude_dirs: vec![
                ".git".into(),
                "target".into(),
                ".vs".into(),
                "node_modules".into(),
                "out".into(),
                "dist".into(),
                "bin".into(),
                "obj".into(),
                ".idea".into(),
            ],
            max_file_size: 2 * 1024 * 1024, // 2 MB
        }
    }
}

pub struct FileManager {
    pub config: FileManagerConfig,
    pub string_table: StringTable,
}

impl FileManager {
    pub fn new(config: FileManagerConfig) -> Self {
        Self {
            config,
            string_table: StringTable::new(),
        }
    }

    pub fn with_string_table(config: FileManagerConfig, table: StringTable) -> Self {
        Self {
            config,
            string_table: table,
        }
    }

    /// Discover and parse all matching script files under the configured root.
    /// Non-script files (localisation, resources) are silently skipped.
    pub fn discover_and_parse(&mut self) -> Result<Vec<ParsedFile>, FileError> {
        let mut files = Vec::new();
        let include_dirs: Vec<String> = self.config.include_dirs.clone();
        let root = self.config.root.clone();

        for include_dir in include_dirs {
            let dir = if include_dir == "." {
                root.clone()
            } else {
                root.join(&include_dir)
            };
            if !dir.exists() {
                continue;
            }
            self.walk_dir(&dir, &mut files)?;
        }

        Ok(files)
    }

    fn walk_dir(&mut self, dir: &Path, out: &mut Vec<ParsedFile>) -> Result<(), FileError> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect();
        // Sort for deterministic ordering
        entries.sort_by_key(|e| {
            e.as_ref()
                .map(|e| e.file_name())
                .unwrap_or_default()
        });

        for entry in entries {
            let entry = entry?;
            let path = entry.path();

            if path.is_dir() {
                let dir_name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("");
                if self
                    .config
                    .exclude_dirs
                    .iter()
                    .any(|ex| dir_name.eq_ignore_ascii_case(ex))
                {
                    continue;
                }
                self.walk_dir(&path, out)?;
                continue;
            }

            // Extension routing — skip non-script files early
            if classify_extension(&path) != FileKind::Script {
                continue;
            }

            let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

            // Check include patterns
            let mut matched = false;
            for pattern in &self.config.file_patterns {
                if glob_match(pattern, file_name) {
                    matched = true;
                    break;
                }
            }
            if !matched {
                continue;
            }

            // Check exclude patterns
            let mut excluded = false;
            for pattern in &self.config.exclude_patterns {
                if glob_match(pattern, file_name) {
                    excluded = true;
                    break;
                }
            }
            if excluded {
                continue;
            }

            // Size guard
            if self.config.max_file_size > 0 {
                if let Ok(meta) = path.metadata() {
                    if meta.len() > self.config.max_file_size {
                        continue;
                    }
                }
            }

            // Compute logical path relative to root
            let logical_path = compute_logical_path(&path, &self.config.root);

            // Parse file
            let content = std::fs::read_to_string(&path)?;
            match parse_string(&content, &self.string_table) {
                Ok(parsed) => {
                    out.push(ParsedFile {
                        path,
                        logical_path,
                        arena: parsed.arena,
                        root_children: parsed.root_children,
                    });
                }
                Err(e) => {
                    // Non-fatal: skip files that fail to parse and continue
                    eprintln!("warn: skipping {}: {}", path.display(), e);
                }
            }
        }
        Ok(())
    }

    pub fn parse_single_file(&mut self, path: &Path) -> Result<ParsedFile, FileError> {
        let content = std::fs::read_to_string(path)?;
        let logical_path = compute_logical_path(path, &self.config.root);
        match parse_string(&content, &self.string_table) {
            Ok(parsed) => Ok(ParsedFile {
                path: path.to_path_buf(),
                logical_path,
                arena: parsed.arena,
                root_children: parsed.root_children,
            }),
            Err(e) => Err(FileError::Parse(format!("{}: {}", path.display(), e))),
        }
    }
}

/// Compute the logical (game-relative) path by stripping the root prefix.
///
/// Given `root = /mnt/mod` and `path = /mnt/mod/common/effects/foo.txt`,
/// returns `common/effects/foo.txt`.
pub fn compute_logical_path(path: &Path, root: &Path) -> String {
    // Normalise both to forward slashes
    let path_str = path.to_string_lossy().replace('\\', "/");
    let root_str = {
        let s = root.to_string_lossy().replace('\\', "/");
        if s.ends_with('/') {
            s
        } else {
            format!("{}/", s)
        }
    };

    if let Some(rel) = path_str.strip_prefix(&root_str) {
        rel.to_string()
    } else {
        // fallback: just the file name
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string()
    }
}

/// Parse a Paradox `.mod` descriptor file (plain key=value Paradox script).
///
/// Mirrors F# FileManager.fs:91-125: extracts `name`, `path`, and
/// `replace_path` entries.
pub fn parse_mod_descriptor(path: &Path) -> Result<ModDescriptor, FileError> {
    let content = std::fs::read_to_string(path)?;
    let mut name = String::new();
    let mut mod_path = None;
    let mut replace_paths = Vec::new();

    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let key = k.trim();
            let val = v.trim().trim_matches('"').to_string();
            match key {
                "name" => name = val,
                "path" | "archive" => mod_path = Some(val),
                "replace_path" => replace_paths.push(val),
                _ => {}
            }
        }
    }

    Ok(ModDescriptor {
        name,
        path: mod_path,
        replace_paths,
    })
}

/// Classify a directory following F# FileManager.fs:80-147.
///
/// - `Vanilla` if it contains `game/` or `common/` typical structure
/// - `Mod` if it looks like a single mod (has common/events/interface/gfx/localisation)
/// - `MultipleMod` if it contains a `mod/` or `mods/` folder with `.mod` files
/// - `Unknown` otherwise
pub fn classify_directory(dir: &Path) -> DirectoryType {
    let looks_like_game_folder = |d: &Path| -> bool {
        for sub in &["common", "events", "interface", "gfx", "localisation"] {
            if d.join(sub).is_dir() {
                return true;
            }
        }
        false
    };

    // Vanilla: contains a "game" sub-directory that itself looks like a game folder
    let game_sub = dir.join("game");
    if game_sub.is_dir() && looks_like_game_folder(&game_sub) {
        return DirectoryType::Vanilla;
    }

    // Mod: the directory itself looks like a mod
    if looks_like_game_folder(dir) {
        return DirectoryType::Mod;
    }

    // MultipleMod: contains mod/ or mods/ with .mod files
    for mod_folder_name in &["mod", "mods"] {
        let mod_folder = dir.join(mod_folder_name);
        if mod_folder.is_dir() {
            let has_mod_files = std::fs::read_dir(&mod_folder)
                .ok()
                .map(|mut entries| {
                    entries.any(|e| {
                        e.ok()
                            .and_then(|e| {
                                let p = e.path();
                                if p.extension()
                                    .map(|ex| ex.eq_ignore_ascii_case("mod"))
                                    .unwrap_or(false)
                                {
                                    Some(())
                                } else {
                                    None
                                }
                            })
                            .is_some()
                    })
                })
                .unwrap_or(false);
            if has_mod_files {
                return DirectoryType::MultipleMod;
            }
        }
    }

    DirectoryType::Unknown
}

/// Simple glob matching (supports `*` wildcard and `?`).
///
/// Handles:
/// - `*.ext` suffix matching
/// - `prefix*` prefix matching
/// - `?` single-char wildcard
/// - Directory-name plain equality
pub fn glob_match(pattern: &str, text: &str) -> bool {
    // Fast path for *.ext
    if let Some(suffix) = pattern.strip_prefix('*') {
        return text.ends_with(suffix);
    }
    // Fast path for prefix*
    if let Some(prefix) = pattern.strip_suffix('*') {
        return text.starts_with(prefix);
    }
    // General: treat * as "any chars", ? as "any single char"
    glob_match_general(pattern, text)
}

fn glob_match_general(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    glob_dp(&p, &t)
}

fn glob_dp(p: &[char], t: &[char]) -> bool {
    let m = p.len();
    let n = t.len();
    let mut dp = vec![vec![false; n + 1]; m + 1];
    dp[0][0] = true;
    for i in 1..=m {
        if p[i - 1] == '*' {
            dp[i][0] = dp[i - 1][0];
        }
    }
    for i in 1..=m {
        for j in 1..=n {
            if p[i - 1] == '*' {
                dp[i][j] = dp[i - 1][j] || dp[i][j - 1];
            } else if p[i - 1] == '?' || p[i - 1] == t[j - 1] {
                dp[i][j] = dp[i - 1][j - 1];
            }
        }
    }
    dp[m][n]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matching() {
        assert!(glob_match("*.txt", "foo.txt"));
        assert!(!glob_match("*.txt", "foo.png"));
        assert!(glob_match("*.cwt", "rules.cwt"));
        assert!(glob_match("foo*", "foobar"));
        assert!(!glob_match("foo*", "barfoo"));
        assert!(glob_match("f?o.txt", "foo.txt"));
        assert!(!glob_match("f?o.txt", "fooo.txt"));
    }

    #[test]
    fn classify_ext() {
        assert_eq!(classify_extension(Path::new("foo.txt")), FileKind::Script);
        assert_eq!(classify_extension(Path::new("foo.gui")), FileKind::Script);
        assert_eq!(classify_extension(Path::new("foo.yml")), FileKind::Localisation);
        assert_eq!(classify_extension(Path::new("foo.csv")), FileKind::Localisation);
        assert_eq!(classify_extension(Path::new("foo.dds")), FileKind::Resource);
        assert_eq!(classify_extension(Path::new("foo.png")), FileKind::Resource);
    }

    #[test]
    fn logical_path_stripping() {
        let root = PathBuf::from("/mnt/mod");
        let path = PathBuf::from("/mnt/mod/common/effects/foo.txt");
        assert_eq!(compute_logical_path(&path, &root), "common/effects/foo.txt");
    }

    #[test]
    fn logical_path_fallback() {
        let root = PathBuf::from("/other");
        let path = PathBuf::from("/mnt/mod/foo.txt");
        assert_eq!(compute_logical_path(&path, &root), "foo.txt");
    }
}
