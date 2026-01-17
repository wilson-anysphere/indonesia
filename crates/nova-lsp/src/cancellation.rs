use lsp_types::NumberOrString;
use nova_core::RequestId;
use nova_scheduler::{CancellationToken, ProgressSender, RequestContext, Scheduler};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct RequestCancellation {
    progress: ProgressSender,
    tokens: Arc<Mutex<HashMap<RequestId, CancellationToken>>>,
}

impl RequestCancellation {
    pub fn new(scheduler: Scheduler) -> Self {
        Self {
            progress: scheduler.progress(),
            tokens: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn register(&self, id: NumberOrString) -> CancellationToken {
        let request_id = request_id_from_lsp(id);
        let token = CancellationToken::new();
        crate::poison::lock(&self.tokens, "RequestCancellation::register")
            .insert(request_id, token.clone());
        token
    }

    pub fn register_context(&self, id: NumberOrString) -> RequestContext {
        let request_id = request_id_from_lsp(id);
        let token = CancellationToken::new();
        crate::poison::lock(&self.tokens, "RequestCancellation::register_context")
            .insert(request_id.clone(), token.clone());
        RequestContext::new(request_id, token, None, self.progress.clone())
    }

    pub fn cancel(&self, id: NumberOrString) -> bool {
        let request_id = request_id_from_lsp(id);
        let guard = crate::poison::lock(&self.tokens, "RequestCancellation::cancel");
        let Some(token) = guard.get(&request_id) else {
            return false;
        };
        token.cancel();
        true
    }

    pub fn finish(&self, id: NumberOrString) {
        let request_id = request_id_from_lsp(id);
        crate::poison::lock(&self.tokens, "RequestCancellation::finish").remove(&request_id);
    }
}

fn request_id_from_lsp(value: NumberOrString) -> RequestId {
    match value {
        NumberOrString::Number(num) => RequestId::Number(num as i64),
        NumberOrString::String(s) => RequestId::String(s.into_boxed_str()),
    }
}
