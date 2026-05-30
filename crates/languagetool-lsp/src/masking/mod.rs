mod comment_blocks;

use crate::language::SupportedLanguage;
use crate::languagetool::{AnnotatedText, Annotation};
use crate::text_index::{BytePosition, ByteRange};
use comment_blocks::{merge_comment_blocks, CommentBlock};
use tree_sitter::{InputEdit, Node, Parser, Point, Tree};
use tree_sitter_md::{MarkdownParser, MarkdownTree};

/// Maintains parser-backed masking state for a document and produces checkable text ranges.
#[derive(Debug)]
pub struct Masker {
    parsed: ParsedMask,
}

impl Masker {
    pub fn new(text: &str, language: SupportedLanguage) -> Self {
        let parsed = ParsedMask::parse(language, text);
        Self { parsed }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn apply_edit(
        &mut self,
        start_byte: usize,
        old_end_byte: usize,
        new_end_byte: usize,
        start_position: BytePosition,
        old_end_position: BytePosition,
        new_end_position: BytePosition,
        new_text: &str,
    ) {
        let edit = InputEdit {
            start_byte,
            old_end_byte,
            new_end_byte,
            start_position: point_from_byte_position(start_position),
            old_end_position: point_from_byte_position(old_end_position),
            new_end_position: point_from_byte_position(new_end_position),
        };
        self.parsed.apply_edit(&edit, new_text);
    }

    /// Returns the list of blocks to be checked by LanguageTool.
    ///
    /// - `PlainText`: one block covering the whole document.
    /// - `Html` / `Markdown`: one block covering the whole document, with
    ///   skipped regions marked as `Markup`.
    /// - `CommentTree`: one block per group of adjacent standalone line
    ///   comments. Blank lines, non-whitespace gaps, inline-to-standalone
    ///   transitions, and block comments split blocks.
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
            ParsedMask::Html { tree, .. } => {
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
            ParsedMask::Markdown { tree, .. } => {
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
            ParsedMask::CommentTree { language, tree, .. } => {
                let mut comment_blocks = Vec::new();
                collect_comment_blocks(text, *language, tree.root_node(), &mut comment_blocks);
                merge_comment_blocks(text, &comment_blocks)
            }
        }
    }
}

/// A contiguous block of document text to be sent to LanguageTool as one request.
///
/// For plain-text, HTML, and Markdown documents there is exactly one block
/// covering the entire document. For comment-tree languages there is one block
/// per group of adjacent standalone line comments. Blank lines, non-whitespace
/// gaps, inline-to-standalone transitions, and block comments split blocks.
#[derive(Debug, Clone)]
pub struct CheckBlock {
    /// Absolute byte span of this block within the document.
    pub byte_range: ByteRange,
    /// Annotated text ready to send to LanguageTool.
    /// `Text` annotations contain comment content; `Markup` annotations contain
    /// comment markers, code between comments, or skipped regions.
    pub annotated: AnnotatedText,
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

impl TryFrom<SupportedLanguage> for CommentTreeLanguage {
    type Error = ();

    fn try_from(value: SupportedLanguage) -> Result<Self, Self::Error> {
        match value {
            SupportedLanguage::Rust => Ok(Self::Rust),
            SupportedLanguage::Scala => Ok(Self::Scala),
            SupportedLanguage::Nix => Ok(Self::Nix),
            SupportedLanguage::Java => Ok(Self::Java),
            SupportedLanguage::Python => Ok(Self::Python),
            SupportedLanguage::Javascript => Ok(Self::Javascript),
            SupportedLanguage::Typescript => Ok(Self::Typescript),
            SupportedLanguage::Tsx => Ok(Self::Tsx),
            SupportedLanguage::PlainText
            | SupportedLanguage::Markdown
            | SupportedLanguage::Html => Err(()),
        }
    }
}

enum ParsedMask {
    CommentTree {
        language: CommentTreeLanguage,
        parser: Parser,
        tree: Tree,
    },
    Html {
        parser: Parser,
        tree: Tree,
    },
    Markdown {
        parser: MarkdownParser,
        tree: MarkdownTree,
    },
    PlainText,
}

impl std::fmt::Debug for ParsedMask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CommentTree { language, .. } => f
                .debug_struct("CommentTree")
                .field("language", language)
                .finish(),
            Self::Html { .. } => f.debug_struct("Html").finish(),
            Self::Markdown { .. } => f.debug_struct("Markdown").finish(),
            Self::PlainText => write!(f, "PlainText"),
        }
    }
}

impl ParsedMask {
    fn parse(language: SupportedLanguage, text: &str) -> Self {
        match language {
            SupportedLanguage::PlainText => Self::PlainText,
            SupportedLanguage::Markdown => {
                let mut parser = MarkdownParser::default();
                let tree = parse_markdown_tree(&mut parser, text, None);
                Self::Markdown { parser, tree }
            }
            SupportedLanguage::Html => {
                let mut parser = html_parser();
                let tree = parse_tree_sitter_tree(&mut parser, text, None);
                Self::Html { parser, tree }
            }
            lang => {
                let language = CommentTreeLanguage::try_from(lang)
                    .expect("remaining SupportedLanguage variants are all Tree-sitter backed");
                let mut parser = comment_tree_parser(language);
                let tree = parse_tree_sitter_tree(&mut parser, text, None);
                Self::CommentTree {
                    tree,
                    language,
                    parser,
                }
            }
        }
    }

    fn apply_edit(&mut self, edit: &InputEdit, text: &str) {
        match self {
            Self::Markdown { parser, tree } => {
                tree.edit(edit);
                *tree = parse_markdown_tree(parser, text, Some(tree));
            }
            Self::Html { parser, tree } => {
                tree.edit(edit);
                *tree = parse_tree_sitter_tree(parser, text, Some(tree));
            }
            Self::CommentTree { parser, tree, .. } => {
                tree.edit(edit);
                *tree = parse_tree_sitter_tree(parser, text, Some(tree));
            }
            Self::PlainText => {}
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Range {
    start: usize,
    end: usize,
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

fn collect_comment_blocks(
    text: &str,
    language: CommentTreeLanguage,
    node: Node<'_>,
    comments: &mut Vec<CommentBlock>,
) {
    if is_comment_node(node) {
        let node_start = node.start_byte();
        let node_end = node.end_byte();
        if let Some(content_range) = comment_content_range(text, language, node) {
            let node_bytes = text.as_bytes().get(node_start..node_end).unwrap_or(&[]);
            let is_block = node_bytes.starts_with(b"/*");
            comments.push(CommentBlock {
                full_range: ByteRange::new(node_start, node_end),
                content_range: ByteRange::new(content_range.start, content_range.end),
                is_block,
                is_standalone_line: comment_starts_line(text, node_start),
            });
        }
        return;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_comment_blocks(text, language, child, comments);
    }
}

fn is_comment_node(node: Node<'_>) -> bool {
    node.kind().contains("comment")
}

fn comment_starts_line(text: &str, node_start: usize) -> bool {
    let line_start = text[..node_start]
        .rfind(['\n', '\r'])
        .map_or(0, |index| index + 1);
    text[line_start..node_start]
        .bytes()
        .all(|byte| matches!(byte, b' ' | b'\t'))
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

fn point_from_byte_position(position: BytePosition) -> Point {
    Point {
        row: position.row,
        column: position.column,
    }
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

/// Returns the text slice `text[start..end]` if the range is non-empty and
/// lies on valid char boundaries; `None` otherwise.
fn get_text_segment(text: &str, start: usize, end: usize) -> Option<&str> {
    if start >= end {
        return None;
    }
    text.get(start..end).filter(|s| !s.is_empty())
}

pub(super) fn push_annotation_text(
    text: &str,
    start: usize,
    end: usize,
    annotations: &mut Vec<Annotation>,
) {
    if let Some(segment) = get_text_segment(text, start, end) {
        annotations.push(Annotation::text(segment.to_string()));
    }
}

pub(super) fn push_annotation_markup(
    text: &str,
    start: usize,
    end: usize,
    annotations: &mut Vec<Annotation>,
) {
    if let Some(segment) = get_text_segment(text, start, end) {
        annotations.push(Annotation::markup(
            segment.to_string(),
            interpret_as_for_markup(segment),
        ));
    }
}

pub(super) fn interpret_as_for_markup(markup: &str) -> Option<String> {
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
mod tests;
