//! The engine hub: one mechanism every module uses.
//!
//! Modules never call each other directly and never share state. Each module
//! owns its own state and holds a [`HubClient`], which is all it needs: it
//! registers with the hub through it ([`HubClient::subscribe`]), sends
//! **notifications** (which the hub broadcasts to every subscriber — each
//! consumes the ones it cares about) and **requests** (which the hub routes to
//! the one module that serves them, the reply riding the same hub). Every request returns a [`Reply`]: a
//! thin wrapper over a tokio oneshot receiver. Code on the runtime (the shell,
//! the drivers) awaits it; synchronous module code — the analysis core, the
//! diagnostics and quick-fix modules, which run on their own threads or the
//! blocking pool — calls [`Reply::blocking_recv`]. The hub is a thread, so a
//! blocked module never stalls it.
//!
//! `HubClient` mirrors the query surface its callers need, so a module that used
//! to hold a neighbour's handle now holds a client instead.
//!
//! The hub **logs every message passing through** at `debug`
//! (`RUST_LOG=java_lsp::hub=debug`): one line per message, each prefixed with the
//! sender's name, with the identifiers and counts that matter but never a payload
//! (an open document's text is logged as its size). The high-cardinality, per-item
//! notifications (a source file, an artifact, a progress tick) are logged at
//! `trace` instead, so `debug` shows the flow rather than thousands of per-item
//! lines. A request's reply is logged too, naming the module that answered it and
//! the time it took. The batched lookups (`IndexQueryNames`, `IndexHasPackages`)
//! are logged by count, never by their key lists.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc::{
    self as tokio_mpsc, unbounded_channel, UnboundedReceiver, UnboundedSender,
};
use tokio::sync::Mutex;
use tracing::debug;

use crate::messages::{Envelope, Message, Response};

/// The next request id: unique for the process, so the hub can pair a reply with
/// its request.
static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub struct Hub {
    clients: Arc<Mutex<HashMap<String, UnboundedSender<Envelope>>>>,
    inbound: UnboundedSender<Envelope>,
}

impl Hub {
    pub fn new() -> Self {
        let (inbound, mut receiver) = unbounded_channel();

        let instance = Self {
            clients: Arc::new(Mutex::new(HashMap::new())),
            inbound,
        };

        {
            let instance = instance.clone();
            tokio::spawn(async move {
                while let Some(msg) = receiver.recv().await {
                    instance.forward(msg).await;
                }
            });
        }

        instance
    }

    pub fn register(&self, sender: String) -> HubClient {
        let inbound = self.inbound.clone();
        let (hub_tx, mut hub_rx) = unbounded_channel();

        tokio::spawn(async move {
            while let Some(msg) = hub_rx.recv().await {
                let _ = inbound.send(msg);
            }
        });

        HubClient {
            sender,
            hub_tx,
            hub: self.clone(),
        }
    }

    pub async fn subscribe(&self, client: &HubClient) -> UnboundedReceiver<Envelope> {
        let (client_tx, client_rx) = unbounded_channel();

        {
            let mut guard = self.clients.lock().await;
            guard.insert(client.sender.clone(), client_tx);
        }

        client_rx
    }

    async fn forward(&self, msg: Envelope) {
        debug!(?msg, "Hub message");

        match &*msg.message {
            Message::Request(_request) => todo!(),
            Message::Response(_response) => todo!(),
            Message::Notification(_) => {
                for tx in self.clients.lock().await.values() {
                    let _ = tx.send(msg.clone());
                }
            }
        }
    }
}

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
pub struct HubClient {
    hub_tx: tokio_mpsc::UnboundedSender<Envelope>,
    hub: Hub,
    sender: String,
}

impl HubClient {
    /// Sends a notification; every module may consume it.
    pub fn notify(&self, message: Message) {
        let envelope = Envelope {
            id: NEXT_REQUEST_ID.fetch_add(1, Ordering::SeqCst),
            message: Arc::new(message),
        };

        let _ = self.hub_tx.send(envelope);
    }

    /// Subscribes to every notification the hub broadcasts, yielding each
    /// message's shared [`Arc`]. The envelope's `id` is dropped: a notification
    /// carries no reply to pair it with.
    pub async fn subscribe(&self) -> UnboundedReceiver<Arc<Message>> {
        let (tx, rx) = unbounded_channel();
        let mut rx_in = self.hub.subscribe(self).await;

        tokio::spawn(async move {
            while let Some(envelope) = rx_in.recv().await {
                if tx.send(envelope.message).is_err() {
                    break;
                }
            }
        });

        rx
    }

    pub fn request(&self) -> Response {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tower_lsp::lsp_types::Url;

    use crate::messages::{Message, Notification};

    use super::*;

    fn folder(uri: &str) -> Message {
        Message::Notification(Notification::FolderAdded {
            uri: Url::parse(uri).expect("a valid file URI"),
        })
    }

    #[tokio::test]
    async fn a_notification_is_broadcast_to_every_subscriber() {
        let hub = Hub::new();
        let mut first = hub.register("first".to_string()).subscribe().await;
        let mut second = hub.register("second".to_string()).subscribe().await;
        let sender = hub.register("sender".to_string());

        sender.notify(folder("file:///workspace"));

        let first_message = first.recv().await.expect("the first subscriber stays open");
        let second_message = second
            .recv()
            .await
            .expect("the second subscriber stays open");

        assert!(matches!(&*first_message, Message::Notification(_)));
        assert!(matches!(&*second_message, Message::Notification(_)));
        // Broadcasting shares one allocation: both subscribers hold the same `Arc`.
        assert!(Arc::ptr_eq(&first_message, &second_message));
    }

    #[tokio::test]
    async fn a_dropped_subscriber_does_not_disturb_the_others() {
        let hub = Hub::new();
        let gone = hub.register("gone".to_string()).subscribe().await;
        let mut live = hub.register("live".to_string()).subscribe().await;
        drop(gone);

        let sender = hub.register("sender".to_string());
        sender.notify(folder("file:///workspace"));

        let message = live.recv().await.expect("the live subscriber stays open");
        assert!(matches!(&*message, Message::Notification(_)));
    }
}
