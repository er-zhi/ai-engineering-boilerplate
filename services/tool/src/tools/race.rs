// race: run several independent "ask an open API the same question" sources concurrently and
// take the first success. Structured-data tools (weather, fx_rate, stock_quote) each talk to two
// keyless public APIs that are individually flaky — racing them means one being slow, rate-limited
// or down costs latency only up to the per-source timeout, not the whole answer. The whole race is
// retried once because these free endpoints fail transiently far more often than permanently.
//
// Sources are `Fn() -> Future` factories rather than plain futures precisely so the retry can
// build a fresh set: a future that has already resolved (or been cancelled at its timeout) cannot
// be polled again.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use futures::StreamExt;
use futures::stream::FuturesUnordered;

pub const PER_SOURCE_TIMEOUT: Duration = Duration::from_secs(6);
pub const RETRY_DELAY: Duration = Duration::from_millis(500);

/// One racer: a name for the error message plus a factory that starts a fresh attempt.
pub type SourceFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + Sync + 'a>>;
pub type SourceFn<'a, T> = Box<dyn Fn() -> SourceFuture<'a, T> + Send + Sync + 'a>;

/// Boxes a source future factory into a [`SourceFn`], so a caller can put closures of different
/// concrete types in one `Vec`.
pub fn source<'a, T, F, Fut>(name: &'static str, f: F) -> (&'static str, SourceFn<'a, T>)
where
    F: Fn() -> Fut + Send + Sync + 'a,
    Fut: Future<Output = Result<T, String>> + Send + Sync + 'a,
{
    (name, Box::new(move || Box::pin(f())))
}

/// Runs every source concurrently, each capped at [`PER_SOURCE_TIMEOUT`], and returns the first
/// `Ok` — the remaining futures are dropped (cancelled) as soon as that happens. If every source
/// fails the whole race runs once more after [`RETRY_DELAY`]; only then does it give up, with an
/// error naming what each source said on the final attempt.
pub async fn first_success<'a, T>(
    sources: &[(&'static str, SourceFn<'a, T>)],
) -> Result<T, String> {
    if sources.is_empty() {
        return Err("no sources configured".to_owned());
    }
    match one_race(sources).await {
        Ok(value) => return Ok(value),
        Err(_) => tokio::time::sleep(RETRY_DELAY).await,
    }
    one_race(sources)
        .await
        .map_err(|errors| format!("all sources failed: {}", errors.join("; ")))
}

/// One pass over the sources. `Err` carries one `"name: reason"` string per source.
async fn one_race<'a, T>(sources: &[(&'static str, SourceFn<'a, T>)]) -> Result<T, Vec<String>> {
    let mut running = FuturesUnordered::new();
    for (name, factory) in sources {
        running.push(attempt(name, factory()));
    }
    let mut errors = Vec::new();
    while let Some(outcome) = running.next().await {
        match outcome {
            Ok(value) => return Ok(value),
            Err(error) => errors.push(error),
        }
    }
    Err(errors)
}

async fn attempt<T>(
    name: &'static str,
    future: impl Future<Output = Result<T, String>>,
) -> Result<T, String> {
    match tokio::time::timeout(PER_SOURCE_TIMEOUT, future).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(format!("{name}: {error}")),
        Err(_) => Err(format!(
            "{name}: timed out after {}s",
            PER_SOURCE_TIMEOUT.as_secs()
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[tokio::test]
    async fn the_first_success_wins_and_the_slow_source_is_cancelled() {
        let slow_finished = Arc::new(AtomicUsize::new(0));
        let flag = Arc::clone(&slow_finished);
        let sources = vec![
            source("slow", move || {
                let flag = Arc::clone(&flag);
                async move {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    flag.fetch_add(1, Ordering::SeqCst);
                    Ok("slow")
                }
            }),
            source("fast", || async { Ok("fast") }),
        ];

        let winner = first_success(&sources).await.expect("a source succeeded");

        assert_eq!(winner, "fast");
        assert_eq!(
            slow_finished.load(Ordering::SeqCst),
            0,
            "the losing future should have been dropped, not awaited to completion"
        );
    }

    #[tokio::test]
    async fn a_failing_source_does_not_stop_a_later_succeeding_one() {
        let sources = vec![
            source("broken", || async { Err::<&str, _>("503".to_owned()) }),
            source("working", || async {
                tokio::time::sleep(Duration::from_millis(20)).await;
                Ok("value")
            }),
        ];

        assert_eq!(
            first_success(&sources).await.expect("second source"),
            "value"
        );
    }

    #[tokio::test]
    async fn all_sources_failing_retries_the_whole_race_once_then_reports_every_source() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let a = Arc::clone(&attempts);
        let b = Arc::clone(&attempts);
        let sources = vec![
            source("alpha", move || {
                let a = Arc::clone(&a);
                async move {
                    a.fetch_add(1, Ordering::SeqCst);
                    Err::<(), _>("connection refused".to_owned())
                }
            }),
            source("beta", move || {
                let b = Arc::clone(&b);
                async move {
                    b.fetch_add(1, Ordering::SeqCst);
                    Err::<(), _>("bad json".to_owned())
                }
            }),
        ];

        let error = first_success(&sources).await.unwrap_err();

        assert_eq!(
            attempts.load(Ordering::SeqCst),
            4,
            "two sources, raced twice"
        );
        assert!(error.contains("alpha: connection refused"), "{error}");
        assert!(error.contains("beta: bad json"), "{error}");
    }

    #[tokio::test]
    async fn a_source_that_succeeds_only_on_the_retry_still_wins() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let sources = vec![source("flaky", move || {
            let counter = Arc::clone(&counter);
            async move {
                if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err("transient".to_owned())
                } else {
                    Ok("second time lucky")
                }
            }
        })];

        assert_eq!(
            first_success(&sources).await.expect("retry"),
            "second time lucky"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_source_slower_than_the_per_source_timeout_is_reported_as_a_timeout() {
        let sources = vec![source("stuck", || async {
            tokio::time::sleep(PER_SOURCE_TIMEOUT + Duration::from_secs(5)).await;
            Ok::<&str, String>("never")
        })];

        // Virtual time: tokio auto-advances the clock while every task is parked on a sleep, so
        // this exercises the real 6s timeout without spending 6s of wall clock.
        let error = first_success(&sources).await.unwrap_err();

        assert!(error.contains("stuck: timed out"), "{error}");
    }
}
