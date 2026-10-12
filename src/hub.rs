//! The engine hub: one mechanism every module uses.
//!
//! Modules never call each other directly and never share state. Each module
//! owns its own state and holds a [`HubClient`] — the hub connection it was
//! registered with ([`Hub::register`]) — which is all it needs: it subscribes to
//! the **notifications** the hub broadcasts, serves the **requests** routed to it
//! ([`HubClient::serve`]), sends **requests** of its own (which the hub routes to
//! the one module that serves them, the reply riding the same hub), and answers
//! the requests it owns ([`HubClient::respond`]).
//! Every request returns a [`Reply`]: a thin wrapper over a tokio oneshot
//! receiver. Code on the runtime (the shell, the drivers) awaits it; synchronous
//! module code — the analysis core, the diagnostics and quick-fix modules, which
//! run on their own threads or the blocking pool — calls [`Reply::blocking_recv`].
//! The hub runs on its own task, so a blocked module never stalls it.
//!
//! A request is addressed to the module that owns it ([`owner_of`]); the hub
//! routes it there and remembers who asked, so the answer the owner sends with
//! [`HubClient::respond`] is routed back to that one requester. A request with no
//! registered owner resolves to the empty reply instead of leaving the requester
//! waiting.
//!
//! `HubClient` mirrors the query surface its callers need, so a module that used
//! to hold a neighbour's handle now holds a client instead.
//!
//! The hub **logs every message passing through** ([`Hub::forward`]), so the whole
//! flow is observable from one place (`RUST_LOG=java_lsp::hub=info`).

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::task::{Context, Poll};

use tokio::sync::mpsc::{
    self as tokio_mpsc, unbounded_channel, UnboundedReceiver, UnboundedSender,
};
use tokio::sync::{oneshot, Mutex};
use tracing::info;

use crate::messages::{Envelope, Message, Notification, Request, Response};

/// The next request id: unique for the process, so the hub can pair a reply with
/// its request.
static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

/// The hub: the mediator every module talks through, and nothing more.
#[derive(Clone)]
pub struct Hub {
    /// Each registered client's inbound channel, keyed by the name it logs under.
    /// Everything the hub sends a client — broadcast notification, routed request,
    /// routed reply — goes here.
    clients: Arc<StdMutex<HashMap<&'static str, UnboundedSender<Envelope>>>>,
    /// Every client's outbound channel funnels into this one, so the hub sees
    /// every message in arrival order.
    inbound: UnboundedSender<Envelope>,
    /// The requests routed and still awaiting a reply, keyed by the request's id
    /// and holding the requester's name, so the reply can find its way back.
    requests: Arc<StdMutex<HashMap<u64, &'static str>>>,
}

impl Hub {
    pub fn new() -> Self {
        let (inbound, mut receiver) = unbounded_channel();

        let instance = Self {
            clients: Arc::new(StdMutex::new(HashMap::new())),
            inbound,
            requests: Arc::new(StdMutex::new(HashMap::new())),
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

    /// Registers a client under `sender` and returns its [`HubClient`].
    ///
    /// The client's inbound channel is in place before this returns, so no later
    /// message can miss it. A dispatcher task sorts what arrives: notifications
    /// to [`HubClient::subscribe`], requests to [`HubClient::serve`], and replies
    /// to the [`Reply`] their request returned.
    pub fn register(&self, sender: &'static str) -> HubClient {
        let inbound = self.inbound.clone();
        let (hub_tx, mut hub_rx) = unbounded_channel();

        tokio::spawn(async move {
            while let Some(msg) = hub_rx.recv().await {
                let _ = inbound.send(msg);
            }
        });

        let (client_tx, mut client_rx) = unbounded_channel();
        self.clients
            .lock()
            .expect("the clients lock is never poisoned")
            .insert(sender, client_tx);

        let pending: Arc<StdMutex<HashMap<u64, oneshot::Sender<Response>>>> =
            Arc::new(StdMutex::new(HashMap::new()));

        let (notify_tx, notify_rx) = unbounded_channel();
        let (request_tx, request_rx) = unbounded_channel();

        {
            let pending = Arc::clone(&pending);
            tokio::spawn(async move {
                while let Some(envelope) = client_rx.recv().await {
                    match &*envelope.message {
                        Message::Notification(notification) => {
                            let _ = notify_tx.send(Arc::clone(notification));
                        }
                        Message::Request(_) => {
                            let _ = request_tx.send(envelope);
                        }
                        Message::Response(response) => {
                            let Some(request_id) = envelope.request_id else {
                                continue;
                            };
                            let waiter = pending
                                .lock()
                                .expect("the pending replies lock is never poisoned")
                                .remove(&request_id);
                            if let Some(waiter) = waiter {
                                let _ = waiter.send(response.clone());
                            }
                        }
                    }
                }
            });
        }

        HubClient {
            hub_tx,
            sender,
            pending,
            notifications: Arc::new(Mutex::new(Some(notify_rx))),
            requests: Arc::new(Mutex::new(Some(request_rx))),
        }
    }

    async fn forward(&self, msg: Envelope) {
        info!(?msg, "Hub message");

        match &*msg.message {
            Message::Request(request) => {
                // Route to the one module that owns it. The owner is a client
                // like any other, so it receives the request on the same channel
                // it would a notification.
                let owner = owner_of(request);
                let delivered = self
                    .clients
                    .lock()
                    .expect("the clients lock is never poisoned")
                    .get(owner)
                    .is_some_and(|tx| tx.send(msg.clone()).is_ok());

                if delivered {
                    // Remember the requester so the reply can find its way back.
                    // Insert before the owner can answer — it can only do so
                    // through this same hub loop, which cannot run until this
                    // call returns.
                    self.requests
                        .lock()
                        .expect("the pending requests lock is never poisoned")
                        .insert(msg.id, msg.sender);
                } else {
                    // No module serves the request: resolve the requester now
                    // with the empty reply rather than leave its `Reply` waiting.
                    self.reply(msg.sender, msg.id, Response::default());
                }
            }
            Message::Response(_) => {
                // A reply rides the hub: look up who asked and route it back.
                let Some(request_id) = msg.request_id else {
                    return;
                };
                let requester = self
                    .requests
                    .lock()
                    .expect("the pending requests lock is never poisoned")
                    .remove(&request_id);
                if let Some(requester) = requester {
                    if let Some(tx) = self
                        .clients
                        .lock()
                        .expect("the clients lock is never poisoned")
                        .get(requester)
                    {
                        let _ = tx.send(msg.clone());
                    }
                }
            }
            Message::Notification(_) => {
                let clients = self
                    .clients
                    .lock()
                    .expect("the clients lock is never poisoned");
                for tx in clients.values() {
                    let _ = tx.send(msg.clone());
                }
            }
        }
    }

    /// Sends `response` to `requester` as the answer to request `request_id`.
    /// Used to resolve a request the hub could not route.
    fn reply(&self, requester: &'static str, request_id: u64, response: Response) {
        let envelope = Envelope {
            id: NEXT_REQUEST_ID.fetch_add(1, Ordering::SeqCst),
            request_id: Some(request_id),
            sender: "hub",
            message: Arc::new(Message::Response(response)),
        };

        if let Some(tx) = self
            .clients
            .lock()
            .expect("the clients lock is never poisoned")
            .get(requester)
        {
            let _ = tx.send(envelope);
        }
    }
}

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}

/// The module that serves a request — the client the hub routes it to.
fn owner_of(request: &Request) -> &'static str {
    match request {
        Request::Navigate(_) => "navigate",
        Request::Index(_) | Request::IndexBatch(_) => "index",
    }
}

/// A client on the hub: one module's whole connection to every other module. It
/// sends notifications and requests, serves the requests it owns, and consumes
/// the notifications the hub broadcasts to it.
#[derive(Clone)]
pub struct HubClient {
    hub_tx: tokio_mpsc::UnboundedSender<Envelope>,
    sender: &'static str,
    /// The requests this client is still awaiting, so a reply routed back can
    /// complete the [`Reply`] its `request` returned.
    pending: Arc<StdMutex<HashMap<u64, oneshot::Sender<Response>>>>,
    /// The notifications the dispatcher forwards; taken by [`Self::subscribe`].
    notifications: Arc<Mutex<Option<UnboundedReceiver<Arc<Notification>>>>>,
    /// The requests routed to this client; taken by [`Self::serve`].
    requests: Arc<Mutex<Option<UnboundedReceiver<Envelope>>>>,
}

impl HubClient {
    /// Sends a notification; every module may consume it.
    pub fn notify(&self, notification: Notification) {
        let envelope = Envelope {
            id: NEXT_REQUEST_ID.fetch_add(1, Ordering::SeqCst),
            request_id: None,
            sender: self.sender,
            message: Arc::new(Message::Notification(Arc::new(notification))),
        };

        let _ = self.hub_tx.send(envelope);
    }

    /// Subscribes to every notification the hub broadcasts, yielding each one's
    /// shared [`Arc`]. Each client subscribes once.
    pub async fn subscribe(&self) -> UnboundedReceiver<Arc<Notification>> {
        self.notifications
            .lock()
            .await
            .take()
            .expect("a client subscribes to notifications once")
    }

    /// Takes the requests routed to this client — the ones its module owns (see
    /// [`owner_of`]) — as whole [`Envelope`]s, so the owner can read the request
    /// and answer it by id with [`Self::respond`]. Each client serves once.
    pub async fn serve(&self) -> UnboundedReceiver<Envelope> {
        self.requests
            .lock()
            .await
            .take()
            .expect("a client serves requests once")
    }

    /// Sends a request for the owning module to answer; the reply arrives on the
    /// returned [`Reply`].
    pub fn request(&self, request: Request) -> Reply {
        let id = NEXT_REQUEST_ID.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = oneshot::channel();
        self.pending
            .lock()
            .expect("the pending replies lock is never poisoned")
            .insert(id, sender);

        let envelope = Envelope {
            id,
            request_id: None,
            sender: self.sender,
            message: Arc::new(Message::Request(request)),
        };

        let _ = self.hub_tx.send(envelope);

        Reply(receiver)
    }

    /// Answers `request_id`; the hub routes the response back to whoever awaits
    /// its [`Reply`].
    pub fn respond(&self, request_id: u64, response: Response) {
        let envelope = Envelope {
            id: NEXT_REQUEST_ID.fetch_add(1, Ordering::SeqCst),
            request_id: Some(request_id),
            sender: self.sender,
            message: Arc::new(Message::Response(response)),
        };

        let _ = self.hub_tx.send(envelope);
    }
}

/// The pending answer to a hub request: a thin wrapper over the request's tokio
/// oneshot receiver. Await it on the runtime; synchronous module code (a module
/// thread, the blocking pool, a sync test) calls [`Reply::blocking_recv`]. If the
/// owner is gone or drops the request unanswered, it resolves to the empty reply
/// (`Response::default()`).
#[must_use = "a request's answer arrives only through its Reply"]
pub struct Reply(oneshot::Receiver<Response>);

impl Reply {
    /// Blocks the current thread for the answer. Panics inside an async context:
    /// await the reply there instead.
    pub fn blocking_recv(self) -> Response {
        self.0.blocking_recv().unwrap_or_default()
    }
}

impl Future for Reply {
    type Output = Response;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Response> {
        Pin::new(&mut self.0)
            .poll(cx)
            .map(Result::unwrap_or_default)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tower_lsp::lsp_types::{Position, Url};

    use crate::messages::{Message, NavigateRequest, NavigateResponse, Notification};

    use super::*;

    fn folder(uri: &str) -> Notification {
        Notification::FoldersDiscoverred {
            uri: vec![Url::parse(uri).expect("a valid file URI")],
        }
    }

    fn declaration_request() -> Request {
        Request::Navigate(NavigateRequest::FindDeclaration {
            uri: Url::parse("file:///Widget.java").expect("a valid file URI"),
            position: Position::new(0, 0),
        })
    }

    #[tokio::test]
    async fn a_notification_is_broadcast_to_every_subscriber() {
        let hub = Hub::new();
        let mut first = hub.register("first").subscribe().await;
        let mut second = hub.register("second").subscribe().await;
        let sender = hub.register("sender");

        sender.notify(folder("file:///workspace"));

        let first_message = first.recv().await.expect("the first subscriber stays open");
        let second_message = second
            .recv()
            .await
            .expect("the second subscriber stays open");

        // Broadcasting shares one allocation: both subscribers hold the same `Arc`.
        assert!(Arc::ptr_eq(&first_message, &second_message));
    }

    #[tokio::test]
    async fn a_dropped_subscriber_does_not_disturb_the_others() {
        let hub = Hub::new();
        let gone = hub.register("gone").subscribe().await;
        let mut live = hub.register("live").subscribe().await;
        drop(gone);

        let sender = hub.register("sender");
        sender.notify(folder("file:///workspace"));

        let message = live.recv().await.expect("the live subscriber stays open");
        assert!(matches!(&*message, Notification::FoldersDiscoverred { .. }));
    }

    #[tokio::test]
    async fn a_request_is_routed_to_its_owner_and_the_reply_rides_back() {
        let hub = Hub::new();
        let requester = hub.register("server");
        let owner = hub.register("navigate");
        let mut requests = owner.serve().await;

        let reply = requester.request(declaration_request());

        // The request reaches the owner, which reads its id and answers it.
        let envelope = requests
            .recv()
            .await
            .expect("the owner receives the request");
        assert!(matches!(&*envelope.message, Message::Request(_)));
        owner.respond(
            envelope.id,
            Response::Navigate(NavigateResponse::References(Vec::new())),
        );

        // The reply rides the hub back to this one requester.
        let response = reply.await;
        assert!(matches!(
            response,
            Response::Navigate(NavigateResponse::References(locations)) if locations.is_empty()
        ));
    }

    #[tokio::test]
    async fn a_reply_is_routed_only_to_its_requester() {
        let hub = Hub::new();
        let requester = hub.register("server");
        let bystander = hub.register("bystander");
        let mut notifications = bystander.subscribe().await;
        let owner = hub.register("navigate");
        let mut requests = owner.serve().await;

        let reply = requester.request(declaration_request());
        let envelope = requests
            .recv()
            .await
            .expect("the owner receives the request");
        owner.respond(
            envelope.id,
            Response::Navigate(NavigateResponse::References(Vec::new())),
        );
        let _ = reply.await;

        // The bystander's subscription never sees the request or the reply.
        assert!(notifications.try_recv().is_err());
    }

    #[tokio::test]
    async fn an_unserved_request_resolves_to_the_empty_reply() {
        let hub = Hub::new();
        // No client serves navigation, so the request has no owner and no reply
        // ever comes; it must not leave the requester waiting forever.
        let requester = hub.register("server");
        let reply = requester.request(declaration_request());
        let response = reply.await;
        assert!(matches!(response, Response::Empty));
    }
}
