//! Suggested-fix payloads carried by diagnostics.
//!
//! A [`SuggestedFix`] is pure metadata attached to a diagnostic at its emit
//! site, where the AST node (and hence its [`SourceRange`]) is still in scope —
//! the span can't be reconstructed later from a diagnostic's start position
//! alone. The engine never applies a fix; the CLI `fix` subcommand and the LSP
//! code-action provider consume these.
//!
//! Ranges use the same convention as [`SourcePos`]: 1-based `line`, 0-based
//! char `col`. Loc diagnostics (1-based columns) convert before building a fix.
//! v1 is single-span, single-line edits only.

use crate::ast::{SourcePos, SourceRange};
use smallvec::SmallVec;

/// A single-span text replacement. An empty `replacement` deletes the span.
#[derive(Debug, Clone, PartialEq)]
pub struct SpanEdit {
    pub range: SourceRange,
    pub replacement: String,
}

/// A named set of edits that resolves one diagnostic. v1 fixes carry exactly one
/// edit; the inline `SmallVec` keeps the common case allocation-free.
#[derive(Debug, Clone, PartialEq)]
pub struct SuggestedFix {
    pub title: String,
    pub edits: SmallVec<[SpanEdit; 1]>,
}

impl SuggestedFix {
    /// A one-edit fix replacing `range` with `replacement`.
    pub fn replace(
        title: impl Into<String>,
        range: SourceRange,
        replacement: impl Into<String>,
    ) -> Self {
        SuggestedFix {
            title: title.into(),
            edits: smallvec::smallvec![SpanEdit {
                range,
                replacement: replacement.into(),
            }],
        }
    }

    /// A one-edit deletion of `range` (empty replacement).
    pub fn delete(title: impl Into<String>, range: SourceRange) -> Self {
        Self::replace(title, range, String::new())
    }
}

/// Range of a key token that begins at `start` and is `char_len` characters
/// long, on one line. Targets a rename/replacement at a block or leaf key
/// without touching its value (e.g. CW253 `set_empire_name` -> `set_name`).
pub fn key_token_range(start: SourcePos, char_len: usize) -> SourceRange {
    SourceRange {
        start,
        end: SourcePos {
            line: start.line,
            col: start.col.saturating_add(char_len as u16),
        },
    }
}

/// Byte offset of the start of each line, indexed by 0-based line number (source
/// line 1 is index 0). Used to convert a (line, char-col) [`SourcePos`] into a
/// byte offset when applying an edit.
pub fn line_start_bytes(text: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    starts
}

/// Byte offset of a [`SourcePos`] (1-based `line`, 0-based char `col`). Walks
/// `col` characters from the line start so a multibyte char counts as one
/// column. A position past the line's end (or the text) clamps to the line end /
/// text end; `col` at the line's newline resolves to the newline's offset.
pub fn pos_to_byte(text: &str, line_starts: &[usize], pos: SourcePos) -> usize {
    let line_idx = pos.line.saturating_sub(1) as usize;
    let Some(&line_start) = line_starts.get(line_idx) else {
        return text.len();
    };
    let mut byte = line_start;
    for (col, ch) in (0_u16..).zip(text[line_start..].chars()) {
        if col >= pos.col || ch == '\n' {
            break;
        }
        byte += ch.len_utf8();
    }
    byte
}

/// Apply single-span edits to `text`, returning the new text. Edits are resolved
/// to byte ranges, sorted by start descending, and applied later-first so earlier
/// offsets stay valid. Overlaps are not checked here — the caller filters them
/// (the CLI `fix` subcommand skips-and-warns per file); the single-edit fixtures
/// never overlap.
pub fn apply_edits(text: &str, edits: &[SpanEdit]) -> String {
    let starts = line_start_bytes(text);
    let mut ranges: Vec<(usize, usize, &str)> = edits
        .iter()
        .map(|e| {
            (
                pos_to_byte(text, &starts, e.range.start),
                pos_to_byte(text, &starts, e.range.end),
                e.replacement.as_str(),
            )
        })
        .collect();
    ranges.sort_by_key(|r| std::cmp::Reverse(r.0));
    let mut out = text.to_string();
    for (s, e, repl) in ranges {
        if s <= e && e <= out.len() {
            out.replace_range(s..e, repl);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(line: u32, col: u16) -> SourcePos {
        SourcePos { line, col }
    }

    #[test]
    fn pos_to_byte_walks_chars_including_multibyte() {
        let text = "ab\ncafé x\n";
        let starts = line_start_bytes(text);
        // line 1, col 2 -> just past "ab" (byte 2).
        assert_eq!(pos_to_byte(text, &starts, pos(1, 2)), 2);
        // line 2 starts at byte 3; col 4 is past "café" (é is 2 bytes) -> the space.
        assert_eq!(pos_to_byte(text, &starts, pos(2, 4)), 3 + 5);
        // col past the line clamps at the newline, not into the next line.
        assert_eq!(pos_to_byte(text, &starts, pos(1, 99)), 2);
    }

    #[test]
    fn apply_single_replacement() {
        let text = "set_empire_name = { }\n";
        let edit = SpanEdit {
            range: key_token_range(pos(1, 0), "set_empire_name".len()),
            replacement: "set_name".to_string(),
        };
        assert_eq!(apply_edits(text, &[edit]), "set_name = { }\n");
    }

    #[test]
    fn apply_multiple_edits_is_order_independent() {
        // Two edits on one line applied later-first: the earlier edit's offsets
        // stay valid regardless of the order they appear in the slice.
        let text = "aaaa bbbb\n";
        let e1 = SpanEdit {
            range: SourceRange {
                start: pos(1, 0),
                end: pos(1, 4),
            },
            replacement: "X".to_string(),
        };
        let e2 = SpanEdit {
            range: SourceRange {
                start: pos(1, 5),
                end: pos(1, 9),
            },
            replacement: "Y".to_string(),
        };
        assert_eq!(apply_edits(text, &[e1.clone(), e2.clone()]), "X Y\n");
        assert_eq!(apply_edits(text, &[e2, e1]), "X Y\n");
    }
}
