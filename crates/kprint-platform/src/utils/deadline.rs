use std::future::Future;
use std::time::Duration;
use std::time::Instant;

tokio::task_local! {
  static DEADLINE: Instant;
}

/// The deadline of what a future does passed before it finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeadlinePassed;

impl std::fmt::Display for DeadlinePassed {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str("Timed out.")
  }
}

impl std::error::Error for DeadlinePassed {}

/// How long after the deadline what's run is dropped, so what gives up at
/// the deadline itself (ex. a download) can say what it was first.
const DEADLINE_GRACE: Duration = Duration::from_secs(1);

/// Runs the future until the deadline, after which it's dropped. A download
/// it makes gives up at the deadline too, rather than being left running
/// once the future is dropped (see [`current_deadline`]).
///
/// A deadline this is already run under still applies, so it can only be
/// made earlier.
pub async fn run_before_deadline<T>(deadline: Instant, future: impl Future<Output = T>) -> Result<T, DeadlinePassed> {
  let deadline = current_deadline().map_or(deadline, |current| current.min(deadline));
  DEADLINE
    .scope(deadline, tokio::time::timeout_at((deadline + DEADLINE_GRACE).into(), future))
    .await
    .map_err(|_| DeadlinePassed)
}

/// The deadline of what's being run by [`run_before_deadline`], when it is.
pub fn current_deadline() -> Option<Instant> {
  DEADLINE.try_with(|deadline| *deadline).ok()
}

#[cfg(test)]
mod test {
  use super::*;

  #[test]
  fn runs_a_future_until_its_deadline() {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
    runtime.block_on(async {
      let start = Instant::now();
      let deadline = start + Duration::from_millis(100);
      assert_eq!(current_deadline(), None);
      assert_eq!(run_before_deadline(deadline, async { current_deadline() }).await, Ok(Some(deadline)));
      // and what doesn't give up at the deadline itself is dropped soon after
      assert_eq!(run_before_deadline(deadline, std::future::pending::<()>()).await, Err(DeadlinePassed));
      assert!(start.elapsed() >= Duration::from_millis(100) + DEADLINE_GRACE);
      assert!(start.elapsed() < Duration::from_secs(5));
      // which a future that does gets to report
      let start = Instant::now();
      let gave_up = run_before_deadline(start + Duration::from_millis(100), async {
        tokio::time::sleep_until(current_deadline().unwrap().into()).await;
        "gave up"
      })
      .await;
      assert_eq!(gave_up, Ok("gave up"));
      // only while the future runs
      assert_eq!(current_deadline(), None);
    });
  }

  #[test]
  fn keeps_an_earlier_deadline_it_runs_under() {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
    runtime.block_on(async {
      let earlier = Instant::now() + Duration::from_secs(60);
      let later = earlier + Duration::from_secs(60);
      let deadlines = run_before_deadline(earlier, async {
        (
          run_before_deadline(later, async { current_deadline() }).await,
          run_before_deadline(earlier - Duration::from_secs(1), async { current_deadline() }).await,
        )
      })
      .await
      .unwrap();
      assert_eq!(deadlines, (Ok(Some(earlier)), Ok(Some(earlier - Duration::from_secs(1)))));
    });
  }
}
