use crate::ServerState;

use std::time::Duration;

impl ServerState {
    pub(super) fn next_outgoing_id(&mut self) -> String {
        let id = self.next_outgoing_request_id;
        self.next_outgoing_request_id = self.next_outgoing_request_id.saturating_add(1);
        format!("nova:{id}")
    }
}

pub(super) fn join_io_threads_with_timeout(io_threads: lsp_server::IoThreads, timeout: Duration) {
    use std::sync::mpsc;

    let (done_tx, done_rx) = mpsc::channel::<std::io::Result<()>>();
    std::thread::spawn(move || {
        let res = io_threads.join();
        let _ = done_tx.send(res);
    });

    match done_rx.recv_timeout(timeout) {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            // Preserve process-exit semantics: we are already shutting down; don't fail the exit
            // path on an I/O join error.
        }
        Err(_) => {
            // Timeout or disconnect: fall back to `process::exit` below.
        }
    }
}

