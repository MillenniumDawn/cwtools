use cwtools_parser::ast::{Arena, Child};
use cwtools_parser::parser::parse_string;
use cwtools_string_table::string_table::StringTable;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum FileError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Parse error: {0}")]
    Parse(String),
    #[error("Pattern error: {0}")]
    Pattern(String),
}

/// A discovered file with its parsed AST.
pub struct ParsedFile {
    pub path: PathBuf,
    pub arena: Arena,
    pub root_children: Vec<Child>,
}

/// Configuration for file discovery.
pub struct FileManagerConfig {
    /// Root directory to search.
    pub root: PathBuf,
    /// Subdirectories to include (e.g., "common", "events").
    pub include_dirs: Vec<String>,
    /// Glob patterns for files (e.g., "*.txt").
    pub file_patterns: Vec<String>,
    /// Patterns to exclude.
    pub exclude_patterns: Vec<String>,
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

    /// Discover and parse all matching files under the configured root.
    pub fn discover_and_parse(&mut self) -> Result<Vec<ParsedFile>, FileError> {
        let mut files = Vec::new();
        let include_dirs: Vec<String> = self.config.include_dirs.clone();
        let root = self.config.root.clone();

        for include_dir in include_dirs {
            let dir = root.join(include_dir);
            if !dir.exists() {
                continue;
            }
            self.walk_dir(&dir, &mut files)?;
        }

        Ok(files)
    }

    fn walk_dir(&mut self, dir: &Path, out: &mut Vec<ParsedFile>) -> Result<(), FileError> {
        let entries = std::fs::read_dir(dir)?;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();

            if path.is_dir() {
                self.walk_dir(&path, out)?;
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

            // Parse file
            let content = std::fs::read_to_string(&path)?;
            match parse_string(&content, &self.string_table) {
                Ok(parsed) => {
                    out.push(ParsedFile {
                        path,
                        arena: parsed.arena,
                        root_children: parsed.root_children,
                    });
                }
                Err(e) => {
                    return Err(FileError::Parse(format!("{}: {}", path.display(), e)));
                }
            }
        }
        Ok(())
    }

    pub fn parse_single_file(&mut self, path: &Path) -> Result<ParsedFile, FileError> {
        let content = std::fs::read_to_string(path)?;
        match parse_string(&content, &self.string_table) {
            Ok(parsed) => Ok(ParsedFile {
                path: path.to_path_buf(),
                arena: parsed.arena,
                root_children: parsed.root_children,
            }),
            Err(e) => Err(FileError::Parse(format!("{}: {}", path.display(), e))),
        }
    }
}

/// Simple glob matching (just * and ? wildcards).
fn glob_match(pattern: &str, text: &str) -> bool {
    // For now, simple suffix match for *.ext patterns
    if pattern.starts_with("*.") {
        let ext = &pattern[1..];
        return text.ends_with(ext);
    }
    // Exact match
    pattern == text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matching() {
        assert!(glob_match("*.txt", "foo.txt"));
        assert!(!glob_match("*.txt", "foo.png"));
    }
}
