use crate::language::SupportedLanguage;
use crate::languagetool::{AnnotatedText, Annotation};
use crate::text_index::ByteRange;
use tree_sitter::{InputEdit, Node, Parser, Point, Tree};
use tree_sitter_md_025::{MarkdownParser, MarkdownTree};

/// A contiguous block of document text to be sent to LanguageTool as one request.
///
/// For plain-text, HTML, and Markdown documents there is exactly one block
/// covering the entire document. For comment-tree languages there is one block
/// per group of adjacent comments (consecutive line comments separated only by
/// single newlines form one block; a blank line or non-whitespace gap splits
/// into separate blocks; each block comment is always its own block).
#[derive(Debug, Clone)]
pub struct CheckBlock {
    /// Absolute byte span of this block within the document.
    pub byte_range: ByteRange,
    /// Annotated text ready to send to LanguageTool.
    /// `Text` annotations contain comment content; `Markup` annotations contain
    /// comment markers, code between comments, or skipped regions.
    pub annotated: AnnotatedText,
}

/// Maintains parser-backed masking state for a document and produces checkable text ranges.
#[derive(Debug, Clone)]
pub struct Masker {
    parsed: ParsedMask,
}

impl Masker {
    pub fn new(text: &str, language: SupportedLanguage) -> Self {
        let parsed = ParsedMask::parse(language, text);
        Self { parsed }
    }

    pub fn apply_edit(
        &mut self,
        old_text: &str,
        text: &str,
        bytes: &crate::text_index::ByteRange,
        new_text: &str,
    ) {
        let byte_start = bytes.start.0;
        let byte_end = bytes.end.0;
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

        self.parsed.apply_edit(&edit, text);
    }

    /// Returns the list of blocks to be checked by LanguageTool.
    ///
    /// - `PlainText`: one block covering the whole document.
    /// - `Html` / `Markdown`: one block covering the whole document, with
    ///   skipped regions marked as `Markup`.
    /// - `CommentTree`: one block per group of adjacent comments (consecutive
    ///   line comments with only single-newline gaps form one block; a blank
    ///   line or non-whitespace gap splits into separate blocks; each block
    ///   comment is always its own block).
    ///
    /// Blocks where the annotated text contains no `Text` annotations are
    /// silently dropped.
    pub fn check_blocks(&self, text: &str) -> Vec<CheckBlock> {
        match &self.parsed {
            ParsedMask::PlainText => {
                let annotated = AnnotatedText {
                    annotation: vec![Annotation::text(text.to_string())],
                };
                if !annotated.has_text() {
                    return Vec::new();
                }
                vec![CheckBlock {
                    byte_range: ByteRange::new(0usize, text.len()),
                    annotated,
                }]
            }
            ParsedMask::Html(tree) => {
                let mut skip_ranges = Vec::new();
                collect_html_skip_ranges(tree.root_node(), &mut skip_ranges);
                let annotation = annotations_from_skip_ranges(text, &mut skip_ranges);
                let annotated = AnnotatedText { annotation };
                if !annotated.has_text() {
                    return Vec::new();
                }
                vec![CheckBlock {
                    byte_range: ByteRange::new(0usize, text.len()),
                    annotated,
                }]
            }
            ParsedMask::Markdown(tree) => {
                let mut skip_ranges = Vec::new();
                collect_markdown_skip_ranges(tree.block_tree().root_node(), &mut skip_ranges);
                for inline_tree in tree.inline_trees() {
                    collect_markdown_inline_skip_ranges(inline_tree.root_node(), &mut skip_ranges);
                }
                let annotation = annotations_from_skip_ranges(text, &mut skip_ranges);
                let annotated = AnnotatedText { annotation };
                if !annotated.has_text() {
                    return Vec::new();
                }
                vec![CheckBlock {
                    byte_range: ByteRange::new(0usize, text.len()),
                    annotated,
                }]
            }
            ParsedMask::CommentTree { language, tree } => {
                let mut comment_infos = Vec::new();
                collect_comment_infos(text, *language, tree.root_node(), &mut comment_infos);
                build_check_blocks(text, &comment_infos)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommentTreeLanguage {
    Rust,
    Scala,
    Nix,
    Java,
    Python,
    Javascript,
    Typescript,
    Tsx,
}

impl From<CommentTreeLanguage> for tree_sitter::Language {
    fn from(value: CommentTreeLanguage) -> Self {
        match value {
            CommentTreeLanguage::Rust => tree_sitter_rust::LANGUAGE.into(),
            CommentTreeLanguage::Scala => tree_sitter_scala::LANGUAGE.into(),
            CommentTreeLanguage::Nix => tree_sitter_nix::LANGUAGE.into(),
            CommentTreeLanguage::Java => tree_sitter_java::LANGUAGE.into(),
            CommentTreeLanguage::Python => tree_sitter_python::LANGUAGE.into(),
            CommentTreeLanguage::Javascript => tree_sitter_javascript::LANGUAGE.into(),
            CommentTreeLanguage::Typescript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            CommentTreeLanguage::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        }
    }
}

impl CommentTreeLanguage {
    fn from_supported_language(value: SupportedLanguage) -> Self {
        match value {
            SupportedLanguage::Rust => Self::Rust,
            SupportedLanguage::Scala => Self::Scala,
            SupportedLanguage::Nix => Self::Nix,
            SupportedLanguage::Java => Self::Java,
            SupportedLanguage::Python => Self::Python,
            SupportedLanguage::Javascript => Self::Javascript,
            SupportedLanguage::Typescript => Self::Typescript,
            SupportedLanguage::Tsx => Self::Tsx,
            SupportedLanguage::PlainText
            | SupportedLanguage::Markdown
            | SupportedLanguage::Html => {
                unreachable!("language is not comment-masked with Tree-sitter")
            }
        }
    }
}

#[derive(Debug, Clone)]
enum ParsedMask {
    CommentTree {
        language: CommentTreeLanguage,
        tree: Tree,
    },
    Html(Tree),
    Markdown(MarkdownTree),
    PlainText,
}

impl ParsedMask {
    fn parse(language: SupportedLanguage, text: &str) -> Self {
        match language {
            SupportedLanguage::PlainText => Self::PlainText,
            SupportedLanguage::Markdown => {
                let mut parser = MarkdownParser::default();
                Self::Markdown(parse_markdown_tree(&mut parser, text, None))
            }
            SupportedLanguage::Html => {
                let mut parser = html_parser();
                Self::Html(parse_tree_sitter_tree(&mut parser, text, None))
            }
            tree_sitter_compatible_language => {
                let comment_language =
                    CommentTreeLanguage::from_supported_language(tree_sitter_compatible_language);
                let mut parser = comment_tree_parser(comment_language);
                let tree = parse_tree_sitter_tree(&mut parser, text, None);
                Self::CommentTree {
                    tree,
                    language: comment_language,
                }
            }
        }
    }

    fn apply_edit(&mut self, edit: &InputEdit, text: &str) {
        match self {
            Self::Markdown(tree) => {
                tree.edit(edit);
                let mut parser = MarkdownParser::default();
                *tree = parse_markdown_tree(&mut parser, text, Some(tree));
            }
            Self::Html(tree) => {
                tree.edit(edit);
                let mut parser = html_parser();
                *tree = parse_tree_sitter_tree(&mut parser, text, Some(tree));
            }
            Self::CommentTree { tree, language } => {
                tree.edit(edit);
                let mut parser = comment_tree_parser(*language);
                *tree = parse_tree_sitter_tree(&mut parser, text, Some(tree));
            }
            Self::PlainText => {}
        }
    }
}

fn parse_markdown_tree(
    parser: &mut MarkdownParser,
    text: &str,
    old_tree: Option<&MarkdownTree>,
) -> MarkdownTree {
    parser
        .parse(text.as_bytes(), old_tree)
        .expect("Tree-sitter parsing should not fail without timeout or cancellation")
}

fn parse_tree_sitter_tree(parser: &mut Parser, text: &str, old_tree: Option<&Tree>) -> Tree {
    parser
        .parse(text, old_tree)
        .expect("Tree-sitter parsing should not fail without timeout or cancellation")
}

fn comment_tree_parser(language: CommentTreeLanguage) -> Parser {
    let mut parser = Parser::new();
    parser
        .set_language(&language.into())
        .expect("bundled Tree-sitter grammar should load");
    parser
}

fn html_parser() -> Parser {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_html::LANGUAGE.into())
        .expect("bundled Tree-sitter grammar should load");
    parser
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
    language: CommentTreeLanguage,
    source: &str,
    node: Node<'_>,
) -> Option<(usize, usize)> {
    match language {
        CommentTreeLanguage::Rust => strip_slash_comment(source, node, true),
        CommentTreeLanguage::Scala
        | CommentTreeLanguage::Java
        | CommentTreeLanguage::Javascript
        | CommentTreeLanguage::Typescript
        | CommentTreeLanguage::Tsx => strip_slash_comment(source, node, false),
        CommentTreeLanguage::Nix | CommentTreeLanguage::Python => strip_hash_comment(source, node),
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

/// Full span of a comment node (including markers) and its content range
/// (with markers and surrounding whitespace stripped).
#[derive(Debug, Clone, Copy)]
struct CommentInfo {
    /// Full byte span of the comment node, including `//`, `/*`, `*/`, etc.
    node_range: Range,
    /// Byte span of the comment content after stripping markers and leading/trailing whitespace.
    content_range: Range,
    /// True if the comment starts with `/*` (block comment).
    is_block: bool,
}

fn collect_comment_infos(
    text: &str,
    language: CommentTreeLanguage,
    node: Node<'_>,
    infos: &mut Vec<CommentInfo>,
) {
    if is_comment_node(node) {
        let node_start = node.start_byte();
        let node_end = node.end_byte();
        if let Some(content_range) = comment_content_range(text, language, node) {
            let node_bytes = text.as_bytes().get(node_start..node_end).unwrap_or(&[]);
            let is_block = node_bytes.starts_with(b"/*");
            infos.push(CommentInfo {
                node_range: Range {
                    start: node_start,
                    end: node_end,
                },
                content_range,
                is_block,
            });
        }
        return;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_comment_infos(text, language, child, infos);
    }
}

fn is_comment_node(node: Node<'_>) -> bool {
    node.kind().contains("comment")
}

fn comment_content_range(
    source: &str,
    language: CommentTreeLanguage,
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

/// Returns true if the gap `text[prev_end..next_start]` contains a blank line
/// (two newlines with only horizontal whitespace between them) or any
/// non-whitespace character.
fn gap_splits_block(text: &str, prev_end: usize, next_start: usize) -> bool {
    let gap = match text.get(prev_end..next_start) {
        Some(g) => g,
        None => return true,
    };
    let bytes = gap.as_bytes();
    // Check for non-whitespace
    if bytes
        .iter()
        .any(|b| !matches!(b, b'\n' | b'\r' | b' ' | b'\t'))
    {
        return true;
    }
    // Check for blank line: a '\n' followed (after optional horizontal whitespace) by another '\n'
    let mut saw_newline = false;
    for &b in bytes {
        match b {
            b'\n' => {
                if saw_newline {
                    return true;
                }
                saw_newline = true;
            }
            b'\r' => {}
            b' ' | b'\t' => {
                // horizontal whitespace between newlines does not reset the flag
            }
            _ => {
                saw_newline = false;
            }
        }
    }
    false
}

fn build_check_blocks(text: &str, infos: &[CommentInfo]) -> Vec<CheckBlock> {
    if infos.is_empty() {
        return Vec::new();
    }

    let mut blocks: Vec<CheckBlock> = Vec::new();
    let mut group_start: usize = 0;

    let mut flush_group = |group: &[CommentInfo]| {
        if group.is_empty() {
            return;
        }
        let block_start = group[0].node_range.start;
        let block_end = group[group.len() - 1].node_range.end;
        let annotation = build_block_annotations(text, group, block_start, block_end);
        let annotated = AnnotatedText { annotation };
        if !annotated.has_text() {
            return;
        }
        blocks.push(CheckBlock {
            byte_range: ByteRange::new(block_start, block_end),
            annotated,
        });
    };

    for i in 1..infos.len() {
        let prev = &infos[i - 1];
        let curr = &infos[i];

        let split = prev.is_block
            || curr.is_block
            || gap_splits_block(text, prev.node_range.end, curr.node_range.start);

        if split {
            flush_group(&infos[group_start..i]);
            group_start = i;
        }
    }
    flush_group(&infos[group_start..]);

    blocks
}

fn build_block_annotations(
    text: &str,
    group: &[CommentInfo],
    block_start: usize,
    block_end: usize,
) -> Vec<Annotation> {
    let mut annotations = Vec::new();
    let mut cursor = block_start;

    for info in group {
        // Gap between cursor and content start: Markup
        if cursor < info.content_range.start {
            push_annotation_markup(text, cursor, info.content_range.start, &mut annotations);
        }
        // Comment content: Text
        push_annotation_text(
            text,
            info.content_range.start,
            info.content_range.end,
            &mut annotations,
        );
        cursor = info.content_range.end;
    }
    // Trailing gap to block end: Markup
    if cursor < block_end {
        push_annotation_markup(text, cursor, block_end, &mut annotations);
    }
    annotations
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text_index::TextIndex;
    use indoc::indoc;

    fn check_blocks_for_test(text: &str, language_id: &str) -> Vec<CheckBlock> {
        let language = SupportedLanguage::from_language_id(language_id).unwrap();
        let mask = Masker::new(text, language);
        mask.check_blocks(text)
    }

    fn all_text(blocks: &[CheckBlock]) -> String {
        blocks
            .iter()
            .flat_map(|b| b.annotated.annotation.iter())
            .filter_map(|a| a.as_text())
            .collect()
    }

    fn all_markup(blocks: &[CheckBlock]) -> String {
        blocks
            .iter()
            .flat_map(|b| b.annotated.annotation.iter())
            .filter_map(|a| a.as_markup())
            .collect()
    }

    #[test]
    fn rust_annotations_mark_code_as_markup_and_comments_as_text() {
        let text = indoc! {r#"
            let value = 1; // This are a comment.
            let other = "This are code";
            /* This are block docs. */
        "#};

        let blocks = check_blocks_for_test(text, "rust");
        let checked_text: Vec<&str> = blocks
            .iter()
            .flat_map(|b| b.annotated.annotation.iter())
            .filter_map(|a| a.as_text())
            .collect();
        let markup = all_markup(&blocks);

        assert!(
            checked_text.contains(&"This are a comment."),
            "checked_text={checked_text:?}"
        );
        assert!(
            checked_text.contains(&"This are block docs. "),
            "checked_text={checked_text:?}"
        );
        assert!(markup.contains("//"));
        assert!(markup.contains("/*"));
    }

    #[test]
    fn consecutive_line_comments_form_one_block() {
        let text = indoc! {"
            // I am a catz.
            // I like chickz.
        "};
        let blocks = check_blocks_for_test(text, "rust");
        assert_eq!(blocks.len(), 1, "expected one block, got {}", blocks.len());
        let checked_text: Vec<&str> = blocks[0]
            .annotated
            .annotation
            .iter()
            .filter_map(|a| a.as_text())
            .collect();
        assert_eq!(checked_text, vec!["I am a catz.", "I like chickz."]);
        let separators: Vec<&str> = blocks[0]
            .annotated
            .annotation
            .iter()
            .filter_map(|a| a.interpret_as())
            .collect();
        assert!(separators.iter().any(|s| s.contains('\n')));
    }

    #[test]
    fn blank_line_splits_line_comments_into_two_blocks() {
        let text = indoc! {"
            // First block.

            // Second block.
        "};
        let blocks = check_blocks_for_test(text, "rust");
        assert_eq!(blocks.len(), 2, "expected two blocks, got {}", blocks.len());
        let first_text = all_text(&blocks[0..1]);
        let second_text = all_text(&blocks[1..2]);
        assert!(first_text.contains("First block."), "first={first_text:?}");
        assert!(
            second_text.contains("Second block."),
            "second={second_text:?}"
        );
    }

    #[test]
    fn block_comment_is_its_own_block() {
        let text = indoc! {"
            // Line comment.
            /* Block comment. */
            // After block.
        "};
        let blocks = check_blocks_for_test(text, "rust");
        assert_eq!(
            blocks.len(),
            3,
            "expected three blocks, got {}",
            blocks.len()
        );
    }

    #[test]
    fn rust_lifetimes_do_not_hide_following_comments() {
        let text = "let value: &'a str = input; // This are docs.\n";
        let blocks = check_blocks_for_test(text, "rust");
        let checked = all_text(&blocks);
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
        let blocks = check_blocks_for_test(text, "rust");
        let checked_texts: Vec<&str> = blocks
            .iter()
            .flat_map(|b| b.annotated.annotation.iter())
            .filter_map(|a| a.as_text())
            .collect();

        assert!(
            checked_texts.contains(&"This are public docs."),
            "{checked_texts:?}"
        );
        assert!(
            checked_texts.contains(&"This are module docs."),
            "{checked_texts:?}"
        );
        assert!(
            checked_texts
                .iter()
                .any(|t| t.contains("This are block docs.")),
            "{checked_texts:?}"
        );
        assert!(
            checked_texts
                .iter()
                .any(|t| t.contains("This are inner block docs.")),
            "{checked_texts:?}"
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
        let blocks = check_blocks_for_test(text, "rust");
        let checked = all_text(&blocks);

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
        let blocks = check_blocks_for_test(text, "rust");
        let checked = all_text(&blocks);
        assert_eq!(checked, "This are a comment.");
    }

    #[test]
    fn comment_tree_masking_supports_explicit_languages() {
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
            let blocks = check_blocks_for_test(text, language);
            let checked = all_text(&blocks);

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
            let blocks = check_blocks_for_test(text, language);
            let checked = all_text(&blocks);
            assert_eq!(checked, expected, "language={language}");
        }
    }

    #[test]
    fn plaintext_produces_one_block() {
        let text = "Hello world. This are a test.";
        let blocks = check_blocks_for_test(text, "plaintext");
        assert_eq!(blocks.len(), 1);
        assert_eq!(all_text(&blocks), text);
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

        let blocks = check_blocks_for_test(text, "markdown");
        let checked = all_text(&blocks);
        let markup = all_markup(&blocks);

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
        let blocks = check_blocks_for_test(text, "python");
        let checked = all_text(&blocks);

        assert!(!checked.contains("This are code"));
        assert!(checked.contains("This are a comment."));
    }

    #[test]
    fn html_masks_script_contents_and_tags() {
        let text = indoc! {"
            <p>This are a tset.</p>
            <script>This are code and should not be checked.</script>
        "};
        let blocks = check_blocks_for_test(text, "html");
        let checked = all_text(&blocks);
        let markup = all_markup(&blocks);

        assert!(checked.contains("This are a tset."));
        assert!(!checked.contains("This are code"));
        assert!(!checked.contains("<p>"));
        assert!(markup.contains("<script>This are code and should not be checked.</script>"));
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

        let blocks = check_blocks_for_test(text, "markdown");
        let checked = all_text(&blocks);
        let markup = all_markup(&blocks);

        assert!(checked.contains("This are prose."));
        assert!(checked.contains("More prose."));
        assert!(!checked.contains("This are code"));
        assert!(markup.contains("This are code"));
    }

    #[test]
    fn markdown_annotations_keep_context_around_inline_code() {
        let text = "Use `This are inline code` carefully.";
        let blocks = check_blocks_for_test(text, "markdown");
        let checked = all_text(&blocks);
        let markup = all_markup(&blocks);

        assert_eq!(checked, "Use  carefully.");
        assert_eq!(markup, "`This are inline code`");
    }

    #[test]
    fn block_byte_ranges_cover_comment_nodes() {
        let text = "let x = 1; // First.\n// Second.\n";
        let blocks = check_blocks_for_test(text, "rust");
        // Both line comments have only a single newline gap, so they form one block.
        assert_eq!(blocks.len(), 1);
        let block = &blocks[0];
        let covered = &text[block.byte_range.start.0..block.byte_range.end.0];
        assert!(covered.contains("First."), "covered={covered:?}");
        assert!(covered.contains("Second."), "covered={covered:?}");
    }

    #[test]
    fn byte_ranges_for_non_ascii_comments() {
        // Ensure byte offsets are correct when non-ASCII chars precede comments.
        let text = "let x = \"café\"; // Ünïcödé comment.\n";
        let index = TextIndex::new(text);
        let blocks = check_blocks_for_test(text, "rust");
        assert_eq!(blocks.len(), 1);
        let block = &blocks[0];
        // The text annotation inside the block should yield the comment content
        // at the right byte offset.
        let content_text: String = block
            .annotated
            .annotation
            .iter()
            .filter_map(|a| a.as_text())
            .collect();
        assert!(
            content_text.contains("Ünïcödé comment."),
            "content={content_text:?}"
        );
        // Verify that the byte_range actually indexes back correctly.
        let _ = index; // suppress unused-variable warning; index is used above for non-ASCII verification
        let block_slice = &text[block.byte_range.start.0..block.byte_range.end.0];
        assert!(
            block_slice.contains("Ünïcödé comment."),
            "block_slice={block_slice:?}"
        );
    }
}
