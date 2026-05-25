use crate::language::SupportedLanguage;
use crate::languagetool::{AnnotatedText, Annotation};
use crate::text_index::TextIndex;
use thiserror::Error;
use tree_sitter::{InputEdit, Language as TreeSitterLanguage, Node, Parser, Point, Tree};
use tree_sitter_md_025::{MarkdownParser, MarkdownTree};

/// Maintains parser-backed masking state for a document and produces checkable text ranges.
#[derive(Debug, Clone)]
pub struct Masker {
    parsed: ParsedMask,
}

#[derive(Debug, Error)]
pub enum MaskError {
    #[error("failed to parse {language:?} mask after incremental edit and full reparse")]
    ParseAfterEdit { language: SupportedLanguage },
}

#[derive(Debug, Clone)]
enum ParsedMask {
    Rust(Tree),
    Scala(Tree),
    Nix(Tree),
    Html(Tree),
    Java(Tree),
    Python(Tree),
    Javascript(Tree),
    Typescript(Tree),
    Tsx(Tree),
    Markdown(MarkdownTree),
    PlainText,
}

impl Masker {
    pub fn new(text: &str, language: SupportedLanguage) -> Self {
        let parsed = ParsedMask::parse(language, text).unwrap_or(ParsedMask::PlainText);
        Self { parsed }
    }

    pub fn apply_edit(
        &mut self,
        old_text: &str,
        text: &str,
        byte_start: usize,
        byte_end: usize,
        new_text: &str,
    ) -> Result<(), MaskError> {
        let start_position = point_for_byte(old_text, byte_start);
        let old_end_position = point_for_byte(old_text, byte_end);
        let new_end_byte = byte_start + new_text.len();
        let new_end_position = point_after_text(start_position, new_text);
        let edit = InputEdit {
            start_byte: byte_start,
            old_end_byte: byte_end,
            new_end_byte,
            start_position,
            old_end_position,
            new_end_position,
        };

        self.parsed.edit(&edit);
        if let Some(parsed) = self.parsed.reparse_incremental(text) {
            self.parsed = parsed;
            return Ok(());
        } else if let Some(parsed) = self.parsed.parse_fresh(text) {
            self.parsed = parsed;
            return Ok(());
        }
        if let Some(language) = self.parsed.language() {
            Err(MaskError::ParseAfterEdit { language })
        } else {
            Ok(())
        }
    }

    pub fn annotated(&self, text: &str) -> AnnotatedText {
        let annotation = self
            .ranges(text)
            .map(|ranges| match ranges {
                MaskRanges::Keep(mut ranges) => annotations_from_keep_ranges(text, &mut ranges),
                MaskRanges::Skip(mut ranges) => annotations_from_skip_ranges(text, &mut ranges),
            })
            .unwrap_or_else(|| match self.parsed {
                ParsedMask::PlainText => vec![Annotation::text(text.to_string())],
                _ => Vec::new(),
            });

        AnnotatedText { annotation }
    }

    pub fn ignored_ranges(&self, text: &str, index: &TextIndex) -> Vec<(usize, usize)> {
        match self.ranges(text) {
            Some(MaskRanges::Keep(mut ranges)) => inverse_ranges_as_utf16(text, index, &mut ranges),
            Some(MaskRanges::Skip(mut ranges)) => ranges_as_utf16(index, &mut ranges),
            None => Vec::new(),
        }
    }

    fn ranges(&self, text: &str) -> Option<MaskRanges> {
        match &self.parsed {
            ParsedMask::Html(tree) => {
                let mut ranges = Vec::new();
                collect_html_skip_ranges(tree.root_node(), &mut ranges);
                Some(MaskRanges::Skip(ranges))
            }
            ParsedMask::Markdown(tree) => {
                let mut ranges = Vec::new();
                collect_markdown_skip_ranges(tree.block_tree().root_node(), &mut ranges);
                for inline_tree in tree.inline_trees() {
                    collect_markdown_inline_skip_ranges(inline_tree.root_node(), &mut ranges);
                }
                Some(MaskRanges::Skip(ranges))
            }
            ParsedMask::PlainText => None,
            parsed => {
                let mut ranges = Vec::new();
                let (language, tree) = parsed.comment_tree()?;
                collect_comment_nodes(text, language, tree.root_node(), &mut ranges);
                Some(MaskRanges::Keep(ranges))
            }
        }
    }
}

impl ParsedMask {
    fn edit(&mut self, edit: &InputEdit) {
        match self {
            Self::Rust(tree)
            | Self::Scala(tree)
            | Self::Nix(tree)
            | Self::Html(tree)
            | Self::Java(tree)
            | Self::Python(tree)
            | Self::Javascript(tree)
            | Self::Typescript(tree)
            | Self::Tsx(tree) => tree.edit(edit),
            Self::Markdown(tree) => tree.edit(edit),
            Self::PlainText => {}
        }
    }

    fn parse(language: SupportedLanguage, text: &str) -> Option<Self> {
        match language {
            SupportedLanguage::PlainText => Some(Self::PlainText),
            SupportedLanguage::Markdown => {
                let mut parser = MarkdownParser::default();
                parser.parse(text.as_bytes(), None).map(Self::Markdown)
            }
            _ => {
                let mut parser = parser(language);
                let tree = parser.parse(text, None)?;
                Some(match language {
                    SupportedLanguage::Rust => Self::Rust(tree),
                    SupportedLanguage::Scala => Self::Scala(tree),
                    SupportedLanguage::Nix => Self::Nix(tree),
                    SupportedLanguage::Html => Self::Html(tree),
                    SupportedLanguage::Java => Self::Java(tree),
                    SupportedLanguage::Python => Self::Python(tree),
                    SupportedLanguage::Javascript => Self::Javascript(tree),
                    SupportedLanguage::Typescript => Self::Typescript(tree),
                    SupportedLanguage::Tsx => Self::Tsx(tree),
                    SupportedLanguage::Markdown | SupportedLanguage::PlainText => unreachable!(),
                })
            }
        }
    }

    fn reparse_incremental(&self, text: &str) -> Option<Self> {
        match self {
            Self::Rust(tree) => reparse_tree(SupportedLanguage::Rust, text, tree).map(Self::Rust),
            Self::Scala(tree) => {
                reparse_tree(SupportedLanguage::Scala, text, tree).map(Self::Scala)
            }
            Self::Nix(tree) => reparse_tree(SupportedLanguage::Nix, text, tree).map(Self::Nix),
            Self::Html(tree) => reparse_tree(SupportedLanguage::Html, text, tree).map(Self::Html),
            Self::Java(tree) => reparse_tree(SupportedLanguage::Java, text, tree).map(Self::Java),
            Self::Python(tree) => {
                reparse_tree(SupportedLanguage::Python, text, tree).map(Self::Python)
            }
            Self::Javascript(tree) => {
                reparse_tree(SupportedLanguage::Javascript, text, tree).map(Self::Javascript)
            }
            Self::Typescript(tree) => {
                reparse_tree(SupportedLanguage::Typescript, text, tree).map(Self::Typescript)
            }
            Self::Tsx(tree) => reparse_tree(SupportedLanguage::Tsx, text, tree).map(Self::Tsx),
            Self::Markdown(tree) => {
                let mut parser = MarkdownParser::default();
                parser
                    .parse(text.as_bytes(), Some(tree))
                    .map(Self::Markdown)
            }
            Self::PlainText => None,
        }
    }

    fn parse_fresh(&self, text: &str) -> Option<Self> {
        match self {
            Self::Rust(_) => Self::parse(SupportedLanguage::Rust, text),
            Self::Scala(_) => Self::parse(SupportedLanguage::Scala, text),
            Self::Nix(_) => Self::parse(SupportedLanguage::Nix, text),
            Self::Html(_) => Self::parse(SupportedLanguage::Html, text),
            Self::Java(_) => Self::parse(SupportedLanguage::Java, text),
            Self::Python(_) => Self::parse(SupportedLanguage::Python, text),
            Self::Javascript(_) => Self::parse(SupportedLanguage::Javascript, text),
            Self::Typescript(_) => Self::parse(SupportedLanguage::Typescript, text),
            Self::Tsx(_) => Self::parse(SupportedLanguage::Tsx, text),
            Self::Markdown(_) => Self::parse(SupportedLanguage::Markdown, text),
            Self::PlainText => None,
        }
    }

    fn language(&self) -> Option<SupportedLanguage> {
        match self {
            Self::Rust(_) => Some(SupportedLanguage::Rust),
            Self::Scala(_) => Some(SupportedLanguage::Scala),
            Self::Nix(_) => Some(SupportedLanguage::Nix),
            Self::Html(_) => Some(SupportedLanguage::Html),
            Self::Java(_) => Some(SupportedLanguage::Java),
            Self::Python(_) => Some(SupportedLanguage::Python),
            Self::Javascript(_) => Some(SupportedLanguage::Javascript),
            Self::Typescript(_) => Some(SupportedLanguage::Typescript),
            Self::Tsx(_) => Some(SupportedLanguage::Tsx),
            Self::Markdown(_) => Some(SupportedLanguage::Markdown),
            Self::PlainText => None,
        }
    }

    fn comment_tree(&self) -> Option<(SupportedLanguage, &Tree)> {
        match self {
            Self::Rust(tree) => Some((SupportedLanguage::Rust, tree)),
            Self::Scala(tree) => Some((SupportedLanguage::Scala, tree)),
            Self::Nix(tree) => Some((SupportedLanguage::Nix, tree)),
            Self::Html(_) => None,
            Self::Java(tree) => Some((SupportedLanguage::Java, tree)),
            Self::Python(tree) => Some((SupportedLanguage::Python, tree)),
            Self::Javascript(tree) => Some((SupportedLanguage::Javascript, tree)),
            Self::Typescript(tree) => Some((SupportedLanguage::Typescript, tree)),
            Self::Tsx(tree) => Some((SupportedLanguage::Tsx, tree)),
            Self::Markdown(_) | Self::PlainText => None,
        }
    }
}

fn tree_sitter_language(language: SupportedLanguage) -> TreeSitterLanguage {
    match language {
        SupportedLanguage::Rust => tree_sitter_rust::LANGUAGE.into(),
        SupportedLanguage::Scala => tree_sitter_scala::LANGUAGE.into(),
        SupportedLanguage::Nix => tree_sitter_nix::LANGUAGE.into(),
        SupportedLanguage::Html => tree_sitter_html::LANGUAGE.into(),
        SupportedLanguage::Java => tree_sitter_java::LANGUAGE.into(),
        SupportedLanguage::Python => tree_sitter_python::LANGUAGE.into(),
        SupportedLanguage::Javascript => tree_sitter_javascript::LANGUAGE.into(),
        SupportedLanguage::Typescript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        SupportedLanguage::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        SupportedLanguage::Markdown | SupportedLanguage::PlainText => unreachable!(),
    }
}

fn parser(language: SupportedLanguage) -> Parser {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_language(language))
        .expect("bundled Tree-sitter grammar should load");
    parser
}

fn reparse_tree(language: SupportedLanguage, text: &str, old_tree: &Tree) -> Option<Tree> {
    parser(language).parse(text, Some(old_tree))
}

fn strip_slash_comment(
    source: &str,
    node: Node<'_>,
    rust_doc_markers: bool,
) -> Option<(usize, usize)> {
    let node_start = node.start_byte();
    let node_end = node.end_byte();
    let text = source.get(node_start..node_end)?;

    let prefix_len = if rust_doc_markers && (text.starts_with("///") || text.starts_with("//!")) {
        3
    } else if text.starts_with("//") {
        2
    } else if text.starts_with("/**") || (rust_doc_markers && text.starts_with("/*!")) {
        3
    } else if text.starts_with("/*") {
        2
    } else {
        return None;
    };
    let suffix_len = text.ends_with("*/") as usize * 2;
    Some((node_start + prefix_len, node_end.saturating_sub(suffix_len)))
}

fn strip_comment_markers(
    language: SupportedLanguage,
    source: &str,
    node: Node<'_>,
) -> Option<(usize, usize)> {
    match language {
        SupportedLanguage::Rust => strip_slash_comment(source, node, true),
        SupportedLanguage::Scala
        | SupportedLanguage::Java
        | SupportedLanguage::Javascript
        | SupportedLanguage::Typescript
        | SupportedLanguage::Tsx => strip_slash_comment(source, node, false),
        SupportedLanguage::Nix | SupportedLanguage::Python => strip_hash_comment(source, node),
        SupportedLanguage::Html | SupportedLanguage::Markdown | SupportedLanguage::PlainText => {
            None
        }
    }
}

fn strip_hash_comment(source: &str, node: Node<'_>) -> Option<(usize, usize)> {
    let node_start = node.start_byte();
    let node_end = node.end_byte();
    let text = source.get(node_start..node_end)?;
    text.starts_with('#').then_some((node_start + 1, node_end))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Range {
    start: usize,
    end: usize,
}

enum MaskRanges {
    Keep(Vec<Range>),
    Skip(Vec<Range>),
}

fn collect_comment_nodes(
    text: &str,
    language: SupportedLanguage,
    node: Node<'_>,
    keep_ranges: &mut Vec<Range>,
) {
    if is_comment_node(node) {
        if let Some(range) = comment_content_range(text, language, node) {
            keep_ranges.push(range);
        }
        return;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_comment_nodes(text, language, child, keep_ranges);
    }
}

fn is_comment_node(node: Node<'_>) -> bool {
    node.kind().contains("comment")
}

fn comment_content_range(
    source: &str,
    language: SupportedLanguage,
    node: Node<'_>,
) -> Option<Range> {
    if let Some(doc) = node.child_by_field_name("doc") {
        return normalized_content_range(source, doc.start_byte(), doc.end_byte());
    }

    let (start, end) = strip_comment_markers(language, source, node)?;
    normalized_content_range(source, start, end)
}

fn normalized_content_range(source: &str, mut start: usize, mut end: usize) -> Option<Range> {
    let bytes = source.as_bytes();

    start = skip_horizontal_whitespace(bytes, start);
    while end > start && matches!(bytes[end - 1], b'\n' | b'\r') {
        end -= 1;
    }

    source.get(start..end).map(|_| Range { start, end })
}

fn collect_markdown_skip_ranges(node: Node<'_>, ranges: &mut Vec<Range>) {
    if matches!(
        node.kind(),
        "fenced_code_block" | "indented_code_block" | "code_fence_content"
    ) {
        ranges.push(Range {
            start: node.start_byte(),
            end: node.end_byte(),
        });
        return;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_markdown_skip_ranges(child, ranges);
    }
}

fn collect_markdown_inline_skip_ranges(node: Node<'_>, ranges: &mut Vec<Range>) {
    if matches!(node.kind(), "code_span" | "link_destination") {
        ranges.push(Range {
            start: node.start_byte(),
            end: node.end_byte(),
        });
        return;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_markdown_inline_skip_ranges(child, ranges);
    }
}

fn collect_html_skip_ranges(node: Node<'_>, ranges: &mut Vec<Range>) {
    if matches!(node.kind(), "script_element" | "style_element") {
        ranges.push(Range {
            start: node.start_byte(),
            end: node.end_byte(),
        });
        return;
    }

    if matches!(
        node.kind(),
        "start_tag" | "end_tag" | "self_closing_tag" | "erroneous_end_tag" | "doctype"
    ) {
        ranges.push(Range {
            start: node.start_byte(),
            end: node.end_byte(),
        });
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_html_skip_ranges(child, ranges);
    }
}

fn point_for_byte(text: &str, byte: usize) -> Point {
    let mut row = 0;
    let mut line_start = 0;
    for (idx, b) in text.as_bytes().iter().enumerate().take(byte) {
        if *b == b'\n' {
            row += 1;
            line_start = idx + 1;
        }
    }
    Point {
        row,
        column: byte - line_start,
    }
}

fn point_after_text(start: Point, text: &str) -> Point {
    let mut row = start.row;
    let mut column = start.column;
    for line in text.split_inclusive('\n') {
        if line.ends_with('\n') {
            row += 1;
            column = 0;
        } else {
            column += line.len();
        }
    }
    Point { row, column }
}

fn skip_horizontal_whitespace(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() && matches!(bytes[index], b' ' | b'\t') {
        index += 1;
    }
    index
}

fn annotations_from_skip_ranges(text: &str, skip_ranges: &mut [Range]) -> Vec<Annotation> {
    let mut annotations = Vec::new();
    let mut cursor = 0;
    for range in merge_ranges(skip_ranges) {
        push_annotation_text(text, cursor, range.start, &mut annotations);
        push_annotation_markup(text, range.start, range.end, &mut annotations);
        cursor = cursor.max(range.end);
    }
    push_annotation_text(text, cursor, text.len(), &mut annotations);
    annotations
}

fn annotations_from_keep_ranges(text: &str, keep_ranges: &mut [Range]) -> Vec<Annotation> {
    let mut annotations = Vec::new();
    let mut cursor = 0;
    for range in merge_ranges(keep_ranges) {
        push_annotation_markup(text, cursor, range.start, &mut annotations);
        push_annotation_text(text, range.start, range.end, &mut annotations);
        cursor = cursor.max(range.end);
    }
    push_annotation_markup(text, cursor, text.len(), &mut annotations);
    annotations
}

fn push_annotation_text(text: &str, start: usize, end: usize, annotations: &mut Vec<Annotation>) {
    if start >= end {
        return;
    }
    if let Some(segment) = text.get(start..end) {
        if !segment.is_empty() {
            annotations.push(Annotation::text(segment.to_string()));
        }
    }
}

fn push_annotation_markup(text: &str, start: usize, end: usize, annotations: &mut Vec<Annotation>) {
    if start >= end {
        return;
    }
    if let Some(segment) = text.get(start..end) {
        if !segment.is_empty() {
            annotations.push(Annotation::markup(
                segment.to_string(),
                interpret_as_for_markup(segment),
            ));
        }
    }
}

fn interpret_as_for_markup(markup: &str) -> Option<String> {
    let line_breaks = markup
        .chars()
        .filter(|ch| matches!(ch, '\n' | '\r'))
        .collect::<String>();
    if !line_breaks.is_empty() {
        Some(line_breaks)
    } else if markup.chars().any(char::is_whitespace) {
        Some(" ".to_string())
    } else {
        None
    }
}

fn merge_ranges(ranges: &mut [Range]) -> Vec<Range> {
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut merged: Vec<Range> = Vec::new();
    for range in ranges
        .iter()
        .copied()
        .filter(|range| range.start < range.end)
    {
        if let Some(last) = merged.last_mut() {
            if range.start <= last.end {
                last.end = last.end.max(range.end);
                continue;
            }
        }
        merged.push(range);
    }
    merged
}

fn ranges_as_utf16(index: &TextIndex, ranges: &mut [Range]) -> Vec<(usize, usize)> {
    merge_ranges(ranges)
        .into_iter()
        .map(|range| {
            (
                index.utf16_offset_for_byte(range.start),
                index.utf16_offset_for_byte(range.end),
            )
        })
        .collect()
}

fn inverse_ranges_as_utf16(
    text: &str,
    index: &TextIndex,
    keep_ranges: &mut [Range],
) -> Vec<(usize, usize)> {
    let mut ignored = Vec::new();
    let mut cursor = 0;
    for range in merge_ranges(keep_ranges) {
        if cursor < range.start {
            ignored.push(Range {
                start: cursor,
                end: range.start,
            });
        }
        cursor = cursor.max(range.end);
    }
    if cursor < text.len() {
        ignored.push(Range {
            start: cursor,
            end: text.len(),
        });
    }
    ranges_as_utf16(index, &mut ignored)
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    fn annotated_for_test(text: &str, language_id: &str) -> AnnotatedText {
        let language = SupportedLanguage::from_language_id(language_id).unwrap();
        let mask = Masker::new(text, language);
        mask.annotated(text)
    }

    fn ignored_ranges_for_test(
        text: &str,
        index: &TextIndex,
        language_id: &str,
    ) -> Vec<(usize, usize)> {
        let language = SupportedLanguage::from_language_id(language_id).unwrap();
        let mask = Masker::new(text, language);
        mask.ignored_ranges(text, index)
    }

    #[test]
    fn rust_annotations_mark_code_as_markup_and_comments_as_text() {
        let text = indoc! {r#"
            let value = 1; // This are a comment.
            let other = "This are code";
            /* This are block docs. */
        "#};

        let data = annotated_for_test(text, "rust");
        let checked_text = data
            .annotation
            .iter()
            .filter_map(|annotation| annotation.as_text())
            .collect::<Vec<_>>();
        let markup = data
            .annotation
            .iter()
            .filter_map(|annotation| annotation.as_markup())
            .collect::<String>();

        assert_eq!(
            checked_text,
            vec!["This are a comment.", "This are block docs. "]
        );
        assert!(markup.contains("let value = 1; "));
        assert!(markup.contains("This are code"));
    }

    #[test]
    fn line_comments_are_separated_by_newline_interpretation() {
        let text = indoc! {"
            // I am a catz.
            // I like chickz.
        "};
        let data = annotated_for_test(text, "rust");
        let checked_text = data
            .annotation
            .iter()
            .filter_map(|annotation| annotation.as_text())
            .collect::<Vec<_>>();
        let separators = data
            .annotation
            .iter()
            .filter_map(|annotation| annotation.interpret_as())
            .collect::<Vec<_>>();

        assert_eq!(checked_text, vec!["I am a catz.", "I like chickz."]);
        assert!(separators.iter().any(|separator| separator.contains('\n')));
    }

    #[test]
    fn rust_lifetimes_do_not_hide_following_comments() {
        let text = "let value: &'a str = input; // This are docs.\n";
        let data = annotated_for_test(text, "rust");
        let checked = data
            .annotation
            .iter()
            .filter_map(|annotation| annotation.as_text())
            .collect::<String>();

        assert_eq!(checked, "This are docs.");
    }

    #[test]
    fn rust_tree_sitter_checks_doc_comments() {
        let text = indoc! {r#"
            /// This are public docs.
            //! This are module docs.
            /** This are block docs. */
            /*! This are inner block docs. */
            fn main() {}
        "#};
        let data = annotated_for_test(text, "rust");
        let checked = data
            .annotation
            .iter()
            .filter_map(|annotation| annotation.as_text())
            .collect::<Vec<_>>();

        assert_eq!(
            checked,
            vec![
                "This are public docs.",
                "This are module docs.",
                "This are block docs. ",
                "This are inner block docs. "
            ]
        );
    }

    #[test]
    fn rust_tree_sitter_ignores_comment_markers_inside_strings() {
        let text = indoc! {r##"
            let normal = "// This are code";
            let raw = r#"/* This are raw string code */"#;
            let ch = '/';
            // This are a real comment.
        "##};
        let data = annotated_for_test(text, "rust");
        let checked = data
            .annotation
            .iter()
            .filter_map(|annotation| annotation.as_text())
            .collect::<String>();

        assert!(!checked.contains("This are code"));
        assert!(!checked.contains("This are raw string code"));
        assert!(checked.contains("This are a real comment."));
    }

    #[test]
    fn rust_tree_sitter_ignored_ranges_keep_only_comments() {
        let text = indoc! {r##"
            let code = "This are code";
            // This are a comment.
        "##};
        let index = TextIndex::new(text);
        let ignored_ranges = ignored_ranges_for_test(text, &index, "rust");
        let checked = complement_utf16_ranges(text, &index, &ignored_ranges)
            .into_iter()
            .filter_map(|(start, end)| index.text_for_utf16_range(text, start, end))
            .collect::<String>();

        assert_eq!(checked, "This are a comment.");
    }

    #[test]
    fn tree_sitter_comment_masking_supports_explicit_languages() {
        let cases = [
            (
                "scala",
                indoc! {r#"
                    val code = "// This are code"
                    // This are a Scala comment.
                "#},
                "This are a Scala comment.",
                "This are code",
            ),
            (
                "nix",
                indoc! {r##"
                    let code = "# This are code";
                    # This are a Nix comment.
                "##},
                "This are a Nix comment.",
                "This are code",
            ),
            (
                "java",
                indoc! {r#"
                    class T { String code = "// This are code"; }
                    // This are a Java comment.
                "#},
                "This are a Java comment.",
                "This are code",
            ),
            (
                "python",
                indoc! {r##"
                    code = "# This are code"
                    # This are a Python comment.
                "##},
                "This are a Python comment.",
                "This are code",
            ),
            (
                "javascript",
                indoc! {r#"
                    const code = "// This are code";
                    // This are a JavaScript comment.
                "#},
                "This are a JavaScript comment.",
                "This are code",
            ),
            (
                "typescript",
                indoc! {r#"
                    const code: string = "// This are code";
                    // This are a TypeScript comment.
                "#},
                "This are a TypeScript comment.",
                "This are code",
            ),
            (
                "tsx",
                indoc! {r#"
                    const code: string = "// This are code";
                    // This are a TSX comment.
                "#},
                "This are a TSX comment.",
                "This are code",
            ),
        ];

        for (language, text, expected, unexpected) in cases {
            let data = annotated_for_test(text, language);
            let checked = data
                .annotation
                .iter()
                .filter_map(|annotation| annotation.as_text())
                .collect::<String>();

            assert!(
                checked.contains(expected),
                "language={language} checked={checked:?}"
            );
            assert!(
                !checked.contains(unexpected),
                "language={language} checked={checked:?}"
            );
            assert!(
                !checked.contains("// This are") && !checked.contains("# This are"),
                "language={language} checked={checked:?}"
            );
        }
    }

    #[test]
    fn language_specific_comment_strippers_remove_block_markers() {
        let cases = [
            (
                "rust",
                "/* This are Rust block docs. */",
                "This are Rust block docs. ",
            ),
            ("java", "/** This are Java docs. */", "This are Java docs. "),
            (
                "scala",
                "/* This are Scala docs. */",
                "This are Scala docs. ",
            ),
            (
                "javascript",
                "/* This are JS docs. */",
                "This are JS docs. ",
            ),
            (
                "typescript",
                "/* This are TS docs. */",
                "This are TS docs. ",
            ),
        ];

        for (language, text, expected) in cases {
            let data = annotated_for_test(text, language);
            let checked = data
                .annotation
                .iter()
                .filter_map(|annotation| annotation.as_text())
                .collect::<String>();

            assert_eq!(checked, expected, "language={language}");
        }
    }

    #[test]
    fn markdown_tree_sitter_skips_fenced_code() {
        let text = indoc! {"
            This are prose.
            ```rust
            This are code.
            ```
            More are prose.
        "};

        let data = annotated_for_test(text, "markdown");
        let checked = data
            .annotation
            .iter()
            .filter_map(|annotation| annotation.as_text())
            .collect::<String>();
        let markup = data
            .annotation
            .iter()
            .filter_map(|annotation| annotation.as_markup())
            .collect::<String>();

        assert!(checked.contains("This are prose."));
        assert!(checked.contains("More are prose."));
        assert!(!checked.contains("This are code."));
        assert!(markup.contains("This are code."));
    }

    #[test]
    fn python_hash_markers_inside_strings_are_not_comments() {
        let text = indoc! {r##"
            value = "# This are code"
            # This are a comment.
        "##};
        let data = annotated_for_test(text, "python");
        let checked = data
            .annotation
            .iter()
            .filter_map(|annotation| annotation.as_text())
            .collect::<String>();

        assert!(!checked.contains("This are code"));
        assert!(checked.contains("This are a comment."));
    }

    #[test]
    fn html_masks_script_contents_and_tags() {
        let text = indoc! {"
            <p>This are a tset.</p>
            <script>This are code and should not be checked.</script>
        "};
        let data = annotated_for_test(text, "html");
        let checked = data
            .annotation
            .iter()
            .filter_map(|annotation| annotation.as_text())
            .collect::<String>();
        let markup = data
            .annotation
            .iter()
            .filter_map(|annotation| annotation.as_markup())
            .collect::<String>();

        assert!(checked.contains("This are a tset."));
        assert!(!checked.contains("This are code"));
        assert!(!checked.contains("<p>"));
        assert!(markup.contains("<script>This are code and should not be checked.</script>"));
    }

    fn complement_utf16_ranges(
        text: &str,
        index: &TextIndex,
        ignored_ranges: &[(usize, usize)],
    ) -> Vec<(usize, usize)> {
        let mut checked = Vec::new();
        let mut cursor = 0;
        let total = index.utf16_offset_for_byte(text.len());
        for &(start, end) in ignored_ranges {
            if cursor < start {
                checked.push((cursor, start));
            }
            cursor = cursor.max(end);
        }
        if cursor < total {
            checked.push((cursor, total));
        }
        checked
    }

    #[test]
    fn markdown_annotations_mark_skipped_areas_as_markup() {
        let text = indoc! {"
            This are prose.
            ```rust
            This are code.
            ```
            More prose.
        "};

        let data = annotated_for_test(text, "markdown");
        let checked = data
            .annotation
            .iter()
            .filter_map(|annotation| annotation.as_text())
            .collect::<String>();
        let markup = data
            .annotation
            .iter()
            .filter_map(|annotation| annotation.as_markup())
            .collect::<String>();

        assert!(checked.contains("This are prose."));
        assert!(checked.contains("More prose."));
        assert!(!checked.contains("This are code"));
        assert!(markup.contains("This are code"));
    }

    #[test]
    fn markdown_annotations_keep_context_around_inline_code() {
        let text = "Use `This are inline code` carefully.";
        let data = annotated_for_test(text, "markdown");
        let checked = data
            .annotation
            .iter()
            .filter_map(|annotation| annotation.as_text())
            .collect::<String>();
        let markup = data
            .annotation
            .iter()
            .filter_map(|annotation| annotation.as_markup())
            .collect::<String>();

        assert_eq!(checked, "Use  carefully.");
        assert_eq!(markup, "`This are inline code`");
    }
}
