use kprint_plugin_types::PluginInfo;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Instant;

#[derive(Clone, Debug, PartialEq)]
pub struct CompilationResult {
  pub bytes: Vec<u8>,
  pub plugin_info: PluginInfo,
}

/// Bounds a compile from outside its own limits.
#[derive(Debug, Clone, Default)]
pub struct CompileControl {
  cancelled: Arc<AtomicBool>,
  deadline: Option<Instant>,
}

impl CompileControl {
  /// A compile that has to be done by the deadline, when there's one.
  pub fn new(deadline: Option<Instant>) -> Self {
    Self {
      cancelled: Default::default(),
      deadline,
    }
  }

  pub fn deadline(&self) -> Option<Instant> {
    self.deadline
  }

  /// Stops the compile, as nothing waits for it anymore.
  pub fn cancel(&self) {
    self.cancelled.store(true, Ordering::SeqCst);
  }

  /// Why the compile has to stop now, if it does.
  pub fn aborted(&self, now: Instant) -> Option<Aborted> {
    if self.cancelled.load(Ordering::SeqCst) {
      Some(Aborted::Cancelled)
    } else if self.deadline.is_some_and(|deadline| now >= deadline) {
      Some(Aborted::DeadlinePassed)
    } else {
      None
    }
  }
}

/// Why a compile stopped before it was done, other than failing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aborted {
  /// Nothing waits for it anymore.
  Cancelled,
  /// What it's for had to be done.
  DeadlinePassed,
  /// It used all the time it has (see [`TimeBudget`]).
  OutOfTime,
}

impl std::fmt::Display for Aborted {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Aborted::Cancelled => f.write_str("nothing waits for it anymore"),
      Aborted::DeadlinePassed => f.write_str("what it's for ran out of time"),
      Aborted::OutOfTime => f.write_str("it ran out of time"),
    }
  }
}
