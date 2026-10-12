//! Shared, stateless Java reading that any module can do without the index.
//!
//! The tree-sitter parser, a file's line-start byte offsets and the
//! byte-offset-to-position conversion they back, and the small reads over a
//! parsed tree — a type node as a [`Ty`], and the file's package. None of this
//! touches the index's data, so a module may read Java without depending on the
//! index's parsing.

use tree_sitter::{Node, Parser, Tree};

use tower_lsp::lsp_types::{Position, Range};

use crate::index::types::{Prim, Ty};

/// A tree-sitter parser for the Java grammar.
pub(crate) fn java_parser() -> Parser {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_java::LANGUAGE.into())
        .expect("failed to load the Java grammar");
    parser
}

/// A file's line-start byte offsets, built once so many byte offsets convert to
/// LSP positions in `O(log n)` each instead of rescanning the text from the top
/// per position — quadratic, and the whole cost of indexing a large file.
#[derive(Clone)]
pub(crate) struct LineIndex {
    pub(crate) starts: Vec<usize>,
}

impl LineIndex {
    pub(crate) fn new(text: &str) -> Self {
        let mut starts = Vec::with_capacity(text.len() / 32 + 1);
        starts.push(0);
        starts.extend(
            text.bytes()
                .enumerate()
                .filter(|(_, byte)| *byte == b'\n')
                .map(|(i, _)| i + 1),
        );
        Self { starts }
    }

    /// The LSP position (line + UTF-16 code units) of a byte offset.
    pub(crate) fn position(&self, text: &str, byte_offset: usize) -> Position {
        let mut offset = byte_offset.min(text.len());
        while offset > 0 && !text.is_char_boundary(offset) {
            offset -= 1;
        }
        let line = self.starts.partition_point(|&start| start <= offset) - 1;
        let line_start = self.starts[line];
        let character = text[line_start..offset]
            .chars()
            .map(|c| c.len_utf16() as u32)
            .sum();
        Position {
            line: line as u32,
            character,
        }
    }

    pub(crate) fn range(&self, text: &str, node: &Node) -> Range {
        Range {
            start: self.position(text, node.start_byte()),
            end: self.position(text, node.end_byte()),
        }
    }
}

/// Converts a type node into a [`Ty`]. Unknown shapes degrade to
/// [`Ty::Unknown`] rather than guessing.
pub(crate) fn type_from_node(node: &Node, text: &str) -> Ty {
    match node.kind() {
        "integral_type" | "floating_point_type" | "boolean_type" => {
            Prim::from_keyword(&text[node.byte_range()])
                .map(Ty::Prim)
                .unwrap_or(Ty::Unknown)
        }
        "void_type" => Ty::Void,
        "type_identifier" | "scoped_type_identifier" => {
            Ty::reference(text[node.byte_range()].to_string())
        }
        "generic_type" => {
            // No fields: the base name and `type_arguments` are both children.
            let mut base = String::new();
            let mut args = Vec::new();
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                match child.kind() {
                    "type_identifier" | "scoped_type_identifier" if base.is_empty() => {
                        base = text[child.byte_range()].to_string();
                    }
                    "type_arguments" => {
                        let mut inner = child.walk();
                        for argument in child.named_children(&mut inner) {
                            args.push(type_from_node(&argument, text));
                        }
                    }
                    _ => {}
                }
            }
            Ty::Ref { name: base, args }
        }
        "array_type" => {
            let element = node
                .child_by_field_name("element")
                .or_else(|| node.named_child(0));
            match element {
                Some(element) => Ty::Array(Box::new(type_from_node(&element, text))),
                None => Ty::Unknown,
            }
        }
        "annotated_type" => node
            .child_by_field_name("type")
            .or_else(|| node.named_child(0))
            .map(|inner| type_from_node(&inner, text))
            .unwrap_or(Ty::Unknown),
        "wildcard" => Ty::Unknown,
        _ => Ty::Unknown,
    }
}

/// The dotted name of the file's package, or `None` for the default package.
pub(crate) fn file_package(tree: &Tree, text: &str) -> Option<String> {
    let root = tree.root_node();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "package_declaration" {
            let mut inner = child.walk();
            return child
                .children(&mut inner)
                .find(|part| part.kind() == "scoped_identifier" || part.kind() == "identifier")
                .map(|part| text[part.byte_range()].to_string());
        }
    }
    None
}
