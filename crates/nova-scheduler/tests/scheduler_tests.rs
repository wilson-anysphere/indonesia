use std::time::Duration;

use nova_scheduler::{Cancelled, KeyedDebouncer, PoolKind, ProgressEvent, Scheduler, TaskError};
use tokio::sync::mpsc;

fn test_scheduler() -> Scheduler {
    nova_scheduler::Scheduler::new_with_io_handle(
        nova_scheduler::SchedulerConfig {
            compute_threads: 1,
            background_threads: 1,
            io_threads: 1,
            progress_channel_capacity: 16,
        },
        tokio::runtime::Handle::current(),
    )
}

#[tokio::test(flavor = "current_thread")]
async fn debouncer_coalesces_and_cancels_previous_job() {
    let scheduler = test_scheduler();
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

#[tokio::test(flavor = "current_thread")]
async fn blocking_task_join_unblocks_on_cancellation_even_if_worker_does_not_cooperate() {
    let scheduler = test_scheduler();
    let token = nova_scheduler::CancellationToken::new();

    let task = scheduler.spawn_compute_with_token(token.clone(), |_token| {
        std::thread::sleep(Duration::from_millis(200));
        Ok::<_, Cancelled>(())
    });

    token.cancel();

    let result = tokio::time::timeout(Duration::from_millis(100), task.join())
        .await
        .expect("timeout waiting for join to observe cancellation");
    assert_eq!(result, Err(TaskError::Cancelled));
}

#[tokio::test(flavor = "current_thread")]
async fn async_task_join_unblocks_on_cancellation_even_if_worker_does_not_cooperate() {
    let scheduler = test_scheduler();
    let token = nova_scheduler::CancellationToken::new();

    let task = scheduler.spawn_io_with_token(token.clone(), |_token| async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        Ok::<_, Cancelled>(())
    });

    token.cancel();

    let result = tokio::time::timeout(Duration::from_millis(100), task.join())
        .await
        .expect("timeout waiting for join to observe cancellation");
    assert_eq!(result, Err(TaskError::Cancelled));
}

#[tokio::test(flavor = "current_thread")]
async fn scheduler_request_cancellation_cancels_blocking_task() {
    let scheduler = test_scheduler();
    let token = nova_scheduler::CancellationToken::new();

    let task = scheduler.spawn_compute_with_token(token.clone(), |token| {
        while !token.is_cancelled() {
            std::thread::yield_now();
        }
        Err::<(), _>(Cancelled)
    });

    token.cancel();

    let result = tokio::time::timeout(Duration::from_secs(1), task.join())
        .await
        .expect("timeout waiting for cancelled task");
    assert_eq!(result, Err(TaskError::Cancelled));
}

#[tokio::test(flavor = "current_thread")]
async fn scheduler_request_cancellation_cancels_async_task() {
    let scheduler = test_scheduler();
    let token = nova_scheduler::CancellationToken::new();

    let task = scheduler.spawn_io_with_token(token.clone(), |token| async move {
        token.cancelled().await;
        Err::<(), _>(Cancelled)
    });

    token.cancel();

    let result = tokio::time::timeout(Duration::from_secs(1), task.join())
        .await
        .expect("timeout waiting for cancelled async task");
    assert_eq!(result, Err(TaskError::Cancelled));
}

#[tokio::test(flavor = "current_thread")]
async fn request_context_deadline_cancels_tasks() {
    let scheduler = test_scheduler();
    let ctx = scheduler
        .request_context("deadline-test")
        .with_timeout(Duration::from_millis(50));

    let task = scheduler.spawn_background_ctx(&ctx, |ctx| {
        while !ctx.token().is_cancelled() {
            std::thread::sleep(Duration::from_millis(5));
        }
        Err::<(), _>(Cancelled)
    });

    let result = tokio::time::timeout(Duration::from_secs(1), task.join())
        .await
        .expect("timeout waiting for deadline cancellation");
    assert_eq!(result, Err(TaskError::Cancelled));
}

#[tokio::test(flavor = "current_thread")]
async fn progress_events_are_ordered() {
    let scheduler = test_scheduler();
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
