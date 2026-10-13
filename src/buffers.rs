//! The open-buffer store: the editor's unsaved text, one hub client among the
//! others.
//!
//! The editor's documents are the one source of unsaved text, so a module that
//! needs a file's *current* text reads it here rather than from disk. This module
//! is the sole owner of that state ([`DocumentStore`]): it consumes the document
//! lifecycle notifications the hub broadcasts — [`Notification::DocumentOpened`]
//! (full text), [`Notification::DocumentChanged`] (the incremental changes to
//! apply), and [`Notification::DocumentClosed`] — and answers the
//! [`BuffersRequest::Text`] read. [`current_text`] is the convenience a consumer
//! calls: the open buffer's text when the editor has it open, otherwise the file
//! on disk.

use std::collections::HashMap;
use std::sync::Arc;

use tower_lsp::lsp_types::{Position, TextDocumentContentChangeEvent, Url};

use crate::hub::{Hub, HubClient};
use crate::messages::{BuffersRequest, BuffersResponse, Message, Notification, Request, Response};

/// One open buffer: its version and its bytes. Bytes (not `String`) so an
/// incremental change can splice at a byte offset without re-encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    pub version: i32,
    pub bytes: Vec<u8>,
}

/// The open buffers, keyed by URI. The canonical store of unsaved text.
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

/// Starts the buffers module on the hub as the client labelled `buffers`: it
/// owns the [`DocumentStore`], applies the document lifecycle notifications the
/// hub broadcasts, and answers the shell's text reads. The store stays on this
/// one task, so no consumer ever holds the editor's text.
pub fn spawn(hub: &Hub) {
    let hub_client = hub.register("buffers");

    tokio::spawn(async move {
        let mut notifications = hub_client.subscribe().await;
        let mut requests = hub_client.serve().await;
        let mut store = DocumentStore::default();

        loop {
            tokio::select! {
                // Biased: a queued document event is always applied before a read
                // is served, so a read issued after an edit sees the edit — the
                // two arrive on separate channels, and random polling could
                // otherwise serve the read against the pre-edit text.
                biased;
                Some(message) = notifications.recv() => {
                    match &*message {
                        Notification::DocumentOpened { uri, version, text } => {
                            store.open(uri.clone(), *version, text.as_str());
                        }
                        Notification::DocumentChanged { uri, version, changes } => {
                            store.change(uri, *version, changes);
                        }
                        Notification::DocumentClosed { uri } => {
                            store.close(uri);
                        }
                        _ => {}
                    }
                }
                Some(envelope) = requests.recv() => {
                    let Message::Request(Request::Buffers(BuffersRequest::Text { uri })) =
                        &*envelope.message
                    else {
                        continue;
                    };
                    let text = store.get(uri).map(text_of);
                    hub_client.respond(envelope.id, Response::Buffers(BuffersResponse::Text(text)));
                }
                else => break,
            }
        }
    });
}

/// The current text of `uri`: the open buffer's text when the editor has it open,
/// otherwise the file on disk. `None` when neither is available. A consumer that
/// needs a file's text calls this instead of reading the file itself, so an
/// unsaved edit — or an untitled buffer — is visible.
pub async fn current_text(client: &HubClient, uri: &Url) -> Option<Arc<String>> {
    let request = Request::Buffers(BuffersRequest::Text { uri: uri.clone() });
    if let Response::Buffers(BuffersResponse::Text(Some(text))) = client.request(request).await {
        return Some(text);
    }
    let path = uri.to_file_path().ok()?;
    tokio::fs::read_to_string(path).await.ok().map(Arc::new)
}

/// A buffer's text for a reply; the store keeps bytes for position conversion.
fn text_of(document: &Document) -> Arc<String> {
    Arc::new(String::from_utf8_lossy(&document.bytes).into_owned())
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

    #[tokio::test]
    async fn current_text_prefers_the_open_buffer_over_disk() {
        let hub = Hub::new();
        spawn(&hub);
        let client = hub.register("test");
        let uri = uri("Buffer");

        client.notify(Notification::DocumentOpened {
            uri: uri.clone(),
            version: 1,
            text: Arc::new("class Buffer { int from = 1; }\n".to_string()),
        });
        client.notify(Notification::DocumentChanged {
            uri: uri.clone(),
            version: 2,
            changes: vec![change_event(None, "class Buffer { int edited = 2; }\n")],
        });

        let text = current_text(&client, &uri)
            .await
            .expect("the buffer answers");
        assert_eq!(text.as_str(), "class Buffer { int edited = 2; }\n");
    }

    #[tokio::test]
    async fn current_text_answers_an_untitled_buffer() {
        let hub = Hub::new();
        spawn(&hub);
        let client = hub.register("test");
        // No file path: only the open buffer can answer.
        let uri = Url::parse("untitled:Untitled-1").unwrap();

        client.notify(Notification::DocumentOpened {
            uri: uri.clone(),
            version: 1,
            text: Arc::new("class Scratch {}\n".to_string()),
        });

        let text = current_text(&client, &uri)
            .await
            .expect("the buffer answers");
        assert_eq!(text.as_str(), "class Scratch {}\n");
    }

    #[tokio::test]
    async fn current_text_falls_back_to_disk_when_not_open() {
        let dir = std::env::temp_dir().join(format!(
            "java-lsp-buffers-disk-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("OnDisk.java");
        std::fs::write(&path, "class OnDisk {}\n").unwrap();
        let uri = Url::from_file_path(&path).unwrap();

        let hub = Hub::new();
        spawn(&hub);
        let client = hub.register("test");

        let text = current_text(&client, &uri).await.expect("disk answers");
        assert_eq!(text.as_str(), "class OnDisk {}\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
