use crate::rpc_out::RpcOut;

use crossbeam_channel::{Receiver, Sender};
use lsp_server::{Message, Notification, Request, RequestId, Response};
use nova_db::SalsaDatabase;
use tokio_util::sync::CancellationToken;

use std::time::Instant;

#[derive(Clone)]
pub(super) struct LspClient {
    sender: Sender<Message>,
}

impl LspClient {
    pub(super) fn new(sender: Sender<Message>) -> Self {
        Self { sender }
    }

    fn send(&self, message: Message) -> std::io::Result<()> {
        self.sender
            .send(message)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "LSP channel closed"))
    }

    pub(super) fn respond(&self, response: Response) -> std::io::Result<()> {
        self.send(Message::Response(response))
    }

    fn notify(&self, method: impl Into<String>, params: serde_json::Value) -> std::io::Result<()> {
        self.send(Message::Notification(Notification {
            method: method.into(),
            params,
        }))
    }

    fn request(
        &self,
        id: RequestId,
        method: impl Into<String>,
        params: serde_json::Value,
    ) -> std::io::Result<()> {
        self.send(Message::Request(Request {
            id,
            method: method.into(),
            params,
        }))
    }
}

impl RpcOut for LspClient {
    fn send_notification(&self, method: &str, params: serde_json::Value) -> std::io::Result<()> {
        self.notify(method.to_string(), params)
    }

    fn send_request(
        &self,
        id: RequestId,
        method: &str,
        params: serde_json::Value,
    ) -> std::io::Result<()> {
        self.request(id, method.to_string(), params)
    }
}

pub(super) enum IncomingMessage {
    Request {
        request: Request,
        cancel_id: lsp_types::NumberOrString,
        cancel_token: CancellationToken,
    },
    Notification(Notification),
    Response(Response),
}

pub(super) fn message_router(
    receiver: Receiver<Message>,
    sender: Sender<IncomingMessage>,
    request_cancellation: nova_lsp::RequestCancellation,
    salsa: Option<SalsaDatabase>,
) {
    let metrics = nova_metrics::MetricsRegistry::global();

    for message in receiver {
        match message {
            Message::Notification(notification) if notification.method == "$/cancelRequest" => {
                let start = Instant::now();
                if let Ok(params) =
                    serde_json::from_value::<lsp_types::CancelParams>(notification.params)
                {
                    let cancelled = request_cancellation.cancel(params.id.clone());
                    if cancelled {
                        if let Some(salsa) = salsa.as_ref() {
                            // Best-effort and non-panicking: cancellation is advisory and should
                            // never crash the router thread.
                            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                salsa.request_cancellation();
                            }));
                        }
                    }
                }
                metrics.record_request("$/cancelRequest", start.elapsed());
            }
            Message::Request(request) => {
                let cancel_id = cancel_id_from_request_id(&request.id);
                let cancel_token = request_cancellation.register(cancel_id.clone());
                if sender
                    .send(IncomingMessage::Request {
                        request,
                        cancel_id,
                        cancel_token,
                    })
                    .is_err()
                {
                    break;
                }
            }
            Message::Notification(notification) => {
                if sender
                    .send(IncomingMessage::Notification(notification))
                    .is_err()
                {
                    break;
                }
            }
            Message::Response(response) => {
                if sender.send(IncomingMessage::Response(response)).is_err() {
                    break;
                }
            }
        }
    }
}

fn cancel_id_from_request_id(id: &RequestId) -> lsp_types::NumberOrString {
    serde_json::to_value(id)
        .ok()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_else(|| lsp_types::NumberOrString::String("<invalid-request-id>".to_string()))
}
