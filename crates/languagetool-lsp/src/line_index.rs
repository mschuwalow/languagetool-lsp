use tower_lsp::lsp_types::Position;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineIndex {
    line_starts_utf16: Vec<usize>,
    total_utf16: usize,
}

impl LineIndex {
    pub fn new(text: &str) -> Self {
        let mut line_starts_utf16 = vec![0];
        let mut offset = 0;

        for ch in text.chars() {
            offset += ch.len_utf16();
            if ch == '\n' {
                line_starts_utf16.push(offset);
            }
        }

        Self {
            line_starts_utf16,
            total_utf16: offset,
        }
    }

    pub fn position(&self, utf16_offset: usize) -> Position {
        let utf16_offset = utf16_offset.min(self.total_utf16);
        let next_line = self
            .line_starts_utf16
            .partition_point(|line_start| *line_start <= utf16_offset);
        let line = next_line.saturating_sub(1);
        let character = utf16_offset - self.line_starts_utf16[line];

        Position {
            line: line as u32,
            character: character as u32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_ascii_offsets() {
        let index = LineIndex::new("hello\nworld");
        assert_eq!(index.position(0), Position::new(0, 0));
        assert_eq!(index.position(5), Position::new(0, 5));
        assert_eq!(index.position(6), Position::new(1, 0));
        assert_eq!(index.position(11), Position::new(1, 5));
    }

    #[test]
    fn maps_utf16_offsets() {
        let index = LineIndex::new("a😀b\nz");
        assert_eq!(index.position(0), Position::new(0, 0));
        assert_eq!(index.position(1), Position::new(0, 1));
        assert_eq!(index.position(3), Position::new(0, 3));
        assert_eq!(index.position(4), Position::new(0, 4));
        assert_eq!(index.position(5), Position::new(1, 0));
    }
}
