use cwtools_parser::ast::{Arena, Child, ParseError};
use cwtools_parser::parser::parse_string;
use cwtools_string_table::string_table::StringTable;
use std::path::{Path, PathBuf};
use thiserror::Error;

// ── Encoding helper ───────────────────────────────────────────────────────────

/// Windows-1252 → Unicode mapping for the 0x80-0x9F range (the gap not covered
/// by ISO-8859-1).  Index 0 = byte 0x80, index 31 = byte 0x9F.
///
/// Source: https://encoding.spec.whatwg.org/index-windows-1252.txt
const CP1252_HIGH: [char; 32] = [
    '\u{20AC}', // 0x80 €
    '\u{FFFD}', // 0x81 (undefined → replacement char)
    '\u{201A}', // 0x82 ‚
    '\u{0192}', // 0x83 ƒ
    '\u{201E}', // 0x84 „
    '\u{2026}', // 0x85 …
    '\u{2020}', // 0x86 †
    '\u{2021}', // 0x87 ‡
    '\u{02C6}', // 0x88 ˆ
    '\u{2030}', // 0x89 ‰
    '\u{0160}', // 0x8A Š
    '\u{2039}', // 0x8B ‹
    '\u{0152}', // 0x8C Œ
    '\u{FFFD}', // 0x8D (undefined)
    '\u{017D}', // 0x8E Ž
    '\u{FFFD}', // 0x8F (undefined)
    '\u{FFFD}', // 0x90 (undefined)
    '\u{2018}', // 0x91 '
    '\u{2019}', // 0x92 '
    '\u{201C}', // 0x93 "
    '\u{201D}', // 0x94 "
    '\u{2022}', // 0x95 •
    '\u{2013}', // 0x96 –
    '\u{2014}', // 0x97 —
    '\u{02DC}', // 0x98 ˜
    '\u{2122}', // 0x99 ™
    '\u{0161}', // 0x9A š
    '\u{203A}', // 0x9B ›
    '\u{0153}', // 0x9C œ
    '\u{FFFD}', // 0x9D (undefined)
    '\u{017E}', // 0x9E ž
    '\u{0178}', // 0x9F Ÿ
];

/// Decode a single byte as Windows-1252.
#[inline]
fn cp1252_byte(b: u8) -> char {
    if b < 0x80 {
        b as char
    } else if b <= 0x9F {
        CP1252_HIGH[(b - 0x80) as usize]
    } else {
        // 0xA0-0xFF: identical to Latin-1 / Unicode
        b as char
    }
}

/// How a file was encoded on disk, detected while reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileEncoding {
    /// Valid UTF-8 starting with the UTF-8 BOM (`EF BB BF`). What Paradox wants
    /// for localisation files.
    Utf8Bom,
    /// Valid UTF-8 but with no BOM.
    Utf8NoBom,
    /// Not valid UTF-8 (decoded via Windows-1252 fallback).
    NonUtf8,
}

const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// Read a file as text: try UTF-8 first, fall back to Windows-1252.
///
/// Pre-Jomini games (CK2, EU4, VIC2, HOI4 old mods) often encode files in
/// Windows-1252.  Blindly using `read_to_string` fails on any accented byte
/// outside ASCII (e.g. `é` = 0xE9).  This helper avoids that breakage.
pub fn read_text(path: &Path) -> Result<String, FileError> {
    read_text_with_encoding(path).map(|(s, _)| s)
}

/// As [`read_text`], but also reports how the file was encoded so callers can
/// enforce encoding rules (e.g. localisation must be UTF-8 BOM).
pub fn read_text_with_encoding(path: &Path) -> Result<(String, FileEncoding), FileError> {
    let bytes = std::fs::read(path)?;
    let has_bom = bytes.starts_with(&UTF8_BOM);
    // Fast path: valid UTF-8 (includes pure ASCII). The BOM, when present, is
    // valid UTF-8 (U+FEFF) and is kept in the string — existing parsers already
    // tolerate a leading BOM character.
    if let Ok(s) = std::str::from_utf8(&bytes) {
        let enc = if has_bom {
            FileEncoding::Utf8Bom
        } else {
            FileEncoding::Utf8NoBom
        };
        return Ok((s.to_owned(), enc));
    }
    // Not valid UTF-8: strip a leading BOM if any, then decode as Windows-1252.
    let body = if has_bom { &bytes[3..] } else { &bytes[..] };
    let text = body.iter().map(|&b| cp1252_byte(b)).collect();
    Ok((text, FileEncoding::NonUtf8))
}

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
        "txt" | "gui" | "gfx" | "asset" | "sfx" | "map" => FileKind::Script,
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
}

/// A discovered script file with its parsed AST.
pub struct ParsedFile {
    /// Absolute path on disk.
    pub path: PathBuf,
    /// Game-relative logical path (e.g. `common/scripted_effects/foo.txt`).
    pub logical_path: String,
    pub arena: Arena,
    pub root_children: Vec<Child>,
    /// Non-fatal parse errors (file was partially parsed; validate what survived).
    pub errors: Vec<ParseError>,
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
    /// Filename patterns to exclude. Matched with the same `glob_match`
    /// semantics as `file_patterns` (supports `*` and `?`).
    pub exclude_patterns: Vec<String>,
    /// Directory names to skip entirely (exact, case-insensitive).
    pub exclude_dirs: Vec<String>,
    /// Directory glob patterns to skip entirely. Like `exclude_dirs` but each
    /// entry is a glob (`*`, `?`) matched against the directory's basename.
    /// Layers on top of `exclude_dirs` — both lists are checked.
    pub exclude_dir_patterns: Vec<String>,
    /// Directory names skipped ONLY at the workspace root (exact, case-insensitive).
    /// Use for names that are dev-scratch at the top level but a real game folder
    /// when nested — e.g. a root `resources/` is scratch, but `common/resources/`
    /// defines the `resource` type (oil, steel, …) and must be indexed.
    pub exclude_root_dirs: Vec<String>,
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
                "sound".into(),
                "music".into(),
            ],
            file_patterns: vec![
                "*.txt".into(),
                "*.gui".into(),
                "*.gfx".into(),
                "*.sfx".into(),
                "*.asset".into(),
                "*.map".into(),
            ],
            exclude_patterns: vec![
                // Free-form text/markdown files that aren't Paradox script —
                // matching `*.txt` would otherwise send them through the full
                // validator. Users can opt back in by clearing the list.
                "Changelog.txt".into(),
                "README.txt".into(),
                "LICENSE.txt".into(),
                "README.md".into(),
                "LICENSE.md".into(),
                "*.md".into(),
            ],
            exclude_dirs: vec![
                ".git".into(),
                // Claude Code tooling dir. Holds git worktrees that mirror the
                // whole mod tree; walking it double-counts every file (e.g. the
                // loc set), so skip it like .git. Never game content.
                ".claude".into(),
                "target".into(),
                ".vs".into(),
                "node_modules".into(),
                "out".into(),
                "dist".into(),
                "bin".into(),
                "obj".into(),
                ".idea".into(),
                ".vscode".into(),
            ],
            exclude_dir_patterns: vec![],
            // A top-level `resources/` is dev scratch the game never loads; skip it
            // only at the root so the real `common/resources/` (resource defs) still
            // indexes. A bare name-exclude would drop both.
            exclude_root_dirs: vec!["resources".into()],
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
        use rayon::prelude::*;

        // Walk the tree to collect the candidate (path, logical_path) list first
        // (cheap, ordered), then read+parse them in parallel. Parsing is the
        // expensive part and is independent per file. `into_par_iter().collect()`
        // preserves the input order, so discovery output is deterministic.
        let mut paths: Vec<(PathBuf, String)> = Vec::new();
        let root = &self.config.root;

        for include_dir in &self.config.include_dirs {
            let dir = if include_dir == "." {
                root.clone()
            } else {
                root.join(include_dir)
            };
            if !dir.exists() {
                continue;
            }
            self.collect_paths(&dir, &mut paths)?;
        }

        let table = &self.string_table;
        let files = paths
            .into_par_iter()
            .filter_map(|(path, logical_path)| {
                let content = read_text(&path).ok()?;
                match parse_string(&content, table) {
                    Ok(parsed) => Some(ParsedFile {
                        path,
                        logical_path,
                        arena: parsed.arena,
                        root_children: parsed.root_children,
                        errors: parsed.errors,
                    }),
                    Err(e) => {
                        // Non-fatal: skip files that fail to parse and continue
                        eprintln!("warn: skipping {}: {}", path.display(), e);
                        None
                    }
                }
            })
            .collect();

        Ok(files)
    }

    /// Walk `dir` collecting (path, logical_path) for every file that passes the
    /// extension/pattern/size filters. Reading and parsing happen later, in
    /// parallel; this pass is just filesystem traversal.
    fn collect_paths(&self, dir: &Path, out: &mut Vec<(PathBuf, String)>) -> Result<(), FileError> {
        let root_prefix = normalize_root_prefix(&self.config.root);
        self.collect_paths_inner(dir, &root_prefix, out)
    }

    fn collect_paths_inner(
        &self,
        dir: &Path,
        root_prefix: &str,
        out: &mut Vec<(PathBuf, String)>,
    ) -> Result<(), FileError> {
        // Collect (sort-key, path) once so sorting doesn't re-allocate an
        // OsString per comparison.
        let mut entries: Vec<(std::ffi::OsString, PathBuf)> = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            entries.push((entry.file_name(), entry.path()));
        }
        // Sort for deterministic ordering
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        for (_name, path) in entries {
            if path.is_dir() {
                let dir_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if self
                    .config
                    .exclude_dirs
                    .iter()
                    .any(|ex| dir_name.eq_ignore_ascii_case(ex))
                {
                    continue;
                }
                if self
                    .config
                    .exclude_dir_patterns
                    .iter()
                    .any(|pat| glob_match(pat, dir_name))
                {
                    continue;
                }
                // Root-anchored excludes: only when this dir is a direct child of
                // the workspace root (its relative path has no separator).
                let rel = compute_logical_path_with_root(&path, root_prefix);
                if !rel.contains('/')
                    && self
                        .config
                        .exclude_root_dirs
                        .iter()
                        .any(|ex| dir_name.eq_ignore_ascii_case(ex))
                {
                    continue;
                }
                if let Err(e) = self.collect_paths_inner(&path, root_prefix, out) {
                    eprintln!("warn: skipping {}: {}", path.display(), e);
                }
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
            if self.config.max_file_size > 0
                && let Ok(meta) = path.metadata()
                && meta.len() > self.config.max_file_size
            {
                continue;
            }

            // Compute logical path relative to root
            let logical_path = compute_logical_path_with_root(&path, root_prefix);
            out.push((path, logical_path));
        }
        Ok(())
    }

    pub fn parse_single_file(&mut self, path: &Path) -> Result<ParsedFile, FileError> {
        let content = read_text(path)?;
        let logical_path = compute_logical_path(path, &self.config.root);
        match parse_string(&content, &self.string_table) {
            Ok(parsed) => Ok(ParsedFile {
                path: path.to_path_buf(),
                logical_path,
                arena: parsed.arena,
                root_children: parsed.root_children,
                errors: parsed.errors,
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
    compute_logical_path_with_root(path, &normalize_root_prefix(root))
}

/// Normalise `root` to a forward-slash, trailing-slash prefix once, so callers
/// that strip many paths against the same root don't redo the work per file.
fn normalize_root_prefix(root: &Path) -> String {
    let s = normalize_slashes(root.to_string_lossy());
    if s.ends_with('/') {
        s.into_owned()
    } else {
        format!("{}/", s)
    }
}

/// Like [`compute_logical_path`] but takes a root prefix already normalised by
/// [`normalize_root_prefix`].
fn compute_logical_path_with_root(path: &Path, root_prefix: &str) -> String {
    let path_str = normalize_slashes(path.to_string_lossy());

    if let Some(rel) = path_str.strip_prefix(root_prefix) {
        rel.to_string()
    } else {
        // fallback: just the file name
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string()
    }
}

/// Convert backslashes to forward slashes, avoiding a full scan/allocation when
/// the string contains none (the common case on Unix).
fn normalize_slashes(s: std::borrow::Cow<'_, str>) -> std::borrow::Cow<'_, str> {
    if s.contains('\\') {
        std::borrow::Cow::Owned(s.replace('\\', "/"))
    } else {
        s
    }
}

/// Parse a Paradox `.mod` descriptor file (plain key=value Paradox script).
///
/// Mirrors F# FileManager.fs:91-125: extracts `name`, `path`, and
/// `replace_path` entries.
pub fn parse_mod_descriptor(path: &Path) -> Result<ModDescriptor, FileError> {
    let raw = read_text(path)?;
    // Strip UTF-8 BOM (U+FEFF) so the first key isn't parsed as "\u{FEFF}name".
    let content = raw.strip_prefix('\u{FEFF}').unwrap_or(&raw);
    Ok(parse_mod_descriptor_str(content))
}

fn parse_mod_descriptor_str(content: &str) -> ModDescriptor {
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
            let val = descriptor_value(v);
            match key {
                "name" => name = val,
                "path" | "archive" => mod_path = Some(val),
                "replace_path" => replace_paths.push(val),
                _ => {}
            }
        }
    }

    ModDescriptor {
        name,
        path: mod_path,
        replace_paths,
    }
}

/// Extract a `.mod` value. A quoted value is the text between the quotes, so a
/// trailing inline comment or an `=` inside the quotes is handled correctly
/// (`replace_path = "common/ideas" # keep` -> `common/ideas`). An unquoted value
/// runs up to an inline `#` comment. The old `trim_matches('"')` left the closing
/// quote in place whenever anything followed it.
fn descriptor_value(v: &str) -> String {
    let v = v.trim();
    if let Some(rest) = v.strip_prefix('"') {
        match rest.split_once('"') {
            Some((inner, _)) => inner.to_string(),
            None => rest.to_string(),
        }
    } else {
        v.split('#').next().unwrap_or(v).trim().to_string()
    }
}

// ── Multi-mod expansion ───────────────────────────────────────────────────────

/// A resolved mod entry: its descriptor plus the on-disk root directory.
#[derive(Debug, Clone)]
pub struct ResolvedMod {
    pub descriptor: ModDescriptor,
    /// Absolute path to the mod root directory.
    pub root: PathBuf,
}

/// Scan a `MultipleMod` workspace directory for `.mod` descriptors and resolve
/// each to a concrete mod root.
///
/// Mirrors F# FileManager.fs:64-90: reads every `*.mod` file inside the
/// `mod/` (or `mods/`) subfolder, parses it, and returns a `ResolvedMod` for
/// each descriptor whose `path` resolves to an existing directory.
///
/// `workspace` must be the directory that `classify_directory` returned
/// `MultipleMod` for.
pub fn expand_multiple_mods(workspace: &Path) -> Vec<ResolvedMod> {
    let mut out = Vec::new();

    for mod_folder_name in &["mod", "mods"] {
        let mod_folder = workspace.join(mod_folder_name);
        if !mod_folder.is_dir() {
            continue;
        }

        let entries = match std::fs::read_dir(&mod_folder) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .extension()
                .map(|e| e.eq_ignore_ascii_case("mod"))
                .unwrap_or(false)
                && let Ok(desc) = parse_mod_descriptor(&path)
                && let Some(mod_path) = &desc.path
            {
                // `path` can be relative (to the workspace) or absolute
                let root = if std::path::Path::new(mod_path).is_absolute() {
                    PathBuf::from(mod_path)
                } else {
                    workspace.join(mod_path)
                };
                if root.is_dir() {
                    out.push(ResolvedMod {
                        descriptor: desc,
                        root,
                    });
                }
            }
        }
    }

    // Sort by name for deterministic ordering
    out.sort_by(|a, b| a.descriptor.name.cmp(&b.descriptor.name));
    out
}

/// Discover files across multiple mods, honouring `replace_path`.
///
/// Mirrors F# FileManager.fs:91-147:
/// * Mods are layered: later mods in `mods` take priority over earlier ones
///   (typically the caller orders them from lowest to highest priority).
/// * A mod's `replace_path` entries suppress *all* files whose logical path
///   starts with that prefix that were contributed by lower-priority sources
///   (including vanilla).
///
/// Returns `(mod_root, files_from_that_root)` pairs so callers know the origin.
pub fn discover_files_multi_mod(
    vanilla_root: Option<&Path>,
    mods: &[ResolvedMod],
    include_dirs: &[String],
) -> Vec<(PathBuf, String)> {
    // Collect (logical_path, absolute_path, source_priority) triples.
    // Higher priority index wins.
    use std::collections::HashMap;

    let mut best: HashMap<String, (PathBuf, usize)> = HashMap::new();

    // Build ordered list: vanilla is priority 0, mods are 1..=n
    let mut sources: Vec<(usize, &Path, &[String])> = Vec::new();

    if let Some(v) = vanilla_root {
        sources.push((0, v, include_dirs));
    }
    for (i, m) in mods.iter().enumerate() {
        sources.push((i + 1, &m.root, include_dirs));
    }

    // Collect candidate files from all sources
    for (priority, root, dirs) in &sources {
        let root_prefix = normalize_root_prefix(root);
        for include_dir in *dirs {
            let dir = if *include_dir == "." {
                root.to_path_buf()
            } else {
                root.join(include_dir)
            };
            if !dir.is_dir() {
                continue;
            }
            collect_files_recursive(&dir, &root_prefix, *priority, &mut best);
        }
    }

    // Apply replace_path suppression: for each mod (in priority order, highest
    // first), any file whose logical path starts with a replace_path prefix and
    // originates from a *lower* priority source is removed.
    // Lowercase each logical path once, rather than per replace_path entry below.
    let logical_lower: HashMap<String, String> = best
        .keys()
        .map(|k| (k.clone(), k.to_ascii_lowercase()))
        .collect();
    for (i, m) in mods.iter().enumerate().rev() {
        let mod_priority = i + 1;
        for rp in &m.descriptor.replace_paths {
            // Normalize: backslash → slash (Windows-authored .mod files), trim
            // leading/trailing slashes, then lowercase for case-insensitive match.
            let prefix_lower = rp.replace('\\', "/").trim_matches('/').to_ascii_lowercase();
            let prefix_lower_slash = format!("{}/", prefix_lower);
            best.retain(|logical, (_path, file_prio)| {
                // If the file's logical path is under this replace_path and
                // comes from a lower-priority source → suppress it.
                let ll = &logical_lower[logical.as_str()];
                let under_prefix = *ll == prefix_lower || ll.starts_with(&prefix_lower_slash);
                if under_prefix && *file_prio < mod_priority {
                    return false;
                }
                true
            });
        }
    }

    let mut result: Vec<(PathBuf, String)> = best
        .into_iter()
        .map(|(logical, (abs_path, _prio))| (abs_path, logical))
        .collect();
    result.sort_by(|a, b| a.1.cmp(&b.1));
    result
}

fn collect_files_recursive(
    dir: &Path,
    root_prefix: &str,
    priority: usize,
    out: &mut std::collections::HashMap<String, (PathBuf, usize)>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files_recursive(&path, root_prefix, priority, out);
        } else {
            let logical = compute_logical_path_with_root(&path, root_prefix);
            // Higher priority wins
            let entry = out.entry(logical).or_insert((path.clone(), priority));
            if priority > entry.1 {
                *entry = (path, priority);
            }
        }
    }
}

/// Recursively collect every file under `root` whose extension is in
/// `extensions`, skipping engine/IDE directories and free-form text files.
///
/// This is the whole-tree walker used by the LSP full-workspace pass. The skip
/// lists (directories and free-form filenames) come from
/// `FileManagerConfig::default()` so they are defined in exactly one place and
/// stay consistent with the CLI's `discover_and_parse`. `extra_file_globs` and
/// `extra_dir_globs` layer on top of those defaults (they extend, never
/// replace, the engine baseline). Each directory's entries are sorted, so the
/// traversal order is deterministic and independent of the filesystem's
/// `read_dir` order.
pub fn walk_workspace_files(
    root: &Path,
    extensions: &[&str],
    extra_file_globs: &[String],
    extra_dir_globs: &[String],
) -> Vec<PathBuf> {
    let cfg = FileManagerConfig::default();
    let root_prefix = normalize_root_prefix(root);
    let mut out = Vec::new();
    walk_workspace_inner(
        root,
        &root_prefix,
        extensions,
        &cfg,
        extra_file_globs,
        extra_dir_globs,
        &mut out,
    );
    out
}

fn walk_workspace_inner(
    dir: &Path,
    root_prefix: &str,
    extensions: &[&str],
    cfg: &FileManagerConfig,
    extra_file_globs: &[String],
    extra_dir_globs: &[String],
    out: &mut Vec<PathBuf>,
) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    // Sort each directory's entries so the scan order matches the CLI's
    // `collect_paths` and stays stable across filesystems. Collect the sort key
    // once so sorting doesn't re-allocate an OsString per comparison.
    let mut entries: Vec<(std::ffi::OsString, PathBuf)> =
        rd.flatten().map(|e| (e.file_name(), e.path())).collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    for (_name, path) in entries {
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            // Root-anchored excludes apply only to direct children of the root.
            let rel = compute_logical_path_with_root(&path, root_prefix);
            let root_level = !rel.contains('/');
            let skip = cfg
                .exclude_dirs
                .iter()
                .any(|ex| name.eq_ignore_ascii_case(ex))
                || cfg
                    .exclude_dir_patterns
                    .iter()
                    .any(|pat| glob_match(pat, name))
                || extra_dir_globs.iter().any(|pat| glob_match(pat, name))
                || (root_level
                    && cfg
                        .exclude_root_dirs
                        .iter()
                        .any(|ex| name.eq_ignore_ascii_case(ex)));
            if !skip {
                walk_workspace_inner(
                    &path,
                    root_prefix,
                    extensions,
                    cfg,
                    extra_file_globs,
                    extra_dir_globs,
                    out,
                );
            }
        } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if !extensions.contains(&ext) {
                continue;
            }
            let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            // Engine baseline (Changelog.txt, README.*, LICENSE.*, *.md) lives in
            // the default config's exclude_patterns; user globs extend it.
            let skip = cfg
                .exclude_patterns
                .iter()
                .any(|pat| glob_match(pat, file_name))
                || extra_file_globs
                    .iter()
                    .any(|pat| glob_match(pat, file_name));
            if !skip {
                out.push(path);
            }
        }
    }
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
    // Fast path for *.ext — only valid when the remainder has no further wildcards.
    if let Some(suffix) = pattern.strip_prefix('*')
        && !suffix.contains(['*', '?'])
    {
        return text.ends_with(suffix);
    }
    // Fast path for prefix* — only valid when the prefix has no wildcards.
    if let Some(prefix) = pattern.strip_suffix('*')
        && !prefix.contains(['*', '?'])
    {
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
    // Single rolling row instead of an (m+1)x(n+1) `Vec<Vec<bool>>` per call
    // (#17): dp[j] holds whether p[0..i] matches t[0..j]; `prev_diag` carries the
    // dp[i-1][j-1] value as we sweep j left-to-right.
    let mut dp = vec![false; n + 1];
    dp[0] = true; // empty pattern matches empty text
    for i in 1..=m {
        let mut prev_diag = dp[0]; // dp[i-1][0]
        // dp[i][0] is true only if every pattern char so far is '*'.
        dp[0] = dp[0] && p[i - 1] == '*';
        for j in 1..=n {
            let above = dp[j]; // dp[i-1][j], before overwrite
            if p[i - 1] == '*' {
                dp[j] = dp[j] || dp[j - 1]; // dp[i-1][j] || dp[i][j-1]
            } else if p[i - 1] == '?' || p[i - 1] == t[j - 1] {
                dp[j] = prev_diag; // dp[i-1][j-1]
            } else {
                dp[j] = false;
            }
            prev_diag = above;
        }
    }
    dp[n]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mod_descriptor_robust_values() {
        // #213: trailing comments, quoted '=', and unquoted values must parse.
        let d = parse_mod_descriptor_str(
            "name = \"Test = Mod\"\n\
             path = \"mod/root\"  # the root\n\
             replace_path = \"common/ideas\"\n\
             replace_path = \"common/foo=bar\"\n\
             replace_path = \"events\" # keep vanilla out\n\
             replace_path = common/units\n\
             replace_path = common/raids # bare with comment\n\
             # a comment line\n\
             dependencies = { \"ModA\" \"ModB\" }\n",
        );
        assert_eq!(d.name, "Test = Mod");
        assert_eq!(d.path.as_deref(), Some("mod/root"));
        assert_eq!(
            d.replace_paths,
            vec![
                "common/ideas",
                "common/foo=bar",
                "events",
                "common/units",
                "common/raids",
            ]
        );
    }

    #[test]
    fn mod_descriptor_clean_lines_unchanged() {
        // The common case (clean quoted lines, as in the Millennium Dawn
        // descriptor) must parse identically to before.
        let d = parse_mod_descriptor_str(
            "name=\"Millennium Dawn\"\nreplace_path = \"common/ideas\"\nreplace_path = \"events\"\n",
        );
        assert_eq!(d.name, "Millennium Dawn");
        assert_eq!(d.replace_paths, vec!["common/ideas", "events"]);
    }

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
    fn glob_match_multi_wildcard() {
        // *foo* must not take the *.ext fast path and treat "foo*" as a literal suffix.
        assert!(glob_match("*foo*", "barfoobar"));
        assert!(glob_match("*foo*", "foo"));
        assert!(glob_match("*foo*", "xfoox"));
        assert!(!glob_match("*foo*", "bar"));
        // prefix* fast path must not trigger when the prefix itself contains ?.
        assert!(glob_match("fo?*", "foobar"));
        assert!(!glob_match("fo?*", "fo")); // needs at least one char after "fo"
    }

    #[test]
    fn default_excludes_skip_changelog_and_markdown() {
        let cfg = FileManagerConfig::default();
        assert!(cfg.exclude_patterns.iter().any(|p| p == "Changelog.txt"));
        assert!(cfg.exclude_patterns.iter().any(|p| p == "*.md"));
    }

    #[test]
    fn exclude_dir_patterns_skips_matching_dirs() {
        use std::fs;
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let root = tmp.path();

        // Layout:
        //   root/common/foo.txt          (include)
        //   root/temp/skipme.txt         (skip: dir matches "temp")
        //   root/template/keepme.txt     (include: dir does NOT match "temp")
        //   root/notes/Changelog.txt     (skip: filename matches)
        for rel in [
            "common/foo.txt",
            "temp/skipme.txt",
            "template/keepme.txt",
            "notes/Changelog.txt",
        ] {
            if let Some(parent) = std::path::Path::new(rel).parent() {
                fs::create_dir_all(root.join(parent)).unwrap();
            }
            fs::write(root.join(rel), "").unwrap();
        }

        let cfg = FileManagerConfig {
            root: root.to_path_buf(),
            include_dirs: vec![".".into()],
            exclude_dir_patterns: vec!["temp".into()],
            ..Default::default()
        };

        let fm = FileManager::new(cfg);
        let mut paths = Vec::new();
        fm.collect_paths(root, &mut paths).unwrap();
        let names: Vec<String> = paths.iter().map(|(_, lp)| lp.clone()).collect();

        assert!(names.iter().any(|n| n.ends_with("common/foo.txt")));
        assert!(
            names.iter().any(|n| n.ends_with("template/keepme.txt")),
            "template/ should NOT match the exact 'temp' pattern"
        );
        assert!(
            !names.iter().any(|n| n.ends_with("temp/skipme.txt")),
            "temp/ should be skipped by exclude_dir_patterns"
        );
        assert!(
            !names.iter().any(|n| n.ends_with("notes/Changelog.txt")),
            "Changelog.txt should be skipped by default exclude_patterns"
        );
    }

    /// A root-level `resources/` is dev scratch the game never loads, but
    /// `common/resources/` defines the `resource` type (oil, steel, …). The
    /// default excludes must skip the former and keep the latter, on BOTH the
    /// CLI (`collect_paths`) and LSP (`walk_workspace_files`) discovery paths.
    #[test]
    fn root_resources_skipped_but_common_resources_indexed() {
        use std::fs;
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let root = tmp.path();
        for rel in ["common/resources/00_resources.txt", "resources/scratch.txt"] {
            fs::create_dir_all(root.join(Path::new(rel).parent().unwrap())).unwrap();
            fs::write(root.join(rel), "").unwrap();
        }

        // CLI path.
        let fm = FileManager::new(FileManagerConfig {
            root: root.to_path_buf(),
            include_dirs: vec![".".into()],
            ..Default::default()
        });
        let mut paths = Vec::new();
        fm.collect_paths(root, &mut paths).unwrap();
        let cli: Vec<String> = paths.iter().map(|(_, lp)| lp.clone()).collect();
        assert!(
            cli.iter()
                .any(|n| n.ends_with("common/resources/00_resources.txt")),
            "common/resources must be indexed: {cli:?}"
        );
        assert!(
            !cli.iter().any(|n| n.ends_with("resources/scratch.txt")),
            "root resources/ must be skipped: {cli:?}"
        );

        // LSP whole-tree path.
        let lsp = walk_workspace_files(root, &["txt"], &[], &[]);
        let lsp: Vec<String> = lsp
            .iter()
            .map(|p| normalize_slashes(p.to_string_lossy()).into_owned())
            .collect();
        assert!(
            lsp.iter()
                .any(|n| n.ends_with("common/resources/00_resources.txt")),
            "common/resources must be walked: {lsp:?}"
        );
        assert!(
            !lsp.iter().any(|n| n.ends_with("resources/scratch.txt")),
            "root resources/ must be skipped by whole-tree walk: {lsp:?}"
        );
    }

    #[test]
    fn classify_ext() {
        assert_eq!(classify_extension(Path::new("foo.txt")), FileKind::Script);
        assert_eq!(classify_extension(Path::new("foo.gui")), FileKind::Script);
        assert_eq!(classify_extension(Path::new("foo.gfx")), FileKind::Script);
        assert_eq!(classify_extension(Path::new("foo.asset")), FileKind::Script);
        assert_eq!(
            classify_extension(Path::new("foo.yml")),
            FileKind::Localisation
        );
        assert_eq!(
            classify_extension(Path::new("foo.csv")),
            FileKind::Localisation
        );
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

    // ── CP-1252 / encoding tests ──────────────────────────────────────────────

    #[test]
    fn cp1252_e_acute_0xe9() {
        // 0xE9 in CP-1252 is U+00E9 (é), same as Latin-1 for bytes >= 0xA0
        assert_eq!(cp1252_byte(0xE9), 'é');
    }

    #[test]
    fn cp1252_euro_sign_0x80() {
        // 0x80 in CP-1252 is the Euro sign U+20AC — NOT U+0080
        assert_eq!(cp1252_byte(0x80), '€');
    }

    #[test]
    fn cp1252_ascii_passthrough() {
        assert_eq!(cp1252_byte(b'A'), 'A');
        assert_eq!(cp1252_byte(b'\n'), '\n');
    }

    #[test]
    fn read_text_cp1252_bytes_via_tmpfile() {
        use std::io::Write as _;

        // Build a sequence: "caf" + 0xE9 (é in CP-1252) + "\n"
        let bytes: &[u8] = b"caf\xE9\n";
        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        tmp.write_all(bytes).expect("write");

        let text = read_text(tmp.path()).expect("read_text");
        assert_eq!(text, "caf\u{E9}\n", "0xE9 should decode as é (U+00E9)");
    }

    // ── multi-mod expand / replace_path tests ─────────────────────────────────

    #[test]
    fn multi_mod_replace_path_suppresses_vanilla() {
        use std::collections::HashMap;
        use std::fs;

        // Create a tiny temp filesystem:
        //   workspace/
        //     vanilla/common/foo.txt
        //     moda/common/foo.txt      (replaces common/)
        //     modb/events/bar.txt
        let workspace = tempfile::TempDir::new().expect("tmpdir");
        let wsp = workspace.path();

        let vanilla = wsp.join("vanilla");
        fs::create_dir_all(vanilla.join("common")).unwrap();
        fs::write(vanilla.join("common/foo.txt"), "vanilla").unwrap();

        let moda_root = wsp.join("moda");
        fs::create_dir_all(moda_root.join("common")).unwrap();
        fs::write(moda_root.join("common/foo.txt"), "moda").unwrap();

        let modb_root = wsp.join("modb");
        fs::create_dir_all(modb_root.join("events")).unwrap();
        fs::write(modb_root.join("events/bar.txt"), "modb").unwrap();

        let mods = vec![
            ResolvedMod {
                descriptor: ModDescriptor {
                    name: "ModA".into(),
                    path: Some(moda_root.to_str().unwrap().to_string()),
                    replace_paths: vec!["common".into()],
                },
                root: moda_root.clone(),
            },
            ResolvedMod {
                descriptor: ModDescriptor {
                    name: "ModB".into(),
                    path: Some(modb_root.to_str().unwrap().to_string()),
                    replace_paths: vec![],
                },
                root: modb_root.clone(),
            },
        ];

        let include_dirs = vec!["common".to_string(), "events".to_string()];
        let files = discover_files_multi_mod(Some(&vanilla), &mods, &include_dirs);

        // Build logical_path → content map
        let by_logical: HashMap<String, String> = files
            .iter()
            .map(|(abs, logical)| {
                let content = fs::read_to_string(abs).unwrap_or_default();
                (logical.clone(), content)
            })
            .collect();

        // Vanilla's common/foo.txt should be suppressed by ModA's replace_path
        assert_eq!(
            by_logical.get("common/foo.txt").map(|s| s.as_str()),
            Some("moda"),
            "ModA's common/foo.txt should win; vanilla suppressed by replace_path"
        );

        // ModB's events/bar.txt should be present
        assert!(
            by_logical.contains_key("events/bar.txt"),
            "ModB events/bar.txt should be present"
        );
    }

    #[test]
    fn walk_workspace_files_returns_sorted_order() {
        // The workspace scan must process files in a deterministic, sorted order
        // independent of the filesystem's read_dir order, so editor diagnostics
        // and indexing are reproducible run to run.
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let root = tmp.path();
        for name in ["zebra.txt", "alpha.txt", "middle.txt"] {
            std::fs::write(root.join(name), "").unwrap();
        }
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub").join("aaa.txt"), "").unwrap();

        let files = walk_workspace_files(root, &["txt"], &[], &[]);
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        let pos = |n: &str| names.iter().position(|x| x == n).expect("file present");
        assert!(pos("alpha.txt") < pos("middle.txt"), "got: {:?}", names);
        assert!(pos("middle.txt") < pos("zebra.txt"), "got: {:?}", names);
    }
}
