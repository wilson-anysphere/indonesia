use lsp_types::NumberOrString;
use nova_core::RequestId;
use nova_scheduler::{CancellationToken, Scheduler};

#[derive(Clone)]
pub struct RequestCancellation {
    scheduler: Scheduler,
}

impl RequestCancellation {
    pub fn new(scheduler: Scheduler) -> Self {
        Self { scheduler }
    }

    pub fn register(&self, id: NumberOrString) -> CancellationToken {
        self.scheduler.register_request(request_id_from_lsp(id))
    }

    pub fn cancel(&self, id: NumberOrString) -> bool {
        let request_id = request_id_from_lsp(id);
        self.scheduler.cancel_request(&request_id)
    }

    pub fn finish(&self, id: NumberOrString) {
        let request_id = request_id_from_lsp(id);
        self.scheduler.finish_request(&request_id);
    }
}

fn request_id_from_lsp(value: NumberOrString) -> RequestId {
    match value {
        NumberOrString::Number(num) => RequestId::Number(num as i64),
        NumberOrString::String(s) => RequestId::String(s.into_boxed_str()),
    }
}
