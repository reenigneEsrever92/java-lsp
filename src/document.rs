//! Versioned document store with incremental text sync.

use std::collections::HashMap;

use tower_lsp::lsp_types::{Position, TextDocumentContentChangeEvent, Url};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    pub version: i32,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Default)]
pub struct DocumentStore {
    documents: HashMap<Url, Document>,
}

impl DocumentStore {
    pub fn open(&mut self, uri: Url, version: i32, text: &str) {
        self.documents.insert(
            uri,
            Document {
                version,
                bytes: text.as_bytes().to_vec(),
            },
        );
    }

    /// Applies the change events in order and updates the document version.
    /// Returns false if the document is not open.
    pub fn change(
        &mut self,
        uri: &Url,
        version: i32,
        changes: &[TextDocumentContentChangeEvent],
    ) -> bool {
        let Some(doc) = self.documents.get_mut(uri) else {
            return false;
        };
        for event in changes {
            match event.range {
                None => doc.bytes = event.text.as_bytes().to_vec(),
                Some(range) => {
                    // An out-of-range position is skipped rather than failing
                    // the whole batch; the document stays at its last good text.
                    let (Some(start), Some(end)) = (
                        position_to_offset(&doc.bytes, range.start),
                        position_to_offset(&doc.bytes, range.end),
                    ) else {
                        tracing::warn!(uri = %uri, ?range, "ignoring change with out-of-range positions");
                        continue;
                    };
                    if start <= end && end <= doc.bytes.len() {
                        doc.bytes
                            .splice(start..end, event.text.as_bytes().iter().copied());
                    }
                }
            }
            doc.version = version;
        }
        true
    }

    pub fn close(&mut self, uri: &Url) -> bool {
        self.documents.remove(uri).is_some()
    }

    pub fn get(&self, uri: &Url) -> Option<&Document> {
        self.documents.get(uri)
    }
}

/// Converts an LSP position (line + UTF-16 code units) to a byte offset.
/// A character past the end of the line clamps to the line end.
fn position_to_offset(bytes: &[u8], position: Position) -> Option<usize> {
    let mut line: u32 = 0;
    let mut line_start: usize = 0;
    while line < position.line {
        let newline = bytes[line_start..].iter().position(|&b| b == b'\n')?;
        line_start += newline + 1;
        line += 1;
    }
    let line_end = bytes[line_start..]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(bytes.len(), |nl| line_start + nl);
    let text = std::str::from_utf8(&bytes[line_start..line_end]).ok()?;
    let mut utf16_units: u32 = 0;
    for (offset, ch) in text.char_indices() {
        if utf16_units >= position.character {
            return Some(line_start + offset);
        }
        utf16_units += ch.len_utf16() as u32;
    }
    Some(line_end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp::lsp_types::Range;

    /// Builds a `TextDocumentContentChangeEvent`; `range` is
    /// `Some((start_line, start_char)..(end_line, end_char))` for incremental
    /// edits, `None` for a full-document replacement.
    fn change_event(
        range: Option<std::ops::Range<(u32, u32)>>,
        text: &str,
    ) -> TextDocumentContentChangeEvent {
        TextDocumentContentChangeEvent {
            range: range.map(|r| Range {
                start: Position::new(r.start.0, r.start.1),
                end: Position::new(r.end.0, r.end.1),
            }),
            range_length: None,
            text: text.to_string(),
        }
    }

    fn uri(name: &str) -> Url {
        Url::parse(&format!("file:///{name}.java")).unwrap()
    }

    fn store_with(name: &str, text: &str) -> (DocumentStore, Url) {
        let mut store = DocumentStore::default();
        let uri = uri(name);
        store.open(uri.clone(), 1, text);
        (store, uri)
    }

    #[test]
    fn open_tracks_version_and_text() {
        let (store, uri) = store_with("A", "class A {}\n");
        let doc = store.get(&uri).unwrap();
        assert_eq!(doc.version, 1);
        assert_eq!(doc.bytes, b"class A {}\n");
    }

    #[test]
    fn incremental_change_splices_and_bumps_version() {
        let (mut store, uri) = store_with("A", "class A {}\n");
        let changed = store.change(&uri, 2, &[change_event(Some((0, 6)..(0, 7)), "B")]);
        assert!(changed);
        let doc = store.get(&uri).unwrap();
        assert_eq!(doc.version, 2);
        assert_eq!(doc.bytes, b"class B {}\n");
    }

    #[test]
    fn mixed_full_and_incremental_sequence() {
        let (mut store, uri) = store_with("A", "class A {}\n");
        store.change(&uri, 2, &[change_event(None, "int x = 1;\nint y = 2;\n")]);
        store.change(
            &uri,
            3,
            &[
                change_event(Some((0, 4)..(0, 5)), "w"),
                change_event(Some((0, 0)..(0, 0)), "// top\n"),
            ],
        );
        let doc = store.get(&uri).unwrap();
        assert_eq!(doc.version, 3);
        assert_eq!(doc.bytes, b"// top\nint w = 1;\nint y = 2;\n");
    }

    #[test]
    fn change_on_unopened_document_fails() {
        let mut store = DocumentStore::default();
        assert!(!store.change(
            &uri("Missing"),
            1,
            &[change_event(None, "class Missing {}\n")],
        ));
    }

    #[test]
    fn close_removes_document() {
        let (mut store, uri) = store_with("A", "class A {}\n");
        assert!(store.close(&uri));
        assert!(store.get(&uri).is_none());
        assert!(!store.close(&uri));
    }

    #[test]
    fn positions_use_utf16_units() {
        // "😀" is one UTF-16 surrogate pair (2 units), 4 bytes.
        let text = "int \u{1F600} = 1;\n";
        let (mut store, uri) = store_with("Emoji", text);
        let doc = store.get(&uri).unwrap().clone();
        // Position after the emoji, before " = 1;" -> character 6 in UTF-16.
        let offset = position_to_offset(&doc.bytes, Position::new(0, 6)).unwrap();
        assert_eq!(&doc.bytes[offset..], b" = 1;\n");
        // Editing at that position must not split the emoji's UTF-8 bytes.
        store.change(&uri, 2, &[change_event(Some((0, 6)..(0, 6)), "*")]);
        assert_eq!(
            store.get(&uri).unwrap().bytes,
            "int \u{1F600}* = 1;\n".as_bytes()
        );
    }

    #[test]
    fn position_past_line_end_clamps_to_line_end() {
        let (store, uri) = store_with("A", "ab\ncd\n");
        let doc = store.get(&uri).unwrap();
        let offset = position_to_offset(&doc.bytes, Position::new(0, 99)).unwrap();
        assert_eq!(offset, 2);
    }

    #[test]
    fn position_beyond_last_line_is_none() {
        let (store, uri) = store_with("A", "ab\n");
        let doc = store.get(&uri).unwrap();
        assert_eq!(position_to_offset(&doc.bytes, Position::new(5, 0)), None);
    }
}
