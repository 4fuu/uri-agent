use anyhow::{Result, bail};
use std::time::{Duration, Instant};
use tokio::sync::watch;

const MINIMUM_INTERVAL: Duration = Duration::from_secs(1);
const DEFAULT_INTERVAL: Duration = Duration::from_secs(5);
const SLOW_DOWN_INCREMENT: Duration = Duration::from_secs(5);

pub enum Poll<T> {
    Complete(T),
    Pending,
    SlowDown { interval: Option<Duration> },
    Failed(String),
}

pub async fn poll<T, F, Fut>(
    interval: Option<Duration>,
    expires: Option<Duration>,
    wait_before_first: bool,
    mut cancel_rx: watch::Receiver<bool>,
    mut poll: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Poll<T>>>,
{
    let deadline = expires.map(|expires| Instant::now() + expires);
    let mut interval = interval
        .filter(|value| *value > Duration::ZERO)
        .unwrap_or(DEFAULT_INTERVAL)
        .max(MINIMUM_INTERVAL);
    let mut slow_downs = 0_u32;
    if wait_before_first {
        sleep_cancel(interval, deadline, &mut cancel_rx).await?;
    }
    loop {
        if *cancel_rx.borrow() {
            bail!("OAuth login was cancelled");
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            break;
        }
        match poll().await? {
            Poll::Complete(value) => return Ok(value),
            Poll::Failed(message) => bail!("{message}"),
            Poll::Pending => {}
            Poll::SlowDown { interval: next } => {
                slow_downs += 1;
                interval = next
                    .filter(|value| *value > Duration::ZERO)
                    .unwrap_or(interval + SLOW_DOWN_INCREMENT)
                    .max(MINIMUM_INTERVAL);
            }
        }
        sleep_cancel(interval, deadline, &mut cancel_rx).await?;
    }
    if slow_downs > 0 {
        bail!(
            "Device flow timed out after one or more slow_down responses. This is often caused by clock drift in WSL or VM environments."
        );
    }
    bail!("Device flow timed out")
}

async fn sleep_cancel(
    interval: Duration,
    deadline: Option<Instant>,
    cancel_rx: &mut watch::Receiver<bool>,
) -> Result<()> {
    let wait = deadline.map_or(interval, |deadline| {
        deadline
            .saturating_duration_since(Instant::now())
            .min(interval)
    });
    if wait.is_zero() {
        return Ok(());
    }
    tokio::select! {
        () = tokio::time::sleep(wait) => Ok(()),
        changed = cancel_rx.changed() => {
            if changed.is_err() || *cancel_rx.borrow() {
                bail!("OAuth login was cancelled");
            }
            Ok(())
        }
    }
}
