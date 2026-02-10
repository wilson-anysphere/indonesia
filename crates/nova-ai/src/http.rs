use crate::AiError;

pub(crate) mod sse;

pub(crate) fn map_reqwest_error(err: reqwest::Error) -> AiError {
    if err.is_timeout() {
        AiError::Timeout
    } else {
        AiError::from(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::service::{make_service_fn, service_fn};
    use hyper::{Body, Response, Server};
    use std::convert::Infallible;
    use std::net::TcpListener;
    use std::time::Duration;
    use tokio::sync::oneshot;

    #[tokio::test(flavor = "current_thread")]
    async fn map_reqwest_error_converts_timeout_errors_into_ai_timeout() {
        // Build a local HTTP server that intentionally delays its response long
        // enough to trigger reqwest's own timeout path (reqwest::Error::is_timeout()).
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("listener addr");
        listener
            .set_nonblocking(true)
            .expect("set listener nonblocking");

        let make_svc = make_service_fn(|_conn| async {
            Ok::<_, Infallible>(service_fn(|_req| async {
                tokio::time::sleep(Duration::from_millis(200)).await;
                Ok::<_, Infallible>(Response::new(Body::from("ok")))
            }))
        });

        let server = Server::from_tcp(listener)
            .expect("server from_tcp")
            .serve(make_svc);

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server_handle = tokio::spawn(server.with_graceful_shutdown(async {
            let _ = shutdown_rx.await;
        }));

        let client = reqwest::Client::new();
        let url = format!("http://{addr}/");

        // Ensure `From<reqwest::Error>` is timeout-aware so `?` conversions don't misclassify
        // reqwest timeouts as `AiError::Http`.
        let err = client
            .get(&url)
            .timeout(Duration::from_millis(50))
            .send()
            .await
            .expect_err("expected request to time out");

        assert!(
            err.is_timeout(),
            "expected a reqwest timeout error; got {err:?}"
        );
        assert!(matches!(AiError::from(err), AiError::Timeout));

        // Also keep explicit mapping helper behavior stable.
        let err = client
            .get(&url)
            .timeout(Duration::from_millis(50))
            .send()
            .await
            .expect_err("expected request to time out");

        assert!(
            err.is_timeout(),
            "expected a reqwest timeout error; got {err:?}"
        );
        assert!(matches!(map_reqwest_error(err), AiError::Timeout));

        let _ = shutdown_tx.send(());
        let _ = server_handle.await;
    }
}
