use tower_lsp::lsp_types::{Position, Range};

/// A byte offset into document text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ByteOffset(pub usize);

impl ByteOffset {
    pub fn as_usize(self) -> usize {
        self.0
    }
}

impl From<usize> for ByteOffset {
    fn from(v: usize) -> Self {
        Self(v)
    }
}

impl From<ByteOffset> for usize {
    fn from(v: ByteOffset) -> Self {
        v.0
    }
}

/// A UTF-16 code-unit offset, as used by the LSP and LanguageTool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Utf16Offset(pub usize);

impl Utf16Offset {
    pub fn as_usize(self) -> usize {
        self.0
    }
}

impl From<usize> for Utf16Offset {
    fn from(v: usize) -> Self {
        Self(v)
    }
}

impl From<Utf16Offset> for usize {
    fn from(v: Utf16Offset) -> Self {
        v.0
    }
}

/// A range expressed as a pair of byte offsets into document text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ByteRange {
    pub start: ByteOffset,
    pub end: ByteOffset,
}

impl ByteRange {
    pub fn new(start: impl Into<ByteOffset>, end: impl Into<ByteOffset>) -> Self {
        Self {
            start: start.into(),
            end: end.into(),
        }
    }
}

impl From<std::ops::Range<usize>> for ByteRange {
    fn from(r: std::ops::Range<usize>) -> Self {
        Self::new(r.start, r.end)
    }
}

impl From<ByteRange> for std::ops::Range<usize> {
    fn from(r: ByteRange) -> Self {
        r.start.0..r.end.0
    }
}

/// A range expressed as a pair of UTF-16 code-unit offsets, as used by the
/// LSP and LanguageTool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Utf16Range {
    pub start: Utf16Offset,
    pub end: Utf16Offset,
}

impl Utf16Range {
    pub fn new(start: impl Into<Utf16Offset>, end: impl Into<Utf16Offset>) -> Self {
        Self {
            start: start.into(),
            end: end.into(),
        }
    }

    pub fn intersects(&self, other: &Utf16Range) -> bool {
        self.start < other.end && self.end > other.start
    }
}

/// Per-document index built in a single O(n) pass.
///
/// Provides O(log n) UTF-16 ↔ byte offset conversion and O(log n) flat-UTF-16-offset →
/// LSP `Position` lookup.
///
/// The UTF-16/byte mapping works by recording a `(utf16_offset, byte_offset)` checkpoint
/// at the position *after* every non-ASCII character. Between consecutive checkpoints all
/// characters are ASCII (1 UTF-16 unit == 1 byte), so any offset within a gap resolves
/// by simple arithmetic from the nearest checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextIndex {
    line_starts_utf16: Vec<usize>,
    line_ends_utf16: Vec<usize>,
    checkpoints: Vec<(usize, usize)>,
    invalid_utf16_offsets: Vec<usize>,
    total_utf16: usize,
    total_bytes: usize,
}

struct LineEditWindow {
    line_first: usize,
    line_replace_end: usize,
    byte_start: usize,
    byte_end: usize,
    utf16_start: usize,
    include_trailing_line: bool,
    utf16_delta: isize,
}

impl TextIndex {
    pub fn new(text: &str) -> Self {
        let mut line_starts_utf16 = vec![0usize];
        let mut line_ends_utf16 = Vec::new();
        let mut checkpoints: Vec<(usize, usize)> = Vec::new();
        let mut invalid_utf16_offsets = Vec::new();
        let mut utf16 = 0usize;
        let mut chars = text.char_indices().peekable();

        while let Some((byte_off, ch)) = chars.next() {
            let u16_len = ch.len_utf16();
            let u8_len = ch.len_utf8();

            if ch == '\r' {
                line_ends_utf16.push(utf16);
                utf16 += u16_len;
                if chars.peek().is_some_and(|(_, next)| *next == '\n') {
                    chars.next();
                    utf16 += 1;
                }
                line_starts_utf16.push(utf16);
                continue;
            }

            if ch == '\n' {
                line_ends_utf16.push(utf16);
                utf16 += u16_len;
                line_starts_utf16.push(utf16);
                continue;
            }

            if u16_len == 2 {
                invalid_utf16_offsets.push(utf16 + 1);
            }
            utf16 += u16_len;
            if u16_len != u8_len {
                checkpoints.push((utf16, byte_off + u8_len));
            }
        }
        line_ends_utf16.push(utf16);

        Self {
            line_starts_utf16,
            line_ends_utf16,
            checkpoints,
            invalid_utf16_offsets,
            total_utf16: utf16,
            total_bytes: text.len(),
        }
    }

    /// Update the index for a single range replacement.
    ///
    /// `text` is the full document text after applying the edit. `bytes` and
    /// `utf16` are the old-text boundaries of the replaced region, as returned
    /// by [`Self::edit_offsets`]. `new_text` is the replacement string.
    pub fn apply_edit(
        &mut self,
        text: &str,
        bytes: &ByteRange,
        utf16: &Utf16Range,
        new_text: &str,
    ) {
        let byte_start = bytes.start.0;
        let byte_end = bytes.end.0;
        let utf16_start = utf16.start.0;
        let utf16_end = utf16.end.0;

        let new_utf16_len: usize = new_text.chars().map(|c| c.len_utf16()).sum();
        let new_byte_len = new_text.len();
        let utf16_delta = new_utf16_len as isize - (utf16_end - utf16_start) as isize;
        let byte_delta = new_byte_len as isize - (byte_end - byte_start) as isize;

        let line_first = self.line_for_utf16_offset(utf16_start).saturating_sub(1);
        let line_last = (self.line_for_utf16_offset(utf16_end) + 1)
            .min(self.line_starts_utf16.len().saturating_sub(1));
        let line_replace_end = (line_last + 1).min(self.line_starts_utf16.len());
        let window_utf16_start = self.line_starts_utf16[line_first];
        let window_byte_start = self
            .byte_offset_for_utf16_raw(window_utf16_start)
            .expect("line starts are valid UTF-16 offsets");
        let window_utf16_end = self
            .line_starts_utf16
            .get(line_replace_end)
            .copied()
            .unwrap_or(self.total_utf16);
        let old_window_byte_end = self
            .byte_offset_for_utf16_raw(window_utf16_end)
            .expect("line starts are valid UTF-16 offsets");
        let new_window_byte_end = (old_window_byte_end as isize + byte_delta) as usize;
        let include_trailing_line = line_replace_end == self.line_starts_utf16.len();

        self.apply_checkpoint_edit(byte_start, byte_end, utf16_start, utf16_end, new_text);
        self.apply_invalid_offset_edit(utf16_start, utf16_end, new_text, utf16_delta);
        self.apply_line_edit(
            text,
            LineEditWindow {
                line_first,
                line_replace_end,
                byte_start: window_byte_start,
                byte_end: new_window_byte_end,
                utf16_start: window_utf16_start,
                include_trailing_line,
                utf16_delta,
            },
        );
        self.total_utf16 = (self.total_utf16 as isize + utf16_delta) as usize;
        self.total_bytes = (self.total_bytes as isize + byte_delta) as usize;
    }

    pub fn position(&self, offset: Utf16Offset) -> Position {
        let utf16_offset = offset.0.min(self.total_utf16);
        let next_line = self
            .line_starts_utf16
            .partition_point(|&ls| ls <= utf16_offset);
        let line = next_line.saturating_sub(1);
        let character = utf16_offset - self.line_starts_utf16[line];
        Position {
            line: line as u32,
            character: character as u32,
        }
    }

    pub fn byte_offset_for_utf16(&self, offset: Utf16Offset) -> Option<ByteOffset> {
        self.byte_offset_for_utf16_raw(offset.0).map(ByteOffset)
    }

    pub fn utf16_offset_for_byte(&self, offset: ByteOffset) -> Utf16Offset {
        Utf16Offset(self.utf16_offset_for_byte_raw(offset.0))
    }

    pub fn text_for_utf16_range<'t>(&self, text: &'t str, range: Utf16Range) -> Option<&'t str> {
        if range.start > range.end {
            return None;
        }
        let byte_start = self.byte_offset_for_utf16_raw(range.start.0)?;
        let byte_end = self.byte_offset_for_utf16_raw(range.end.0)?;
        text.get(byte_start..byte_end)
    }

    /// Returns byte and UTF-16 offsets for an LSP range.
    pub fn edit_offsets(&self, range: Range) -> Option<(ByteRange, Utf16Range)> {
        let utf16_start = self.utf16_offset_for_lsp_position(range.start)?;
        let utf16_end = self.utf16_offset_for_lsp_position(range.end)?;
        if utf16_start > utf16_end {
            return None;
        }
        let byte_start = self.byte_offset_for_utf16_raw(utf16_start)?;
        let byte_end = self.byte_offset_for_utf16_raw(utf16_end)?;
        Some((
            ByteRange::new(byte_start, byte_end),
            Utf16Range::new(utf16_start, utf16_end),
        ))
    }

    fn byte_offset_for_utf16_raw(&self, utf16_offset: usize) -> Option<usize> {
        if utf16_offset > self.total_utf16 {
            return None;
        }
        if self
            .invalid_utf16_offsets
            .binary_search(&utf16_offset)
            .is_ok()
        {
            return None;
        }
        if utf16_offset == self.total_utf16 {
            return Some(self.total_bytes);
        }
        let idx = self
            .checkpoints
            .partition_point(|&(cp_utf16, _)| cp_utf16 <= utf16_offset);
        let (base_utf16, base_byte) = if idx == 0 {
            (0, 0)
        } else {
            self.checkpoints[idx - 1]
        };
        Some(base_byte + (utf16_offset - base_utf16))
    }

    fn utf16_offset_for_byte_raw(&self, byte_offset: usize) -> usize {
        if byte_offset >= self.total_bytes {
            return self.total_utf16;
        }
        let idx = self
            .checkpoints
            .partition_point(|&(_, cp_byte)| cp_byte <= byte_offset);
        let (base_utf16, base_byte) = if idx == 0 {
            (0, 0)
        } else {
            self.checkpoints[idx - 1]
        };
        base_utf16 + (byte_offset - base_byte)
    }

    fn utf16_offset_for_lsp_position(&self, position: Position) -> Option<usize> {
        let line = position.line as usize;
        if line >= self.line_starts_utf16.len() {
            return None;
        }
        let line_start = self.line_starts_utf16[line];
        let line_end = self.line_ends_utf16[line];
        let offset = line_start + position.character as usize;
        (offset <= line_end).then_some(offset)
    }

    fn line_for_utf16_offset(&self, utf16_offset: usize) -> usize {
        let utf16_offset = utf16_offset.min(self.total_utf16);
        self.line_starts_utf16
            .partition_point(|&ls| ls <= utf16_offset)
            .saturating_sub(1)
            .min(self.line_starts_utf16.len().saturating_sub(1))
    }

    fn apply_checkpoint_edit(
        &mut self,
        byte_start: usize,
        byte_end: usize,
        utf16_start: usize,
        utf16_end: usize,
        new_text: &str,
    ) {
        let new_utf16_len: usize = new_text.chars().map(|c| c.len_utf16()).sum();
        let utf16_delta = new_utf16_len as isize - (utf16_end - utf16_start) as isize;
        let byte_delta = new_text.len() as isize - (byte_end - byte_start) as isize;
        let cp_first = self
            .checkpoints
            .partition_point(|&(_, cb)| cb <= byte_start);
        let cp_last = self.checkpoints.partition_point(|&(_, cb)| cb <= byte_end);

        let mut new_cps = Vec::new();
        let mut utf16 = utf16_start;
        for (off, ch) in new_text.char_indices() {
            let u16_len = ch.len_utf16();
            let u8_len = ch.len_utf8();
            utf16 += u16_len;
            if u16_len != u8_len {
                new_cps.push((utf16, byte_start + off + u8_len));
            }
        }

        for (cu, cb) in &mut self.checkpoints[cp_last..] {
            *cu = (*cu as isize + utf16_delta) as usize;
            *cb = (*cb as isize + byte_delta) as usize;
        }
        self.checkpoints.splice(cp_first..cp_last, new_cps);
    }

    fn apply_invalid_offset_edit(
        &mut self,
        utf16_start: usize,
        utf16_end: usize,
        new_text: &str,
        utf16_delta: isize,
    ) {
        let invalid_first = self
            .invalid_utf16_offsets
            .partition_point(|&offset| offset <= utf16_start);
        let invalid_last = self
            .invalid_utf16_offsets
            .partition_point(|&offset| offset <= utf16_end);

        let mut new_invalid = Vec::new();
        let mut utf16 = utf16_start;
        for ch in new_text.chars() {
            if ch.len_utf16() == 2 {
                new_invalid.push(utf16 + 1);
            }
            utf16 += ch.len_utf16();
        }

        for offset in &mut self.invalid_utf16_offsets[invalid_last..] {
            *offset = (*offset as isize + utf16_delta) as usize;
        }
        self.invalid_utf16_offsets
            .splice(invalid_first..invalid_last, new_invalid);
    }

    fn apply_line_edit(&mut self, text: &str, window: LineEditWindow) {
        let segment = &text[window.byte_start..window.byte_end];
        let (new_starts, new_ends) =
            line_tables_for_segment(segment, window.utf16_start, window.include_trailing_line);

        for start in &mut self.line_starts_utf16[window.line_replace_end..] {
            *start = (*start as isize + window.utf16_delta) as usize;
        }
        for end in &mut self.line_ends_utf16[window.line_replace_end..] {
            *end = (*end as isize + window.utf16_delta) as usize;
        }
        self.line_starts_utf16
            .splice(window.line_first..window.line_replace_end, new_starts);
        self.line_ends_utf16
            .splice(window.line_first..window.line_replace_end, new_ends);
    }
}

fn line_tables_for_segment(
    text: &str,
    base_utf16: usize,
    include_trailing_line: bool,
) -> (Vec<usize>, Vec<usize>) {
    let mut starts = vec![base_utf16];
    let mut ends = Vec::new();
    let mut utf16 = base_utf16;
    let mut ended_with_line_break = false;
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        ended_with_line_break = false;
        if ch == '\r' {
            ends.push(utf16);
            utf16 += 1;
            if chars.peek().is_some_and(|next| *next == '\n') {
                chars.next();
                utf16 += 1;
            }
            starts.push(utf16);
            ended_with_line_break = true;
            continue;
        }

        if ch == '\n' {
            ends.push(utf16);
            utf16 += 1;
            starts.push(utf16);
            ended_with_line_break = true;
            continue;
        }

        utf16 += ch.len_utf16();
    }
    ends.push(utf16);

    if !include_trailing_line && ended_with_line_break {
        starts.pop();
        ends.pop();
    }

    (starts, ends)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_ascii_offsets() {
        let index = TextIndex::new("hello\nworld");
        assert_eq!(index.position(Utf16Offset(0)), Position::new(0, 0));
        assert_eq!(index.position(Utf16Offset(5)), Position::new(0, 5));
        assert_eq!(index.position(Utf16Offset(6)), Position::new(1, 0));
        assert_eq!(index.position(Utf16Offset(11)), Position::new(1, 5));
    }

    #[test]
    fn maps_utf16_offsets() {
        let index = TextIndex::new("a😀b\nz");
        assert_eq!(index.position(Utf16Offset(0)), Position::new(0, 0));
        assert_eq!(index.position(Utf16Offset(1)), Position::new(0, 1));
        assert_eq!(index.position(Utf16Offset(3)), Position::new(0, 3));
        assert_eq!(index.position(Utf16Offset(4)), Position::new(0, 4));
        assert_eq!(index.position(Utf16Offset(5)), Position::new(1, 0));
    }

    #[test]
    fn maps_lsp_range_to_byte_range() {
        let text = "a😀b";
        let index = TextIndex::new(text);
        assert_eq!(
            index.edit_offsets(Range::new(Position::new(0, 1), Position::new(0, 3))),
            Some((
                ByteRange::new(1usize, 5usize),
                Utf16Range::new(1usize, 3usize)
            ))
        );
        assert_eq!(
            index.text_for_utf16_range(text, Utf16Range::new(1usize, 3usize)),
            Some("😀")
        );
    }

    #[test]
    fn rejects_lsp_positions_past_line_end() {
        let index = TextIndex::new("a\nb");
        assert_eq!(
            index.edit_offsets(Range::new(Position::new(0, 2), Position::new(0, 2))),
            None
        );
        assert_eq!(
            index.edit_offsets(Range::new(Position::new(1, 99), Position::new(1, 99))),
            None
        );
    }

    #[test]
    fn rejects_reversed_lsp_ranges() {
        let index = TextIndex::new("abc");
        assert_eq!(
            index.edit_offsets(Range::new(Position::new(0, 2), Position::new(0, 1))),
            None
        );
    }

    #[test]
    fn rejects_utf16_offsets_inside_surrogate_pairs() {
        let index = TextIndex::new("a😀b");
        assert_eq!(index.byte_offset_for_utf16(Utf16Offset(2)), None);
        assert_eq!(
            index.edit_offsets(Range::new(Position::new(0, 2), Position::new(0, 2))),
            None
        );
    }

    #[test]
    fn byte_offset_roundtrip() {
        let text = "hello 😀 wörld\nfoo";
        let index = TextIndex::new(text);
        let mut utf16 = 0usize;
        for (byte_off, ch) in text.char_indices() {
            assert_eq!(
                index.byte_offset_for_utf16(Utf16Offset(utf16)),
                Some(ByteOffset(byte_off)),
                "utf16={utf16} ch={ch:?}"
            );
            assert_eq!(
                index.utf16_offset_for_byte(ByteOffset(byte_off)),
                Utf16Offset(utf16),
                "byte={byte_off} ch={ch:?}"
            );
            utf16 += ch.len_utf16();
        }
        assert_eq!(
            index.byte_offset_for_utf16(Utf16Offset(utf16)),
            Some(ByteOffset(text.len()))
        );
        assert_eq!(
            index.utf16_offset_for_byte(ByteOffset(text.len())),
            Utf16Offset(utf16)
        );
    }

    #[test]
    fn ascii_only_has_no_checkpoints() {
        let index = TextIndex::new("hello world");
        assert!(index.checkpoints.is_empty());
    }

    fn boundary_corpus() -> Vec<&'static str> {
        vec![
            "",
            "abc",
            "a\nb",
            "a\rb",
            "a\r\nb",
            "\n",
            "\r",
            "\r\n",
            "😀",
            "a😀b",
            "å😀ß",
            "a\n😀\r\nß",
            "one\rtwo\nthree\r\nfour",
            "😀\r\n😀\n😀\r😀",
        ]
    }

    fn replacement_corpus() -> Vec<&'static str> {
        vec![
            "", "x", "xyz", "😀", "ß", "\n", "\r", "\r\n", "x\ny", "x\r\ny",
        ]
    }

    fn utf16_char_boundaries(text: &str) -> Vec<usize> {
        let mut offsets = vec![0];
        let mut utf16 = 0;
        for ch in text.chars() {
            utf16 += ch.len_utf16();
            offsets.push(utf16);
        }
        offsets
    }

    fn editable_utf16_offsets(text: &str) -> Vec<usize> {
        let index = TextIndex::new(text);
        utf16_char_boundaries(text)
            .into_iter()
            .filter(|&offset| {
                let position = index.position(Utf16Offset(offset));
                index
                    .edit_offsets(Range::new(position, position))
                    .is_some_and(|(_, utf16)| utf16.start.0 == offset && utf16.end.0 == offset)
            })
            .collect()
    }

    fn check_apply_edit_offsets(
        before: &str,
        utf16_start: usize,
        utf16_end: usize,
        new_text: &str,
    ) {
        let mut index = TextIndex::new(before);
        let range = Range::new(
            index.position(Utf16Offset(utf16_start)),
            index.position(Utf16Offset(utf16_end)),
        );
        let (bytes, utf16) = index.edit_offsets(range).unwrap();

        let mut after = before.to_string();
        after.replace_range(bytes.start.0..bytes.end.0, new_text);
        index.apply_edit(&after, &bytes, &utf16, new_text);
        let expected = TextIndex::new(&after);

        assert_eq!(
            index, expected,
            "before={before:?} range={utf16_start}..{utf16_end} replacement={new_text:?} after={after:?}"
        );
    }

    // Helper: apply an edit via apply_edit and verify the result equals TextIndex::new
    // on the post-edit text.
    fn check_apply_edit(before: &str, range: Range, new_text: &str) {
        let mut index = TextIndex::new(before);
        let (bytes, utf16) = index.edit_offsets(range).unwrap();

        let mut after = before.to_string();
        after.replace_range(bytes.start.0..bytes.end.0, new_text);
        index.apply_edit(&after, &bytes, &utf16, new_text);
        let expected = TextIndex::new(&after);

        assert_eq!(index, expected, "after text: {after:?}");
    }

    #[test]
    fn incremental_apply_edit_matches_fresh_index_for_exhaustive_small_edits() {
        for before in boundary_corpus() {
            let offsets = editable_utf16_offsets(before);
            for &start in &offsets {
                for &end in offsets.iter().filter(|&&end| end >= start) {
                    for replacement in replacement_corpus() {
                        check_apply_edit_offsets(before, start, end, replacement);
                    }
                }
            }
        }
    }

    #[test]
    fn index_roundtrips_all_valid_utf16_and_byte_boundaries() {
        for text in boundary_corpus() {
            let index = TextIndex::new(text);
            for byte in 0..=text.len() {
                if text.is_char_boundary(byte) {
                    let utf16 = index.utf16_offset_for_byte(ByteOffset(byte));
                    assert_eq!(
                        index.byte_offset_for_utf16(utf16),
                        Some(ByteOffset(byte)),
                        "text={text:?} byte={byte} utf16={:?}",
                        utf16.0
                    );
                }
            }

            for utf16 in 0..=index.total_utf16 {
                match index.byte_offset_for_utf16(Utf16Offset(utf16)) {
                    Some(byte) => {
                        assert!(
                            text.is_char_boundary(byte.0),
                            "text={text:?} utf16={utf16} byte={:?}",
                            byte.0
                        );
                        assert_eq!(index.utf16_offset_for_byte(byte), Utf16Offset(utf16));
                    }
                    None => assert!(
                        index.invalid_utf16_offsets.binary_search(&utf16).is_ok(),
                        "only surrogate interiors should be invalid: text={text:?} utf16={utf16}"
                    ),
                }
            }
        }
    }

    #[test]
    fn line_tables_are_consistent_for_mixed_line_endings() {
        for text in boundary_corpus() {
            let index = TextIndex::new(text);
            assert_eq!(
                index.line_starts_utf16.len(),
                index.line_ends_utf16.len(),
                "text={text:?}"
            );
            assert_eq!(index.line_starts_utf16.first(), Some(&0));

            for line in 0..index.line_starts_utf16.len() {
                let start = index.line_starts_utf16[line];
                let end = index.line_ends_utf16[line];
                assert!(start <= end, "text={text:?} line={line}");
                assert!(index.byte_offset_for_utf16(Utf16Offset(start)).is_some());
                assert!(index.byte_offset_for_utf16(Utf16Offset(end)).is_some());

                if let Some(&next_start) = index.line_starts_utf16.get(line + 1) {
                    assert!(end <= next_start, "text={text:?} line={line}");
                    assert!(next_start <= index.total_utf16, "text={text:?} line={line}");
                }
            }
        }
    }

    #[test]
    fn lsp_positions_past_line_end_are_invalid_across_line_endings() {
        for text in boundary_corpus() {
            let index = TextIndex::new(text);
            for line in 0..index.line_starts_utf16.len() {
                let line_start = index.line_starts_utf16[line];
                let line_end = index.line_ends_utf16[line];
                let overlong = Position::new(line as u32, (line_end - line_start + 100) as u32);
                assert_eq!(
                    index.edit_offsets(Range::new(overlong, overlong)),
                    None,
                    "text={text:?} line={line}"
                );
            }
        }
    }

    #[test]
    fn apply_edit_ascii_replace() {
        check_apply_edit(
            "hello world",
            Range::new(Position::new(0, 6), Position::new(0, 11)),
            "zed",
        );
    }

    #[test]
    fn apply_edit_insert_newline() {
        check_apply_edit(
            "hello world",
            Range::new(Position::new(0, 5), Position::new(0, 5)),
            "\n",
        );
    }

    #[test]
    fn apply_edit_delete_newline() {
        check_apply_edit(
            "hello\nworld",
            Range::new(Position::new(0, 5), Position::new(1, 0)),
            "",
        );
    }

    #[test]
    fn apply_edit_replace_emoji_with_ascii() {
        check_apply_edit(
            "a😀b",
            Range::new(Position::new(0, 1), Position::new(0, 3)),
            "x",
        );
    }

    #[test]
    fn apply_edit_insert_emoji() {
        check_apply_edit(
            "ab",
            Range::new(Position::new(0, 1), Position::new(0, 1)),
            "😀",
        );
    }

    #[test]
    fn apply_edit_multiline_replace() {
        check_apply_edit(
            "line one\nline two\nline three",
            Range::new(Position::new(0, 5), Position::new(2, 4)),
            "X\nY",
        );
    }

    #[test]
    fn maps_crlf_line_endings() {
        // \r\n counts as one line ending; the new line starts after the \n.
        let index = TextIndex::new("hello\r\nworld");
        assert_eq!(index.position(Utf16Offset(0)), Position::new(0, 0));
        assert_eq!(index.position(Utf16Offset(5)), Position::new(0, 5)); // the \r
        assert_eq!(index.position(Utf16Offset(6)), Position::new(0, 6)); // the \n
        assert_eq!(index.position(Utf16Offset(7)), Position::new(1, 0)); // 'w'
        assert_eq!(index.position(Utf16Offset(12)), Position::new(1, 5));
    }

    #[test]
    fn maps_bare_cr_line_endings() {
        let index = TextIndex::new("hello\rworld");
        assert_eq!(index.position(Utf16Offset(0)), Position::new(0, 0));
        assert_eq!(index.position(Utf16Offset(5)), Position::new(0, 5)); // the \r
        assert_eq!(index.position(Utf16Offset(6)), Position::new(1, 0)); // 'w'
    }

    #[test]
    fn apply_edit_crlf_insert() {
        check_apply_edit(
            "hello\r\nworld",
            Range::new(Position::new(0, 5), Position::new(0, 5)),
            " there",
        );
    }

    #[test]
    fn apply_edit_replaces_crlf_with_lf() {
        check_apply_edit(
            "a\r\nb",
            Range::new(Position::new(0, 1), Position::new(1, 0)),
            "\n",
        );
    }

    #[test]
    fn apply_edit_rebuilds_line_index_when_forming_crlf() {
        check_apply_edit(
            "a\rb",
            Range::new(Position::new(1, 0), Position::new(1, 0)),
            "\n",
        );
    }
}
