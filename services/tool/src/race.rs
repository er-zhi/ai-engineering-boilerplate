// race: start a fan-out of independent operations, keep the first `take` successes and cancel the
// rest where they stand. It knows nothing about HTTP, URLs or any subject — the operations are the
// caller's, and the only thing here is the shape of "several ways to get the same thing".
//
// Operations are `Fn() -> Future` factories rather than plain futures on purpose: a future that has
// already resolved — or been cancelled at its timeout — cannot be polled again, so a reserve start
// needs a fresh one. Boxing them also lets closures of different concrete types share one slice.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use futures::StreamExt;
use futures::stream::FuturesUnordered;

pub type OpFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;
pub type OpFn<'a, T> = Box<dyn Fn() -> OpFuture<'a, T> + Send + Sync + 'a>;

/// Boxes an operation factory so callers can put differently-typed closures in one slice.
pub fn op<'a, T, F, Fut>(name: &'a str, f: F) -> (&'a str, OpFn<'a, T>)
where
    F: Fn() -> Fut + Send + Sync + 'a,
    Fut: Future<Output = Result<T, String>> + Send + 'a,
{
    (name, Box::new(move || Box::pin(f())))
}

/// What a race produced. Not a `Result`: a partial answer is a normal outcome, and only the caller
/// knows whether fewer than `take` successes is enough for what it is doing.
#[derive(Debug)]
pub struct Outcome<'a, T> {
    /// Up to `take` successes, in the order they arrived, each with the name of the operation that
    /// produced it.
    pub taken: Vec<(&'a str, T)>,
    /// One `"name: reason"` per operation that was started and did not succeed. Operations still in
    /// flight when the quorum was reached are cancelled and appear nowhere: they have no verdict.
    pub failures: Vec<String>,
}

/// Runs at most `fan_out` operations at a time, each capped at `per_op_timeout`, until `take` of
/// them have succeeded or the list is exhausted. A finished operation immediately makes room for the
/// next one in the slice, so the width stays full. Reaching the quorum drops the whole set of
/// in-flight futures, which cancels them where they stand rather than waiting them out.
///
/// `'o` (the borrow of the slice) and `'a` (the lifetime the names and values ride) are kept as two
/// separate parameters rather than one. `OpFn` is a boxed trait object, so dropping it is opaque to
/// the compiler — dropck conservatively assumes that drop could still touch `'a`-borrowed data, and
/// demands `'a` strictly outlive the point `ops` itself is dropped. Naming the slice's own borrow
/// `'a` too would make that borrow responsible for satisfying its own strict-outlives demand, which
/// is only possible at `'static`. Splitting the two lets `'o` end where the caller's borrow of `ops`
/// ends while `'a` — the names' and values' real origin — stays exactly as long as it always was.
pub async fn race<'o, 'a: 'o, T>(
    ops: &'o [(&'a str, OpFn<'a, T>)],
    fan_out: usize,
    take: usize,
    per_op_timeout: Duration,
) -> Outcome<'a, T> {
    if ops.is_empty() {
        return Outcome {
            taken: Vec::new(),
            failures: vec!["no operations to race".to_owned()],
        };
    }
    let width = fan_out.max(1);
    let quorum = take.max(1);
    let mut next = 0;
    let mut running = FuturesUnordered::new();
    let mut outcome = Outcome {
        taken: Vec::new(),
        failures: Vec::new(),
    };

    while next < ops.len() && running.len() < width {
        running.push(attempt(&ops[next], per_op_timeout));
        next += 1;
    }
    while let Some(result) = running.next().await {
        match result {
            Ok(taken) => {
                outcome.taken.push(taken);
                if outcome.taken.len() >= quorum {
                    break;
                }
            }
            Err(failure) => outcome.failures.push(failure),
        }
        if next < ops.len() {
            running.push(attempt(&ops[next], per_op_timeout));
            next += 1;
        }
    }
    outcome
}

async fn attempt<'o, 'a: 'o, T>(
    (name, factory): &'o (&'a str, OpFn<'a, T>),
    per_op_timeout: Duration,
) -> Result<(&'a str, T), String> {
    match tokio::time::timeout(per_op_timeout, factory()).await {
        Ok(Ok(value)) => Ok((name, value)),
        Ok(Err(reason)) => Err(format!("{name}: {reason}")),
        // `{:?}` rather than `as_secs()`: a 50 ms cap printed as "0s" reads like a bug report
        // about the wrong thing.
        Err(_) => Err(format!("{name}: timed out after {per_op_timeout:?}")),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    const QUICK: Duration = Duration::from_secs(5);

    /// Counts cancellations only. `resolved` is set on the success path, so a future that ran to
    /// completion is not confused with one the race cut loose — without that flag the counter would
    /// tick either way and the test would prove nothing.
    struct Tracked {
        cancelled: Arc<AtomicUsize>,
        resolved: bool,
    }

    impl Drop for Tracked {
        fn drop(&mut self) {
            if !self.resolved {
                self.cancelled.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    /// Every race in these tests is wrapped in this: a test that asserts the race *stops early*
    /// must fail when it does not, rather than pass slowly after the slow operation finishes.
    const MUST_FINISH_WITHIN: Duration = Duration::from_millis(500);

    fn answering<'a>(name: &'a str, after: Duration) -> (&'a str, OpFn<'a, &'a str>) {
        op(name, move || async move {
            tokio::time::sleep(after).await;
            Ok(name)
        })
    }

    fn failing<'a>(name: &'a str) -> (&'a str, OpFn<'a, &'a str>) {
        op(name, move || async move { Err("refused".to_owned()) })
    }

    #[tokio::test]
    async fn takes_the_quorum_and_stops_without_waiting_for_the_rest() {
        let ops = [
            answering("a", Duration::from_millis(5)),
            answering("b", Duration::from_millis(10)),
            answering("c", Duration::from_secs(30)),
        ];
        let outcome = tokio::time::timeout(MUST_FINISH_WITHIN, race(&ops, 3, 2, QUICK))
            .await
            .expect("the quorum was reached, so the race must not wait for c");
        assert_eq!(
            outcome
                .taken
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[tokio::test]
    // `resolved` is only ever read by `Drop::drop`, which rustc's liveness check for
    // `unused_assignments`/`unused_variables` does not look inside: it sees a capture that is
    // written once and, within this function's own body, never read back.
    #[allow(unused_assignments, unused_variables)]
    async fn the_losers_are_cancelled_rather_than_awaited() {
        let cancelled = Arc::new(AtomicUsize::new(0));
        let slow = cancelled.clone();
        let ops = [
            answering("fast", Duration::from_millis(1)),
            op("slow", move || {
                let mut guard = Tracked {
                    cancelled: slow.clone(),
                    resolved: false,
                };
                async move {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    guard.resolved = true;
                    Ok("slow")
                }
            }),
        ];
        let outcome = tokio::time::timeout(MUST_FINISH_WITHIN, race(&ops, 2, 1, QUICK))
            .await
            .expect("the race must return on the first success");
        assert_eq!(outcome.taken.len(), 1);
        assert_eq!(
            cancelled.load(Ordering::SeqCst),
            1,
            "the slow op resolved or was awaited instead of being dropped"
        );
    }

    #[tokio::test]
    async fn a_failure_starts_the_next_reserve() {
        let ops = [
            failing("one"),
            failing("two"),
            failing("three"),
            answering("four", Duration::from_millis(1)),
            answering("five", Duration::from_millis(1)),
        ];
        let outcome = race(&ops, 3, 1, QUICK).await;
        assert_eq!(outcome.taken.len(), 1, "the reserve never started");
        assert_eq!(outcome.failures.len(), 3);
    }

    #[tokio::test]
    async fn every_failure_is_named() {
        let ops = [failing("one"), failing("two")];
        let outcome = race(&ops, 2, 1, QUICK).await;
        assert!(outcome.taken.is_empty());
        assert!(
            outcome.failures.iter().any(|f| f.starts_with("one: ")),
            "{:?}",
            outcome.failures
        );
        assert!(
            outcome.failures.iter().any(|f| f.starts_with("two: ")),
            "{:?}",
            outcome.failures
        );
    }

    #[tokio::test]
    async fn an_operation_past_its_timeout_fails_instead_of_hanging_the_race() {
        let ops = [
            answering("slow", Duration::from_secs(30)),
            answering("quick", Duration::from_millis(1)),
        ];
        let outcome = race(&ops, 2, 2, Duration::from_millis(50)).await;
        assert_eq!(outcome.taken.len(), 1);
        assert!(
            outcome.failures[0].contains("timed out"),
            "{:?}",
            outcome.failures
        );
    }

    #[tokio::test]
    async fn no_operations_is_a_named_failure_not_a_panic() {
        let ops: [(&str, OpFn<'_, &str>); 0] = [];
        let outcome = race(&ops, 3, 1, QUICK).await;
        assert!(outcome.taken.is_empty());
        assert_eq!(outcome.failures, vec!["no operations to race".to_owned()]);
    }
}
