use std::time::Duration;

use nova_core::RequestId;
use nova_scheduler::{Cancelled, KeyedDebouncer, PoolKind, ProgressEvent, Scheduler};
use tokio::sync::mpsc;

#[tokio::test]
async fn debouncer_coalesces_and_cancels_previous_job() {
    let scheduler = Scheduler::default();
    let debouncer = KeyedDebouncer::new(
        scheduler.clone(),
        PoolKind::Compute,
        Duration::from_millis(15),
    );

    let (tx, mut rx) = mpsc::unbounded_channel::<&'static str>();
    let tx_first = tx.clone();

    let first = debouncer.debounce("file:///test.java", move |_token| {
        let _ = tx_first.send("first");
        Ok(())
    });

    let tx_second = tx.clone();
    let second = debouncer.debounce("file:///test.java", move |_token| {
        let _ = tx_second.send("second");
        Ok(())
    });

    assert!(first.is_cancelled());
    assert!(!second.is_cancelled());

    let value = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("timeout waiting for debounced task")
        .expect("channel closed");
    assert_eq!(value, "second");

    let no_more = tokio::time::timeout(Duration::from_millis(30), rx.recv()).await;
    assert!(no_more.is_err(), "unexpected extra debounced job executed");
}

#[tokio::test]
async fn scheduler_request_cancellation_cancels_blocking_task() {
    let scheduler = Scheduler::default();
    let request_id: RequestId = 1_i64.into();
    let token = scheduler.register_request(request_id.clone());

    let task = scheduler.spawn_compute_with_token(token, |token| {
        while !token.is_cancelled() {
            std::thread::yield_now();
        }
        Err::<(), _>(Cancelled)
    });

    scheduler.cancel_request(&request_id);

    let result = tokio::time::timeout(Duration::from_secs(1), task.join())
        .await
        .expect("timeout waiting for cancelled task");
    assert_eq!(result, Err(Cancelled));

    scheduler.finish_request(&request_id);
}

#[tokio::test]
async fn scheduler_request_cancellation_cancels_async_task() {
    let scheduler = Scheduler::default();
    let request_id: RequestId = 2_i64.into();
    let token = scheduler.register_request(request_id.clone());

    let task = scheduler.spawn_io_with_token(token, |token| async move {
        token.cancelled().await;
        Err::<(), _>(Cancelled)
    });

    scheduler.cancel_request(&request_id);

    let result = tokio::time::timeout(Duration::from_secs(1), task.join())
        .await
        .expect("timeout waiting for cancelled async task");
    assert_eq!(result, Err(Cancelled));

    scheduler.finish_request(&request_id);
}

#[tokio::test]
async fn progress_events_are_ordered() {
    let scheduler = Scheduler::default();
    let mut rx = scheduler.subscribe_progress();

    let progress = scheduler.progress().start("indexing");
    progress.report(Some("halfway".to_string()), Some(50));
    progress.finish(Some("done".to_string()));
    drop(progress);

    let begin = rx.recv().await.expect("expected begin event");
    let report = rx.recv().await.expect("expected report event");
    let end = rx.recv().await.expect("expected end event");

    let ProgressEvent::Begin { id, .. } = begin else {
        panic!("expected begin event, got {begin:?}");
    };

    match report {
        ProgressEvent::Report { id: report_id, .. } => assert_eq!(report_id, id),
        other => panic!("expected report event, got {other:?}"),
    }

    match end {
        ProgressEvent::End { id: end_id, .. } => assert_eq!(end_id, id),
        other => panic!("expected end event, got {other:?}"),
    }

    let no_more = tokio::time::timeout(Duration::from_millis(25), rx.recv()).await;
    assert!(no_more.is_err(), "progress emitted unexpected extra events");
}
