//! Positions in a document without walking the whole text.
//!
//! The editor has three position systems that all need converting between:
//! egui's `TextEdit` cursor is a char index over the whole buffer, language
//! servers speak (line, UTF-16 column), and `LayoutJob` sections and string
//! slicing use byte offsets. Every conversion used to scan from the start of
//! the text, several times per frame, which on a multi-megabyte file was a
//! visible part of each keystroke. A `LineIndex` records where every line
//! starts in both bytes and chars, so a conversion is a binary search plus a
//! scan of one line.
//!
//! The index is rebuilt after an edit, one pass over the bytes. That is
//! still linear in document size but it is one cheap pass instead of many
//! char-decoding ones, and it is the price of keeping the buffer a plain
//! `String`; an incrementally patched index is a later step, once the
//! editor owns its edits instead of egui.

use std::ops::Range;

/// Where a line begins, in both units the editor needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineStart {
    pub byte: u32,
    pub chr: u32,
}

/// Start offsets of every line in a text. Line numbers are zero-based here;
/// the goto bar's one-based numbers are converted at the edge.
#[derive(Clone, Debug, Default)]
pub struct LineIndex {
    starts: Vec<LineStart>,
    total_bytes: u32,
    total_chars: u32,
    /// Longest line in bytes, its newline included. Free to keep during the
    /// build pass, and it is what tells a caller whether per-line work on this
    /// document is bounded at all: a file whose only newline is the last byte
    /// is one line several megabytes wide, and anything that lays out or
    /// parses a whole line at a time has to refuse it rather than try.
    max_line_bytes: u32,
}

impl LineIndex {
    /// One pass over the bytes. A char starts at every byte that is not a
    /// UTF-8 continuation byte, so char counting never decodes anything.
    pub fn build(text: &str) -> Self {
        let bytes = text.as_bytes();
        let mut starts = Vec::with_capacity(bytes.len() / 40 + 1);
        starts.push(LineStart { byte: 0, chr: 0 });
        let mut chars = 0u32;
        let mut max_line_bytes = 0u32;
        let mut line_start = 0usize;
        for (i, &b) in bytes.iter().enumerate() {
            if b & 0xC0 != 0x80 {
                chars += 1;
            }
            if b == b'\n' {
                starts.push(LineStart { byte: (i + 1) as u32, chr: chars });
                max_line_bytes = max_line_bytes.max((i + 1 - line_start) as u32);
                line_start = i + 1;
            }
        }
        // The text after the last newline is a line too, and on a file with no
        // newline at all it is the only one.
        max_line_bytes = max_line_bytes.max((bytes.len() - line_start) as u32);
        Self { starts, total_bytes: bytes.len() as u32, total_chars: chars, max_line_bytes }
    }

    /// Length of the longest line in bytes, its newline included.
    pub fn max_line_bytes(&self) -> usize {
        self.max_line_bytes as usize
    }

    /// Number of lines, counting a trailing newline as starting one more.
    pub fn line_count(&self) -> usize {
        self.starts.len()
    }

    pub fn char_count(&self) -> usize {
        self.total_chars as usize
    }

    /// Zero-based line containing byte offset `b`, clamped to the last line.
    pub fn line_of_byte(&self, b: usize) -> usize {
        let b = b.min(self.total_bytes as usize) as u32;
        self.starts.partition_point(|s| s.byte <= b).saturating_sub(1)
    }

    /// Zero-based line containing char index `c`, clamped to the last line.
    pub fn line_of_char(&self, c: usize) -> usize {
        let c = c.min(self.total_chars as usize) as u32;
        self.starts.partition_point(|s| s.chr <= c).saturating_sub(1)
    }

    /// Byte offset where zero-based `line` starts, clamped to the end.
    pub fn line_byte_start(&self, line: usize) -> usize {
        self.starts.get(line).map(|s| s.byte as usize).unwrap_or(self.total_bytes as usize)
    }

    /// Char index where zero-based `line` starts, clamped to the end.
    pub fn line_char_start(&self, line: usize) -> usize {
        self.starts.get(line).map(|s| s.chr as usize).unwrap_or(self.total_chars as usize)
    }

    /// Byte range of zero-based `line`, including its newline if it has one.
    pub fn line_bytes(&self, line: usize) -> Range<usize> {
        let s = self.line_byte_start(line);
        let e = self.line_byte_start(line + 1);
        s..e
    }

    /// Char index to byte offset, clamped to the text.
    pub fn char_to_byte(&self, text: &str, c: usize) -> usize {
        let c = c.min(self.total_chars as usize);
        let line = self.line_of_char(c);
        let start = &self.starts[line];
        let within = c - start.chr as usize;
        let line_text = &text[start.byte as usize..];
        line_text.char_indices().nth(within).map(|(b, _)| start.byte as usize + b).unwrap_or(text.len())
    }

    /// Byte offset to char index. `b` must lie on a char boundary; it is
    /// clamped to the text.
    pub fn byte_to_char(&self, text: &str, b: usize) -> usize {
        let b = b.min(text.len());
        let line = self.line_of_byte(b);
        let start = &self.starts[line];
        start.chr as usize + text[start.byte as usize..b].chars().count()
    }

    /// Char index to the LSP position (zero-based line, UTF-16 column).
    pub fn char_to_lsp(&self, text: &str, c: usize) -> (u32, u32) {
        let c = c.min(self.total_chars as usize);
        let line = self.line_of_char(c);
        let start = &self.starts[line];
        let within = c - start.chr as usize;
        let col: usize = text[start.byte as usize..].chars().take(within).map(|ch| ch.len_utf16()).sum();
        (line as u32, col as u32)
    }

    /// LSP position to byte offset, clamped to the line and the text. A
    /// column past the end of the line lands on the newline, matching what
    /// servers expect when they point at end-of-line.
    pub fn lsp_to_byte(&self, text: &str, line: u32, col: u32) -> usize {
        let line = line as usize;
        if line >= self.starts.len() {
            return text.len();
        }
        let range = self.line_bytes(line);
        let line_text = &text[range.clone()];
        let mut units = 0u32;
        for (b, ch) in line_text.char_indices() {
            if ch == '\n' || units >= col {
                return range.start + b;
            }
            units += ch.len_utf16() as u32;
        }
        range.end
    }
}

/// Non-overlapping matches of `query` in `text` as byte ranges. Case folding
/// is per char, so a needle never has to be the same byte length as what it
/// matches; the range always covers whole chars of the original text.
pub fn find_matches(text: &str, query: &str, case_insensitive: bool) -> Vec<(usize, usize)> {
    if query.is_empty() {
        return Vec::new();
    }
    let fold = |c: char| -> Vec<char> {
        if case_insensitive {
            c.to_lowercase().collect()
        } else {
            vec![c]
        }
    };
    let needle: Vec<char> = query.chars().flat_map(fold).collect();
    let mut out = Vec::new();
    let mut from = 0usize;
    'outer: while from < text.len() {
        let mut it = text[from..].char_indices();
        while let Some((rel, _)) = it.next() {
            let start = from + rel;
            // Try to match the needle starting at `start`.
            let mut ni = 0usize;
            for (b, ch) in text[start..].char_indices() {
                let folded = fold(ch);
                if needle[ni..].starts_with(&folded) {
                    ni += folded.len();
                    if ni == needle.len() {
                        let end = start + b + ch.len_utf8();
                        out.push((start, end));
                        from = end;
                        continue 'outer;
                    }
                } else {
                    break;
                }
            }
        }
        break;
    }
    out
}

/// Replace every match in one pass. Returns the new text and the count.
pub fn replace_all(text: &str, query: &str, replacement: &str, case_insensitive: bool) -> (String, usize) {
    let matches = find_matches(text, query, case_insensitive);
    if matches.is_empty() {
        return (text.to_string(), 0);
    }
    let mut out = String::with_capacity(text.len());
    let mut last = 0usize;
    for (s, e) in &matches {
        out.push_str(&text[last..*s]);
        out.push_str(replacement);
        last = *e;
    }
    out.push_str(&text[last..]);
    (out, matches.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_records_every_line_in_bytes_and_chars() {
        let text = "aa\n\u{2713}b\ncc";
        let ix = LineIndex::build(text);
        assert_eq!(ix.line_count(), 3);
        assert_eq!(ix.line_byte_start(0), 0);
        assert_eq!(ix.line_byte_start(1), 3);
        assert_eq!(ix.line_byte_start(2), 8);
        assert_eq!(ix.line_char_start(1), 3);
        assert_eq!(ix.line_char_start(2), 6);
        assert_eq!(ix.char_count(), 8);
        // Past the end clamps rather than panics.
        assert_eq!(ix.line_byte_start(99), text.len());
    }

    #[test]
    fn max_line_bytes_counts_the_newline_and_the_unterminated_last_line() {
        assert_eq!(LineIndex::build("aa\nbbbb").max_line_bytes(), 4);
        assert_eq!(LineIndex::build("aaaaa\nb").max_line_bytes(), 6);
        assert_eq!(LineIndex::build("").max_line_bytes(), 0);
        assert_eq!(LineIndex::build("\n").max_line_bytes(), 1);
        // No newline anywhere: the whole document is one line, which is the
        // shape the virtualised view has to cap rather than lay out.
        assert_eq!(LineIndex::build("abcdef").max_line_bytes(), 6);
    }

    #[test]
    fn trailing_newline_starts_an_empty_last_line() {
        let ix = LineIndex::build("a\n");
        assert_eq!(ix.line_count(), 2);
        assert_eq!(ix.line_byte_start(1), 2);
    }

    #[test]
    fn char_and_byte_conversions_agree_across_multibyte_text() {
        let text = "\u{2713}bc\nd\u{00e9}f";
        let ix = LineIndex::build(text);
        for (c, (b, _)) in text.char_indices().enumerate() {
            assert_eq!(ix.char_to_byte(text, c), b, "char {c}");
            assert_eq!(ix.byte_to_char(text, b), c, "byte {b}");
        }
        assert_eq!(ix.char_to_byte(text, 99), text.len());
        assert_eq!(ix.byte_to_char(text, 99), text.chars().count());
    }

    #[test]
    fn lsp_positions_use_utf16_columns() {
        let text = "ab\n\u{1F600}x\nend";
        let ix = LineIndex::build(text);
        // The emoji is one char, two UTF-16 units, four bytes.
        assert_eq!(ix.char_to_lsp(text, 3), (1, 0));
        assert_eq!(ix.char_to_lsp(text, 4), (1, 2));
        assert_eq!(ix.lsp_to_byte(text, 1, 2), 7);
        assert_eq!(ix.lsp_to_byte(text, 1, 0), 3);
        // Column past the line end lands on the newline, not the next line.
        assert_eq!(ix.lsp_to_byte(text, 0, 50), 2);
        // Line past the end clamps to the text end.
        assert_eq!(ix.lsp_to_byte(text, 9, 0), text.len());
    }

    #[test]
    fn line_lookup_by_byte_and_char() {
        let text = "aa\nbb\ncc";
        let ix = LineIndex::build(text);
        assert_eq!(ix.line_of_byte(0), 0);
        assert_eq!(ix.line_of_byte(2), 0);
        assert_eq!(ix.line_of_byte(3), 1);
        assert_eq!(ix.line_of_byte(8), 2);
        assert_eq!(ix.line_of_char(5), 1);
        assert_eq!(ix.line_of_char(100), 2);
    }

    #[test]
    fn find_matches_is_case_insensitive_by_default_and_non_overlapping() {
        assert_eq!(find_matches("Foo foo FOO", "foo", true), vec![(0, 3), (4, 7), (8, 11)]);
        assert_eq!(find_matches("Foo foo FOO", "foo", false), vec![(4, 7)]);
        assert!(find_matches("abc", "", true).is_empty());
        assert!(find_matches("ab", "abc", true).is_empty());
        assert_eq!(find_matches("aaa", "aa", true), vec![(0, 2)]);
    }

    #[test]
    fn find_matches_returns_byte_ranges_over_multibyte_text() {
        let text = "\u{2713} caf\u{00e9} CAF\u{00c9}";
        let m = find_matches(text, "caf\u{00e9}", true);
        assert_eq!(m.len(), 2);
        assert_eq!(&text[m[0].0..m[0].1], "caf\u{00e9}");
        assert_eq!(&text[m[1].0..m[1].1], "CAF\u{00c9}");
    }

    #[test]
    fn replace_all_counts_and_splices_every_match() {
        let (out, n) = replace_all("cat cat dog", "cat", "dog", true);
        assert_eq!(out, "dog dog dog");
        assert_eq!(n, 2);
        let (out2, n2) = replace_all("nothing here", "xyz", "q", true);
        assert_eq!(out2, "nothing here");
        assert_eq!(n2, 0);
    }

}
