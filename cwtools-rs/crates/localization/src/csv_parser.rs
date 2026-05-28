//! CSV localisation parser (CK2-style).
//!
//! CK2 uses `;`-delimited rows with multiple languages per row:
//!   key;english;french;german;spanish;...
//!
//! This is a simple hand-written parser.

use crate::commands::{LocEntry, Position};

/// Parse a CSV localisation file.
///
/// Returns a map: column_index → Vec<LocEntry>.
/// Caller determines which column corresponds to which language.
///
/// # Arguments
/// * `text` – raw CSV text
/// * `name` – file name for error reporting
/// * `language_columns` – map: column_index → Lang (Lang is defined in commands.rs)
pub fn parse_csv_loc(
    text: &str,
    name: &str,
) -> Vec<LocEntry> {
    let mut entries = Vec::new();

    for (line_num, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Simple split on ';' (CK2 doesn't use quoted cells with ';')
        let parts: Vec<&str> = trimmed.split(';').collect();
        if parts.is_empty() {
            continue;
        }

        let key = parts[0].trim().to_string();
        if key.is_empty() {
            continue;
        }

        // For now, just store all columns as a single description string
        let desc = parts
            .iter()
            .skip(1)
            .copied()
            .collect::<Vec<_>>()
            .join(";");

        let position = Position::new(name, line_num + 1, 1);

        entries.push(LocEntry {
            key,
            value: None,
            desc,
            position,
            error_range: None,
            refs: Vec::new(),
            commands: Vec::new(),
            jomini_commands: Vec::new(),
        });
    }

    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_csv() {
        let text = r#"key1;English text;French text
key2;More English;More French
# comment
key3;Last entry;"#;

        let entries = parse_csv_loc(text, "test.csv");
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].key, "key1");
        assert_eq!(entries[0].desc, "English text;French text");
    }

    #[test]
    fn test_empty_lines() {
        let entries = parse_csv_loc("\n\n#comment\n\n", "test.csv");
        assert!(entries.is_empty());
    }
}
