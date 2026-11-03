use super::native_io::{native_io_on, NATIVE_IO_LIMIT};
use super::*;
use std::future::Future;
use std::sync::mpsc;
use std::time::Duration;
use tokio::sync::{oneshot, Semaphore};

// Isolated lanes exercise the production helper without competing with other
// storage fixtures. Every native wait has an escape if the async test fails.
fn held_call(
    slots: Arc<Semaphore>,
) -> (
    JoinHandle<Result<()>>,
    mpsc::Sender<()>,
    oneshot::Receiver<()>,
) {
    let (release, wait) = mpsc::channel();
    let (entered, ready) = oneshot::channel();
    let task = tokio::spawn(native_io_on(slots, move || {
        let _ = entered.send(());
        wait.recv_timeout(Duration::from_secs(10))
            .context("release native fixture")?;
        Ok(())
    }));
    (task, release, ready)
}

async fn entered(ready: oneshot::Receiver<()>) {
    tokio::time::timeout(Duration::from_secs(5), ready)
        .await
        .expect("native closure scheduled")
        .expect("native closure entered");
}

#[tokio::test(flavor = "current_thread")]
async fn native_lane_keeps_two_permits_until_cancelled_calls_really_finish() {
    let slots = Arc::new(Semaphore::new(NATIVE_IO_LIMIT));
    assert_eq!(NATIVE_IO_LIMIT, 2);
    let (first, release_first, first_ready) = held_call(slots.clone());
    let (second, release_second, second_ready) = held_call(slots.clone());
    entered(first_ready).await;
    entered(second_ready).await;
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    assert_eq!(slots.available_permits(), 0);
    assert!(slots.clone().try_acquire_owned().is_err());
    let (third, release_third, mut third_ready) = held_call(slots.clone());
    // Poll the actual acquisition to Pending, rather than assuming task order.
    {
        let probe = slots.clone().acquire_owned();
        tokio::pin!(probe);
        assert!(
            std::future::poll_fn(|cx| {
                std::task::Poll::Ready(probe.as_mut().poll(cx).is_pending())
            })
            .await
        );
        assert!(matches!(
            third_ready.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
    }
    release_first.send(()).unwrap();
    entered(third_ready).await;
    assert_eq!(slots.available_permits(), 0);
    release_second.send(()).unwrap();
    release_third.send(()).unwrap();
    second.await.unwrap().unwrap();
    third.await.unwrap().unwrap();
    assert_eq!(slots.available_permits(), NATIVE_IO_LIMIT);
}

#[tokio::test(flavor = "current_thread")]
async fn current_thread_heartbeat_runs_while_native_call_is_held() {
    let slots = Arc::new(Semaphore::new(NATIVE_IO_LIMIT));
    let (task, release, ready) = held_call(slots);
    entered(ready).await;
    let heartbeat = tokio::spawn(async {
        tokio::task::yield_now().await;
        42
    });
    let observed = tokio::time::timeout(Duration::from_secs(2), heartbeat).await;
    let native_still_held = !task.is_finished();
    // Cleanup before assertions, including on heartbeat failure.
    release.send(()).unwrap();
    task.await.unwrap().unwrap();
    assert!(
        native_still_held,
        "heartbeat must arrive before native completion"
    );
    assert_eq!(observed.expect("executor heartbeat").unwrap(), 42);
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_before_admission_does_not_run_native_work() {
    let slots = Arc::new(Semaphore::new(NATIVE_IO_LIMIT));
    let all = slots.clone().acquire_many_owned(2).await.unwrap();
    let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observed = ran.clone();
    let future = native_io_on(slots.clone(), move || {
        observed.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    });
    {
        tokio::pin!(future);
        assert!(
            std::future::poll_fn(|cx| {
                std::task::Poll::Ready(future.as_mut().poll(cx).is_pending())
            })
            .await
        );
    }
    drop(all);
    assert_eq!(slots.available_permits(), NATIVE_IO_LIMIT);
    assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
}
