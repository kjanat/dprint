//! Sets up wasm plugins in a separate process that dprint supervises.
//!
//! Setting up a wasm plugin compiles it with Cranelift and then runs plugin
//! code (the module's start function and its plugin info export). Neither can
//! be interrupted once started, so a pathological compile or plugin code stuck
//! in a loop would hang dprint indefinitely. Doing the setup in a child process
//! gives dprint something it can kill. The parent watches the child while it
//! works and:
//!
//! - notices as soon as it exits without a result (ex. it crashed),
//! - kills it once its CPU time stops increasing (it's blocked, not working),
//! - kills it once a step uses far more CPU time than that step needs,
//!
//! - kills it once an attempt takes longer than it could need however much
//!   CPU time it's given, and gives up once the compile as a whole does
//!   (waiting for a worker slot included), however slowly the CPU time
//!   increases. That time grows with the module, up to [`MAX_TOTAL_WALL`]
//!   unless `DPRINT_WASM_COMPILE_TIMEOUT` gives another,
//! - kills it once nothing waits for it anymore, or what it's for has to be
//!   done (see [`CompileControl`]),
//!
//! then retries, unless it's out of time. A compile that keeps the CPU busy
//! past its limit is retried without Cranelift's optimizations, because
//! retrying the same deterministic compile would only spin the same way
//! again. A worker that crashed or got blocked is first retried as is, since
//! that's more likely to be transient.
//!
//! The CPU time is evidence of whether the worker is working, not proof: one
//! that gets no CPU time (ex. it's throttled, starved or waiting on I/O) is
//! killed and retried like a blocked one, and the wall clock limits are what
//! bound it either way.
//!
//! When a worker can't be started (ex. the dprint executable can't be found
//! or the OS refuses to contain the process), compiling fails rather than
//! running in the dprint process, where nothing could stop it. Only setting
//! `DPRINT_WASM_COMPILE_WORKER=0` does that. A worker stuck in uninterruptible
//! kernel I/O can also take a moment to go once it's killed.

use std::ffi::OsString;
use std::io::BufReader;
use std::io::BufWriter;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Duration;
use std::time::Instant;

use anyhow::Result;
use dprint_core::owned_child::OwnedChild;
use dprint_core::plugins::PluginInfo;

use super::super::NoRetrySetupError;
use super::WASM_PLUGIN_THREAD_STACK_SIZE;
use super::compile::WasmSetupStep;
use super::compile::compile_with_steps;
use crate::environment::Environment;
use crate::plugins::CompilationResult;

/// When this is the first argument, the dprint binary runs as a compile worker.
pub const COMPILE_WORKER_ARG: &str = "__compile-wasm-plugin";
const UNOPTIMIZED_ARG: &str = "--unoptimized";
/// Set to `0` to set up wasm plugins in the dprint process itself, unsupervised.
const WORKER_ENV_VAR: &str = "DPRINT_WASM_COMPILE_WORKER";
/// Set to a number of seconds to give a compile that long in all (see
/// [`Limits::total_wall`]) instead of what its size gives it, up to
/// [`MAX_TOTAL_WALL`].
const TIMEOUT_ENV_VAR: &str = "DPRINT_WASM_COMPILE_TIMEOUT";
const MAX_ATTEMPTS: usize = 3;

/// The most time a compile gets in all, however large the module, unless
/// [`TIMEOUT_ENV_VAR`] says otherwise. The time a module's size gives it
/// bounds a compile that makes slow progress, but by itself it would allow
/// 77 minutes for a 37 MiB module and 34 hours for the largest the protocol
/// accepts, which is as good as hanging.
///
/// Measured with a release build on a 4 vCPU x86_64 machine, the largest
/// plugins at plugins.dprint.dev compile in: ruff (12.4 MiB) 3.6s on 4
/// threads and 12.7s on 1; biome (9.8 MiB) 3.3s and 11.5s; bibtex-tidy (6.6
/// MiB) 2.2s and 7.8s; oxc (4.9 MiB) 1.9s and 6.6s; typescript (4.0 MiB) 1.4s
/// and 5.0s. The other 25 take under 4s on 1 thread. So on 1 thread a
/// compile takes 1.0 to 1.4s per MiB, and 10 minutes is over 40 times what
/// the largest plugin takes that way: enough for a machine 10 times slower
/// to compile it three times over, and for a cold cache of all of them to
/// queue for one worker. A module that genuinely needs longer is far larger
/// than any plugin, so it says so with the environment variable.
const MAX_TOTAL_WALL: Duration = Duration::from_secs(10 * 60);

/// Compiles a wasm plugin in a supervised worker process, retrying when the
/// worker stalls or crashes.
pub fn compile_supervised<TEnvironment: Environment>(
  environment: &TEnvironment,
  plugin_display: &str,
  wasm_bytes: &[u8],
  control: &CompileControl,
) -> Result<CompilationResult> {
  match compile_mode(environment) {
    // nothing can stop it once it starts, which was asked for
    CompileMode::InProcess => return super::compile(wasm_bytes),
    // a test binary can't run as the worker
    CompileMode::Supervised if cfg!(test) => return super::compile(wasm_bytes),
    CompileMode::Supervised => {}
  }
  let wasm_bytes: Arc<[u8]> = Arc::from(wasm_bytes);
  let (max_workers, threads_per_worker) = worker_parallelism(environment.max_threads());
  supervise_compile(
    environment,
    plugin_display,
    wasm_bytes.len(),
    &Limits::for_module(wasm_bytes.len(), compile_timeout(environment)),
    control,
    WorkerQueue {
      slots: &WORKER_SLOTS,
      limit: max_workers,
    },
    || Ok(environment.current_exe()?),
    |executable, optimize| {
      let worker = ProcessWorker::spawn(executable, wasm_bytes.clone(), optimize, threads_per_worker)?;
      Ok(Box::new(worker) as Box<dyn Worker>)
    },
  )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompileMode {
  /// In a worker process dprint supervises.
  Supervised,
  /// In the dprint process, which nothing can stop once it starts. Only when
  /// asked for with `DPRINT_WASM_COMPILE_WORKER=0`.
  InProcess,
}

fn compile_mode(environment: &impl Environment) -> CompileMode {
  if environment.env_var(WORKER_ENV_VAR).is_some_and(|value| value == "0") {
    CompileMode::InProcess
  } else {
    CompileMode::Supervised
  }
}

/// The time [`TIMEOUT_ENV_VAR`] gives a compile in all, if it's set to a
/// number of seconds.
fn compile_timeout(environment: &impl Environment) -> Option<Duration> {
  let value = environment.env_var(TIMEOUT_ENV_VAR)?;
  match value.to_str().and_then(|value| value.trim().parse::<u64>().ok()).filter(|seconds| *seconds > 0) {
    Some(seconds) => Some(Duration::from_secs(seconds)),
    None => {
      log_warn!(
        environment,
        "Ignoring {}={}, as it isn't a number of seconds above 0.",
        TIMEOUT_ENV_VAR,
        value.to_string_lossy()
      );
      None
    }
  }
}

/// The error for a compile that couldn't be supervised, which is never
/// done in the dprint process instead.
fn worker_setup_error(plugin_display: &str, what_failed: &str, err: anyhow::Error) -> anyhow::Error {
  NoRetrySetupError(format!(
    concat!(
      "Error compiling {}: {}: {:#}\n\n",
      "dprint compiles a plugin in a separate process, so it can stop a compile that hangs. ",
      "To compile it in the dprint process instead, where nothing can stop it, set {}=0."
    ),
    plugin_display, what_failed, err, WORKER_ENV_VAR,
  ))
  .into()
}

/// Supervises compiling a plugin in worker processes it spawns: waits for a
/// worker slot, then runs the attempts, all within the compile's time budget.
#[allow(clippy::too_many_arguments)]
fn supervise_compile<TEnvironment: Environment>(
  environment: &TEnvironment,
  plugin_display: &str,
  wasm_len: usize,
  limits: &Limits,
  control: &CompileControl,
  queue: WorkerQueue,
  current_exe: impl FnOnce() -> Result<std::path::PathBuf>,
  mut spawn: impl FnMut(&Path, bool) -> Result<Box<dyn Worker>>,
) -> Result<CompilationResult> {
  // the time starts before anything else, so waiting for the compiles ahead
  // of this one counts
  let budget = TimeBudget::start(limits, control);
  let executable = current_exe().map_err(|err| worker_setup_error(plugin_display, "Could not find the dprint executable to run it with", err))?;
  let _slot = queue.wait(control, budget.deadline).map_err(|aborted| {
    NoRetrySetupError(match aborted {
      Aborted::OutOfTime => format!(
        "Stopped compiling {} while waiting to start, as the compiles ahead of it took the {:.1}s it has",
        plugin_display,
        budget.total().as_secs_f64()
      ),
      aborted => format!("Stopped compiling {} while waiting to start: {}", plugin_display, aborted),
    })
  })?;
  run_attempts(environment, plugin_display, wasm_len, limits, control, budget, |optimize| {
    spawn(&executable, optimize).map_err(|err| worker_setup_error(plugin_display, "Could not start a process to compile it in", err))
  })
}

/// The wall clock time a compile has for all of it: waiting for a worker
/// slot, the attempts and the retries, which don't extend it.
#[derive(Debug, Clone, Copy)]
struct TimeBudget {
  start: Instant,
  deadline: Instant,
}

impl TimeBudget {
  /// Starts now, ending at the deadline of what the compile is for when
  /// that's sooner than the limit.
  fn start(limits: &Limits, control: &CompileControl) -> Self {
    let start = Instant::now();
    let deadline = start + limits.total_wall;
    Self {
      start,
      deadline: control.deadline.map_or(deadline, |caller_deadline| caller_deadline.min(deadline)),
    }
  }

  fn total(&self) -> Duration {
    self.deadline - self.start
  }
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

  #[cfg(test)]
  pub fn deadline(&self) -> Option<Instant> {
    self.deadline
  }

  /// Stops the compile, as nothing waits for it anymore.
  pub fn cancel(&self) {
    self.cancelled.store(true, Ordering::SeqCst);
  }

  /// Why the compile has to stop now, if it does.
  fn aborted(&self, now: Instant) -> Option<Aborted> {
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
enum Aborted {
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

/// How many workers may run at once, and how many threads each one compiles
/// with, so that together they use at most `max_threads` threads.
///
/// Every plugin of a cold cache is set up at the same time and a worker is a
/// process of its own, so without a limit each one would compile with
/// `max_threads` threads (and its own memory). Cranelift compiles a module's
/// functions in parallel and the largest plugin decides how long a cold setup
/// takes, so a worker gets up to 4 threads, and a larger budget runs more
/// workers at once.
fn worker_parallelism(max_threads: usize) -> (usize, usize) {
  let threads_per_worker = max_threads.clamp(1, 4);
  ((max_threads / threads_per_worker).max(1), threads_per_worker)
}

/// Limits how many compile workers run at once (see `worker_parallelism`).
struct WorkerSlots {
  running: Mutex<usize>,
  freed: Condvar,
}

static WORKER_SLOTS: WorkerSlots = WorkerSlots {
  running: Mutex::new(0),
  freed: Condvar::new(),
};

/// How often a compile waiting for a worker slot checks whether it should
/// stop waiting.
const QUEUE_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The worker slots a compile waits for, and how many of them there are.
struct WorkerQueue {
  slots: &'static WorkerSlots,
  limit: usize,
}

impl WorkerQueue {
  /// Waits for a slot, unless the compile is cancelled, past the deadline of
  /// what it's for, or past the deadline of its time budget first.
  fn wait(&self, control: &CompileControl, deadline: Instant) -> std::result::Result<WorkerSlot, Aborted> {
    let aborted = |now: Instant| control.aborted(now).or((now >= deadline).then_some(Aborted::OutOfTime));
    let mut running = self.slots.running.lock().unwrap_or_else(|err| err.into_inner());
    while *running >= self.limit {
      if let Some(aborted) = aborted(Instant::now()) {
        return Err(aborted);
      }
      running = self
        .slots
        .freed
        .wait_timeout(running, QUEUE_POLL_INTERVAL)
        .unwrap_or_else(|err| err.into_inner())
        .0;
    }
    if let Some(aborted) = aborted(Instant::now()) {
      return Err(aborted);
    }
    *running += 1;
    Ok(WorkerSlot(self.slots))
  }
}

struct WorkerSlot(&'static WorkerSlots);

impl Drop for WorkerSlot {
  fn drop(&mut self) {
    *self.0.running.lock().unwrap_or_else(|err| err.into_inner()) -= 1;
    self.0.freed.notify_one();
  }
}

/// Runs this process as a compile worker: reads a wasm module from stdin, sets
/// it up, and reports each step as it starts followed by the result on stdout.
pub fn run_compile_worker(args: &[OsString]) -> i32 {
  let optimize = !args.iter().any(|arg| arg == UNOPTIMIZED_ARG);
  let wasm_bytes = match read_module(&mut std::io::stdin().lock()) {
    Ok(wasm_bytes) => wasm_bytes,
    Err(err) => {
      #[allow(clippy::print_stderr)]
      {
        eprintln!("Error reading the wasm module from stdin: {:#}", err);
      }
      return 1;
    }
  };
  // The parent keeps stdin open while it supervises this worker, and the OS
  // closes it when the parent exits, even when it's killed. Exit then, rather
  // than finishing (or spinning in) a setup nothing is waiting for.
  std::thread::spawn(|| {
    let mut byte = [0; 1];
    while let Ok(1..) = std::io::stdin().read(&mut byte) {}
    std::process::exit(1);
  });
  // plugin code runs on the native stack, so use the stack size plugins run with
  let handle = std::thread::Builder::new().stack_size(WASM_PLUGIN_THREAD_STACK_SIZE).spawn(move || {
    let mut stdout = BufWriter::new(std::io::stdout().lock());
    let mut write_failed = false;
    let result = compile_with_steps(&wasm_bytes, optimize, &mut |step| {
      write_failed |= write_message(&mut stdout, &WorkerMessage::Step(step)).is_err();
    });
    let message = match result {
      Ok(result) => WorkerMessage::Done(result),
      Err(err) => WorkerMessage::Error(format!("{:#}", err)),
    };
    if write_message(&mut stdout, &message).is_err() || write_failed {
      1
    } else {
      0
    }
  });
  match handle {
    Ok(handle) => handle.join().unwrap_or(1),
    Err(_) => 1,
  }
}

// ---- protocol (worker stdin) ----

/// Writes the module with its length first, so the worker knows when it has
/// all of it while stdin stays open.
fn write_module(writer: &mut impl Write, wasm_bytes: &[u8]) -> std::io::Result<()> {
  write_chunk(writer, wasm_bytes)?;
  writer.flush()
}

fn read_module(reader: &mut impl Read) -> std::io::Result<Vec<u8>> {
  read_chunk(reader, MAX_MODULE_LEN)
}

// ---- protocol (worker stdout) ----

enum WorkerMessage {
  /// A step of the setup started.
  Step(WasmSetupStep),
  /// The setup failed for a reason retrying won't fix (ex. not a dprint plugin).
  Error(String),
  Done(CompilationResult),
}

// What the worker may send is bounded, so a worker that goes wrong can't
// make dprint use any amount of memory.
const MAX_MODULE_LEN: u64 = 1024 * 1024 * 1024;
const MAX_ERROR_LEN: u64 = 1024 * 1024;
const MAX_PLUGIN_INFO_LEN: u64 = 1024 * 1024;
const MAX_COMPILED_LEN: u64 = 1024 * 1024 * 1024;

const STEP_TAG: u8 = b'S';
const ERROR_TAG: u8 = b'E';
const DONE_TAG: u8 = b'D';

fn write_message(writer: &mut impl Write, message: &WorkerMessage) -> std::io::Result<()> {
  match message {
    WorkerMessage::Step(step) => writer.write_all(&[STEP_TAG, step.as_u8()])?,
    WorkerMessage::Error(text) => {
      writer.write_all(&[ERROR_TAG])?;
      write_chunk(writer, text.as_bytes())?;
    }
    WorkerMessage::Done(result) => {
      writer.write_all(&[DONE_TAG])?;
      write_chunk(writer, &serde_json::to_vec(&result.plugin_info)?)?;
      write_chunk(writer, &result.bytes)?;
    }
  }
  // flush every message so the parent sees each step as it starts
  writer.flush()
}

fn write_chunk(writer: &mut impl Write, bytes: &[u8]) -> std::io::Result<()> {
  writer.write_all(&(bytes.len() as u64).to_le_bytes())?;
  writer.write_all(bytes)
}

/// Reads the next message, or `None` once the worker's output has ended.
fn read_message(reader: &mut impl Read) -> std::io::Result<Option<WorkerMessage>> {
  let mut tag = [0; 1];
  match reader.read_exact(&mut tag) {
    Ok(()) => {}
    Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
    Err(err) => return Err(err),
  }
  let message = match tag[0] {
    STEP_TAG => {
      let mut step = [0; 1];
      reader.read_exact(&mut step)?;
      WorkerMessage::Step(WasmSetupStep::from_u8(step[0]).ok_or_else(|| invalid_data(format!("unknown step {}", step[0])))?)
    }
    ERROR_TAG => WorkerMessage::Error(String::from_utf8_lossy(&read_chunk(reader, MAX_ERROR_LEN)?).into_owned()),
    DONE_TAG => {
      let plugin_info: PluginInfo = serde_json::from_slice(&read_chunk(reader, MAX_PLUGIN_INFO_LEN)?).map_err(invalid_data)?;
      let bytes = read_chunk(reader, MAX_COMPILED_LEN)?;
      WorkerMessage::Done(CompilationResult { bytes, plugin_info })
    }
    tag => return Err(invalid_data(format!("unknown message tag {}", tag))),
  };
  Ok(Some(message))
}

fn read_chunk(reader: &mut impl Read, max_len: u64) -> std::io::Result<Vec<u8>> {
  let mut len = [0; 8];
  reader.read_exact(&mut len)?;
  let len = u64::from_le_bytes(len);
  if len > max_len {
    return Err(invalid_data(format!("{} bytes is over the limit of {} bytes", len, max_len)));
  }
  let mut bytes = Vec::new();
  reader.take(len).read_to_end(&mut bytes)?;
  if bytes.len() as u64 != len {
    return Err(std::io::ErrorKind::UnexpectedEof.into());
  }
  Ok(bytes)
}

fn invalid_data(err: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> std::io::Error {
  std::io::Error::new(std::io::ErrorKind::InvalidData, err)
}

// ---- supervision ----

/// How long a worker may go without progress, how much CPU time each step
/// may use, and how long an attempt and all of them may take.
#[derive(Debug, Clone)]
struct Limits {
  /// How often to check on the worker.
  poll_interval: Duration,
  /// The worker is blocked once its CPU time hasn't increased for this long.
  no_progress: Duration,
  /// CPU time for starting up and reading the module, before the first step.
  startup_cpu: Duration,
  /// CPU time for compiling: a base amount plus an amount per MiB of wasm.
  compile_cpu_base: Duration,
  compile_cpu_per_mib: Duration,
  /// CPU time for each of the other steps.
  step_cpu: Duration,
  /// How long an attempt may take, however much CPU time it gets.
  attempt_wall: Duration,
  /// How long the compile may take in all: waiting for a worker slot, the
  /// attempts and the retries.
  total_wall: Duration,
}

impl Limits {
  /// The limits for a module of the size, with the time it gets in all
  /// overridden when `total_wall` is given (see [`TIMEOUT_ENV_VAR`]).
  fn for_module(wasm_len: usize, total_wall: Option<Duration>) -> Self {
    // debug builds of dprint compile with a debug build of Cranelift, which is
    // about 10x slower
    let compile_scale = if cfg!(debug_assertions) { 10 } else { 1 };
    Self::for_module_scaled(wasm_len, compile_scale, total_wall)
  }

  /// `compile_scale` multiplies the time compiling may take.
  fn for_module_scaled(wasm_len: usize, compile_scale: u32, total_wall: Option<Duration>) -> Self {
    let mut limits = Self {
      poll_interval: Duration::from_millis(100),
      no_progress: Duration::from_secs(5),
      startup_cpu: Duration::from_secs(10),
      // compiling needs at most ~1.5s of CPU time per MiB of wasm (ex. ruff's
      // 12.4 MiB takes ~10s), so this allows for CPUs ~20x slower than that
      compile_cpu_base: Duration::from_secs(30) * compile_scale,
      compile_cpu_per_mib: Duration::from_secs(30) * compile_scale,
      // serializing and the plugin code take milliseconds
      step_cpu: Duration::from_secs(3),
      attempt_wall: Duration::ZERO,
      total_wall: Duration::ZERO,
    };
    // An attempt gets twice the CPU time all its steps may use, so one that
    // gets half a CPU still finishes, and all of them get twice that, so a
    // retry after a slow attempt still can. The CPU time is measured across
    // the worker's threads, so a compile using several of them is well
    // within this. All of it is capped though (see `MAX_TOTAL_WALL`), so a
    // large module can't make the limits meaningless.
    let steps_cpu = limits.startup_cpu + limits.cpu_budget(Some(WasmSetupStep::Compile), wasm_len) + limits.step_cpu * 3;
    limits.total_wall = total_wall.unwrap_or_else(|| (steps_cpu * 4).min(MAX_TOTAL_WALL * compile_scale));
    limits.attempt_wall = (steps_cpu * 2).min(limits.total_wall);
    limits
  }

  fn cpu_budget(&self, step: Option<WasmSetupStep>, wasm_len: usize) -> Duration {
    match step {
      None => self.startup_cpu,
      Some(WasmSetupStep::Compile) => self.compile_cpu_base + self.compile_cpu_per_mib.mul_f64(wasm_len as f64 / (1024.0 * 1024.0)),
      Some(_) => self.step_cpu,
    }
  }
}

/// What the supervisor needs from a running worker. This is a trait so the
/// supervision can be tested without spawning processes.
trait Worker {
  /// Waits up to `timeout` for the worker's next message.
  fn recv(&mut self, timeout: Duration) -> WorkerEvent;
  /// The CPU time the worker has used so far, if it can be measured.
  fn cpu_time(&mut self) -> Option<Duration>;
  /// Kills the worker and what it started, and waits for it to exit.
  fn kill(&mut self) -> std::io::Result<()>;
}

enum WorkerEvent {
  Message(WorkerMessage),
  /// Nothing arrived within the timeout.
  Idle,
  /// The worker's output ended without a result. Describes how it exited.
  Exited(String),
}

#[derive(Debug, Clone, PartialEq)]
enum StallReason {
  NoProgress(Duration),
  OverBudget { used: Duration, budget: Duration },
}

/// Decides when a worker has stalled based on its CPU time.
struct StallMonitor<'a> {
  limits: &'a Limits,
  wasm_len: usize,
  step: Option<WasmSetupStep>,
  step_start: Instant,
  step_start_cpu: Option<Duration>,
  last_cpu: Option<Duration>,
  last_progress: Instant,
}

impl<'a> StallMonitor<'a> {
  fn new(limits: &'a Limits, wasm_len: usize, now: Instant, cpu: Option<Duration>) -> Self {
    Self {
      limits,
      wasm_len,
      step: None,
      step_start: now,
      step_start_cpu: cpu,
      last_cpu: cpu,
      last_progress: now,
    }
  }

  fn start_step(&mut self, step: WasmSetupStep, now: Instant, cpu: Option<Duration>) {
    self.step = Some(step);
    self.step_start = now;
    self.step_start_cpu = cpu;
    // starting a step is progress
    self.last_progress = now;
    if cpu.is_some() {
      self.last_cpu = cpu;
    }
  }

  fn check(&mut self, now: Instant, cpu: Option<Duration>) -> Option<StallReason> {
    if let Some(cpu) = cpu {
      if self.last_cpu.is_none_or(|last| cpu > last) {
        self.last_cpu = Some(cpu);
        self.last_progress = now;
      } else {
        let idle = now.saturating_duration_since(self.last_progress);
        if idle >= self.limits.no_progress {
          return Some(StallReason::NoProgress(idle));
        }
      }
    }
    // when the CPU time can't be measured, fall back to the wall clock
    let used = match (cpu, self.step_start_cpu) {
      (Some(cpu), Some(start_cpu)) => cpu.saturating_sub(start_cpu),
      _ => now.saturating_duration_since(self.step_start),
    };
    let budget = self.limits.cpu_budget(self.step, self.wasm_len);
    if used > budget {
      return Some(StallReason::OverBudget { used, budget });
    }
    None
  }
}

#[derive(Debug)]
enum AttemptFailure {
  Stalled {
    step: Option<WasmSetupStep>,
    reason: StallReason,
    kill_error: Option<String>,
  },
  /// The attempt took as long as it was allowed to.
  TimedOut {
    step: Option<WasmSetupStep>,
    elapsed: Duration,
    kill_error: Option<String>,
  },
  Exited {
    step: Option<WasmSetupStep>,
    description: String,
  },
}

impl AttemptFailure {
  fn step(&self) -> Option<WasmSetupStep> {
    match self {
      AttemptFailure::Stalled { step, .. } | AttemptFailure::TimedOut { step, .. } | AttemptFailure::Exited { step, .. } => *step,
    }
  }

  /// The worker kept the CPU busy for longer than the step could need.
  fn is_over_budget(&self) -> bool {
    matches!(
      self,
      AttemptFailure::Stalled {
        reason: StallReason::OverBudget { .. },
        ..
      }
    )
  }

  fn was_killed(&self) -> bool {
    matches!(self, AttemptFailure::Stalled { .. } | AttemptFailure::TimedOut { .. })
  }
}

fn step_description(step: Option<WasmSetupStep>) -> &'static str {
  step.map(|step| step.description()).unwrap_or("starting up")
}

impl std::fmt::Display for AttemptFailure {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let kill_error = match self {
      AttemptFailure::Stalled {
        step,
        reason: StallReason::NoProgress(idle),
        kill_error,
      } => {
        write!(
          f,
          "stalled while {}: no CPU progress for {:.1}s (it's blocked, or isn't given CPU time)",
          step_description(*step),
          idle.as_secs_f64()
        )?;
        kill_error
      }
      AttemptFailure::Stalled {
        step,
        reason: StallReason::OverBudget { used, budget },
        kill_error,
      } => {
        write!(
          f,
          "stalled while {}: used {:.1}s of CPU time, over its limit of {:.1}s",
          step_description(*step),
          used.as_secs_f64(),
          budget.as_secs_f64()
        )?;
        kill_error
      }
      AttemptFailure::TimedOut { step, elapsed, kill_error } => {
        write!(f, "timed out while {}: took {:.1}s", step_description(*step), elapsed.as_secs_f64())?;
        kill_error
      }
      AttemptFailure::Exited { step, description } => return write!(f, "crashed while {}: {}", step_description(*step), description),
    };
    if let Some(kill_error) = kill_error {
      // which explains why it may still be running
      write!(f, " (killing it failed: {})", kill_error)?;
    }
    Ok(())
  }
}

enum AttemptOutcome {
  Done {
    result: CompilationResult,
    step_times: Vec<(WasmSetupStep, Duration)>,
  },
  /// The plugin itself can't be set up, so retrying won't help.
  PluginError(String),
  Failed(AttemptFailure),
  /// It was stopped from outside (see `CompileControl`), and killed.
  Aborted {
    aborted: Aborted,
    kill_error: Option<String>,
  },
}

/// Kills the worker, giving why that failed when it did.
fn kill_worker(worker: &mut dyn Worker) -> Option<String> {
  worker.kill().err().map(|err| format!("{:#}", err))
}

/// Supervises an attempt until the worker's done or it has to stop, which is
/// by `deadline` at the latest.
fn supervise_attempt(worker: &mut dyn Worker, limits: &Limits, wasm_len: usize, deadline: Instant, control: &CompileControl) -> AttemptOutcome {
  let start = Instant::now();
  let mut monitor = StallMonitor::new(limits, wasm_len, start, worker.cpu_time());
  let mut step_times = Vec::new();
  let mut current_step: Option<(WasmSetupStep, Instant)> = None;
  loop {
    match worker.recv(limits.poll_interval) {
      WorkerEvent::Message(WorkerMessage::Step(step)) => {
        let now = Instant::now();
        if let Some((previous, start)) = current_step.replace((step, now)) {
          step_times.push((previous, now - start));
        }
        monitor.start_step(step, now, worker.cpu_time());
      }
      WorkerEvent::Message(WorkerMessage::Done(result)) => {
        if let Some((step, start)) = current_step {
          step_times.push((step, start.elapsed()));
        }
        return AttemptOutcome::Done { result, step_times };
      }
      WorkerEvent::Message(WorkerMessage::Error(message)) => return AttemptOutcome::PluginError(message),
      WorkerEvent::Exited(description) => {
        return AttemptOutcome::Failed(AttemptFailure::Exited {
          step: monitor.step,
          description,
        });
      }
      WorkerEvent::Idle => {}
    }
    let now = Instant::now();
    if let Some(aborted) = control.aborted(now) {
      let kill_error = kill_worker(worker);
      return AttemptOutcome::Aborted { aborted, kill_error };
    }
    // however much CPU time it's getting
    if now >= deadline {
      let kill_error = kill_worker(worker);
      return AttemptOutcome::Failed(AttemptFailure::TimedOut {
        step: monitor.step,
        elapsed: now - start,
        kill_error,
      });
    }
    if let Some(reason) = monitor.check(now, worker.cpu_time()) {
      let kill_error = kill_worker(worker);
      return AttemptOutcome::Failed(AttemptFailure::Stalled {
        step: monitor.step,
        reason,
        kill_error,
      });
    }
  }
}

fn run_attempts<TEnvironment: Environment>(
  environment: &TEnvironment,
  plugin_display: &str,
  wasm_len: usize,
  limits: &Limits,
  control: &CompileControl,
  budget: TimeBudget,
  mut spawn: impl FnMut(bool) -> Result<Box<dyn Worker>>,
) -> Result<CompilationResult> {
  let start = Instant::now();
  let total_deadline = budget.deadline;
  let mut optimize = true;
  let mut compile_failures = 0;
  let mut failures = Vec::new();
  let mut attempt = 0;
  while attempt < MAX_ATTEMPTS && Instant::now() < total_deadline {
    attempt += 1;
    let mut worker = spawn(optimize)?;
    let attempt_deadline = (Instant::now() + limits.attempt_wall).min(total_deadline);
    match supervise_attempt(worker.as_mut(), limits, wasm_len, attempt_deadline, control) {
      AttemptOutcome::Done { result, step_times } => {
        log_debug!(
          environment,
          "Compiled {} in {}ms{} ({})",
          plugin_display,
          start.elapsed().as_millis(),
          if attempt > 1 {
            format!(" over {} attempts, the last taking", attempt)
          } else {
            String::new()
          },
          step_times
            .iter()
            .map(|(step, time)| format!("{} {}ms", step.description(), time.as_millis()))
            .collect::<Vec<_>>()
            .join(", ")
        );
        if !optimize {
          log_warn!(
            environment,
            "Compiled {} without optimizations, so it may format more slowly. Run `dprint clear-cache` to try an optimized compile again.",
            plugin_display
          );
        }
        return Ok(result);
      }
      // the plugin fails the same way however often it's set up
      AttemptOutcome::PluginError(message) => return Err(NoRetrySetupError(message).into()),
      AttemptOutcome::Aborted { aborted, kill_error } => {
        return Err(
          NoRetrySetupError(format!(
            "Stopped compiling {}, as {}{}",
            plugin_display,
            aborted,
            kill_error.map(|err| format!(" (killing it failed: {})", err)).unwrap_or_default()
          ))
          .into(),
        );
      }
      AttemptOutcome::Failed(failure) => {
        if failure.step() == Some(WasmSetupStep::Compile) {
          compile_failures += 1;
          // spinning is likely an optimizer pathology that would happen again,
          // as is failing to compile twice
          if failure.is_over_budget() || compile_failures >= 2 {
            optimize = false;
          }
        }
        if attempt < MAX_ATTEMPTS && Instant::now() < total_deadline {
          log_warn!(
            environment,
            "Compiling {} {}. {} (attempt {} of {}).",
            plugin_display,
            failure,
            match (failure.was_killed(), optimize) {
              (true, true) => "Killed it and retrying",
              (true, false) => "Killed it and retrying without optimizations",
              (false, true) => "Retrying",
              (false, false) => "Retrying without optimizations",
            },
            attempt + 1,
            MAX_ATTEMPTS,
          );
        }
        failures.push(failure);
      }
    }
  }
  Err(
    NoRetrySetupError(format!(
      "Failed compiling {} {}:\n{}",
      plugin_display,
      if attempt < MAX_ATTEMPTS {
        format!("within {:.1}s", budget.total().as_secs_f64())
      } else {
        format!("after {} attempts", attempt)
      },
      failures
        .iter()
        .enumerate()
        .map(|(i, failure)| format!("  {}. {}", i + 1, failure))
        .collect::<Vec<_>>()
        .join("\n")
    ))
    .into(),
  )
}

// ---- worker process ----

struct ProcessWorker {
  /// Killed (with anything it started) when this is dropped.
  child: OwnedChild,
  pid: sysinfo::Pid,
  system: sysinfo::System,
  messages: mpsc::Receiver<std::io::Result<WorkerMessage>>,
  stderr: Option<std::thread::JoinHandle<Vec<u8>>>,
  /// Closes the worker's stdin when dropped.
  _close_stdin: mpsc::Sender<()>,
}

impl ProcessWorker {
  fn spawn(executable: &Path, wasm_bytes: Arc<[u8]>, optimize: bool, threads: usize) -> Result<Self> {
    let mut command = Command::new(executable);
    command.arg(COMPILE_WORKER_ARG);
    // wasmtime compiles a module's functions in parallel on rayon's global
    // thread pool, so give it this worker's share (see `worker_parallelism`)
    command.env("RAYON_NUM_THREADS", threads.to_string());
    if !optimize {
      command.arg(UNOPTIMIZED_ARG);
    }
    let mut child = OwnedChild::spawn(command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()))?;

    // these threads end once the worker exits and its pipes close
    let mut stdin = child.take_stdin().unwrap();
    let (close_stdin, stdin_closed) = mpsc::channel::<()>();
    std::thread::spawn(move || {
      if write_module(&mut stdin, &wasm_bytes).is_ok() {
        // stdin stays open while this worker is supervised. The worker exits
        // once it closes, which happens however this process ends.
        let _ = stdin_closed.recv();
      }
    });
    let mut stdout = BufReader::new(child.take_stdout().unwrap());
    let (sender, messages) = mpsc::channel();
    std::thread::spawn(move || {
      loop {
        match read_message(&mut stdout) {
          Ok(Some(message)) => {
            if sender.send(Ok(message)).is_err() {
              break;
            }
          }
          Ok(None) => break,
          Err(err) => {
            let _ = sender.send(Err(err));
            break;
          }
        }
      }
    });
    let mut stderr = child.take_stderr().unwrap();
    let stderr = std::thread::spawn(move || read_end_of(&mut stderr, MAX_STDERR_LEN));

    Ok(Self {
      pid: sysinfo::Pid::from_u32(child.id()),
      child,
      system: sysinfo::System::new(),
      messages,
      stderr: Some(stderr),
      _close_stdin: close_stdin,
    })
  }

  /// Describes how the worker exited once its output ended.
  fn exit_description(&mut self) -> String {
    // the worker closes its output when it exits, so this shouldn't take long
    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
      match self.child.try_wait() {
        Ok(Some(status)) => break Some(status),
        Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
        _ => break None,
      }
    };
    let mut text = match status {
      Some(status) => status.to_string(),
      None => "closed its output without exiting".to_string(),
    };
    // anything it started goes with it, which also closes its stderr
    if let Err(err) = self.kill() {
      text.push_str(&format!(" (killing it failed: {:#})", err));
    }
    let stderr = self.stderr.take().and_then(|handle| handle.join().ok()).unwrap_or_default();
    let stderr = String::from_utf8_lossy(&stderr);
    // the end of the output has the panic or error message
    let lines = stderr.trim().lines().collect::<Vec<_>>();
    for line in &lines[lines.len().saturating_sub(10)..] {
      text.push_str("\n    ");
      text.push_str(line);
    }
    text
  }
}

impl Worker for ProcessWorker {
  fn recv(&mut self, timeout: Duration) -> WorkerEvent {
    match self.messages.recv_timeout(timeout) {
      Ok(Ok(message)) => WorkerEvent::Message(message),
      Ok(Err(err)) => {
        let mut description = format!("unreadable output ({:#})", err);
        if let Err(err) = self.kill() {
          description.push_str(&format!(" (killing it failed: {:#})", err));
        }
        WorkerEvent::Exited(description)
      }
      Err(mpsc::RecvTimeoutError::Timeout) => match self.child.try_wait() {
        // something it started has its output, which would otherwise keep
        // it open after it exited
        Ok(Some(status)) => WorkerEvent::Exited(format!(
          "{}, while something it started kept its output open{}",
          status,
          match self.kill() {
            Ok(()) => String::new(),
            Err(err) => format!(" (killing it failed: {:#})", err),
          }
        )),
        _ => WorkerEvent::Idle,
      },
      Err(mpsc::RecvTimeoutError::Disconnected) => WorkerEvent::Exited(self.exit_description()),
    }
  }

  fn cpu_time(&mut self) -> Option<Duration> {
    self.system.refresh_processes_specifics(
      sysinfo::ProcessesToUpdate::Some(&[self.pid]),
      true,
      sysinfo::ProcessRefreshKind::nothing().with_cpu(),
    );
    self
      .system
      .process(self.pid)
      .map(|process| Duration::from_millis(process.accumulated_cpu_time()))
  }

  fn kill(&mut self) -> std::io::Result<()> {
    // also waits for it to exit
    self.child.kill()
  }
}

/// How much of the end of a worker's stderr is kept, which has the panic or
/// error message.
const MAX_STDERR_LEN: usize = 64 * 1024;

/// Reads the reader to its end, keeping only the last `max_len` bytes.
fn read_end_of(reader: &mut impl Read, max_len: usize) -> Vec<u8> {
  let mut kept = std::collections::VecDeque::with_capacity(max_len.min(8192));
  let mut buffer = [0; 8192];
  loop {
    match reader.read(&mut buffer) {
      Ok(0) => break,
      Ok(read) => {
        kept.extend(&buffer[..read]);
        let excess = kept.len().saturating_sub(max_len);
        kept.drain(..excess);
      }
      Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
      Err(_) => break,
    }
  }
  kept.into()
}

#[cfg(test)]
mod test {
  use std::collections::VecDeque;
  use std::sync::Mutex;

  use super::*;
  use crate::environment::TestEnvironment;

  fn plugin_info() -> PluginInfo {
    PluginInfo {
      name: "test-plugin".to_string(),
      version: "0.1.0".to_string(),
      config_key: "test".to_string(),
      help_url: "https://dprint.dev/plugins/test".to_string(),
      config_schema_url: String::new(),
      update_url: None,
    }
  }

  fn compilation_result() -> CompilationResult {
    CompilationResult {
      bytes: vec![1, 2, 3],
      plugin_info: plugin_info(),
    }
  }

  #[test]
  fn protocol_round_trips_messages() {
    let mut bytes = Vec::new();
    write_message(&mut bytes, &WorkerMessage::Step(WasmSetupStep::Compile)).unwrap();
    write_message(&mut bytes, &WorkerMessage::Step(WasmSetupStep::PluginInfo)).unwrap();
    write_message(&mut bytes, &WorkerMessage::Error("not a plugin".to_string())).unwrap();
    write_message(&mut bytes, &WorkerMessage::Done(compilation_result())).unwrap();

    let mut reader = bytes.as_slice();
    assert!(matches!(read_message(&mut reader).unwrap(), Some(WorkerMessage::Step(WasmSetupStep::Compile))));
    assert!(matches!(
      read_message(&mut reader).unwrap(),
      Some(WorkerMessage::Step(WasmSetupStep::PluginInfo))
    ));
    assert!(matches!(read_message(&mut reader).unwrap(), Some(WorkerMessage::Error(text)) if text == "not a plugin"));
    match read_message(&mut reader).unwrap() {
      Some(WorkerMessage::Done(result)) => assert_eq!(result, compilation_result()),
      _ => unreachable!(),
    }
    assert!(read_message(&mut reader).unwrap().is_none());
  }

  #[test]
  fn protocol_sends_the_module_with_its_length() {
    let mut bytes = Vec::new();
    write_module(&mut bytes, b"\0asm module").unwrap();
    // followed by nothing while stdin stays open, so the length tells the
    // worker when it has the whole module
    assert_eq!(read_module(&mut bytes.as_slice()).unwrap(), b"\0asm module");
    // the parent exited part way through sending it
    assert!(read_module(&mut &bytes[..bytes.len() - 1]).is_err());
  }

  #[test]
  fn splits_the_threads_between_workers() {
    // (max threads) -> (workers at once, threads per worker)
    assert_eq!(worker_parallelism(1), (1, 1));
    assert_eq!(worker_parallelism(2), (1, 2));
    assert_eq!(worker_parallelism(4), (1, 4));
    assert_eq!(worker_parallelism(6), (1, 4));
    assert_eq!(worker_parallelism(8), (2, 4));
    assert_eq!(worker_parallelism(16), (4, 4));
  }

  #[test]
  fn limits_how_many_workers_run_at_once() {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    static SLOTS: WorkerSlots = WorkerSlots {
      running: Mutex::new(0),
      freed: Condvar::new(),
    };
    static RUNNING: AtomicUsize = AtomicUsize::new(0);
    static MAX_RUNNING: AtomicUsize = AtomicUsize::new(0);
    let handles = (0..8)
      .map(|_| {
        std::thread::spawn(|| {
          let _slot = WorkerQueue { slots: &SLOTS, limit: 2 }.wait(&CompileControl::default(), in_a_minute()).unwrap();
          let running = RUNNING.fetch_add(1, Ordering::SeqCst) + 1;
          MAX_RUNNING.fetch_max(running, Ordering::SeqCst);
          std::thread::sleep(Duration::from_millis(20));
          RUNNING.fetch_sub(1, Ordering::SeqCst);
        })
      })
      .collect::<Vec<_>>();
    for handle in handles {
      handle.join().unwrap();
    }
    assert_eq!(MAX_RUNNING.load(Ordering::SeqCst), 2);
    assert_eq!(*SLOTS.running.lock().unwrap(), 0);
  }

  #[test]
  fn protocol_errors_on_truncated_or_unknown_output() {
    let mut bytes = Vec::new();
    write_message(&mut bytes, &WorkerMessage::Done(compilation_result())).unwrap();
    // the worker died part way through writing its result
    assert!(read_message(&mut &bytes[..bytes.len() - 1]).is_err());
    // something other than the worker protocol (ex. a test harness' output)
    assert!(read_message(&mut b"running 0 tests".as_slice()).is_err());
  }

  fn limits() -> Limits {
    Limits {
      poll_interval: Duration::from_millis(1),
      no_progress: Duration::from_secs(5),
      startup_cpu: Duration::from_secs(10),
      compile_cpu_base: Duration::from_secs(30),
      compile_cpu_per_mib: Duration::from_secs(30),
      step_cpu: Duration::from_secs(10),
      attempt_wall: Duration::from_secs(60),
      total_wall: Duration::from_secs(60),
    }
  }

  #[test]
  fn monitor_detects_no_cpu_progress() {
    let limits = limits();
    let start = Instant::now();
    let cpu = Some(Duration::from_millis(100));
    let mut monitor = StallMonitor::new(&limits, 1024, start, cpu);
    monitor.start_step(WasmSetupStep::Compile, start, cpu);
    // still working
    assert_eq!(monitor.check(start + Duration::from_secs(3), Some(Duration::from_millis(900))), None);
    // the CPU time stopped increasing, but not for long enough yet
    assert_eq!(monitor.check(start + Duration::from_secs(7), Some(Duration::from_millis(900))), None);
    assert_eq!(
      monitor.check(start + Duration::from_secs(8), Some(Duration::from_millis(900))),
      Some(StallReason::NoProgress(Duration::from_secs(5)))
    );
  }

  #[test]
  fn monitor_detects_a_step_over_its_cpu_budget() {
    let limits = limits();
    let start = Instant::now();
    let mut monitor = StallMonitor::new(&limits, 2 * 1024 * 1024, start, Some(Duration::ZERO));
    monitor.start_step(WasmSetupStep::Compile, start, Some(Duration::from_secs(1)));
    // compiling 2 MiB is allowed 30s + 2 * 30s = 90s of CPU time
    assert_eq!(monitor.check(start + Duration::from_secs(30), Some(Duration::from_secs(91))), None);
    assert_eq!(
      monitor.check(start + Duration::from_secs(31), Some(Duration::from_secs(92))),
      Some(StallReason::OverBudget {
        used: Duration::from_secs(91),
        budget: Duration::from_secs(90),
      })
    );

    // plugin code gets a flat budget
    let mut monitor = StallMonitor::new(&limits, 2 * 1024 * 1024, start, Some(Duration::ZERO));
    monitor.start_step(WasmSetupStep::PluginInfo, start, Some(Duration::from_secs(1)));
    assert_eq!(
      monitor.check(start + Duration::from_secs(12), Some(Duration::from_secs(12))),
      Some(StallReason::OverBudget {
        used: Duration::from_secs(11),
        budget: Duration::from_secs(10),
      })
    );
  }

  #[test]
  fn monitor_uses_the_wall_clock_without_cpu_times() {
    let limits = limits();
    let start = Instant::now();
    let mut monitor = StallMonitor::new(&limits, 0, start, None);
    monitor.start_step(WasmSetupStep::Serialize, start, None);
    // can't tell whether it's making progress, so only the budget applies
    assert_eq!(monitor.check(start + Duration::from_secs(9), None), None);
    assert_eq!(
      monitor.check(start + Duration::from_secs(11), None),
      Some(StallReason::OverBudget {
        used: Duration::from_secs(11),
        budget: Duration::from_secs(10),
      })
    );
  }

  /// What a fake worker does once it runs out of scripted events.
  #[derive(Clone, Copy)]
  enum Then {
    /// Its CPU time stops increasing.
    Block,
    /// Its CPU time keeps increasing.
    Spin,
    /// Its CPU time keeps increasing by a tiny amount at a time, which is
    /// always progress and never over a budget.
    Trickle,
    /// Its CPU time can't be measured.
    Unmeasured,
  }

  /// A worker that plays back a script of events.
  struct FakeWorker {
    events: VecDeque<WorkerEvent>,
    then: Then,
    cpu: Duration,
    killed: Arc<Mutex<bool>>,
    kill_error: Option<&'static str>,
  }

  impl Worker for FakeWorker {
    fn recv(&mut self, timeout: Duration) -> WorkerEvent {
      match self.events.pop_front() {
        Some(event) => {
          self.cpu += Duration::from_millis(10);
          event
        }
        None => {
          std::thread::sleep(timeout);
          match self.then {
            Then::Spin => self.cpu += Duration::from_millis(10),
            Then::Trickle => self.cpu += Duration::from_nanos(1),
            Then::Block | Then::Unmeasured => {}
          }
          WorkerEvent::Idle
        }
      }
    }

    fn cpu_time(&mut self) -> Option<Duration> {
      match self.then {
        Then::Unmeasured => None,
        _ => Some(self.cpu),
      }
    }

    fn kill(&mut self) -> std::io::Result<()> {
      *self.killed.lock().unwrap() = true;
      match self.kill_error {
        Some(error) => Err(std::io::Error::other(error)),
        None => Ok(()),
      }
    }
  }

  fn steps_until(last: WasmSetupStep) -> Vec<WorkerEvent> {
    [
      WasmSetupStep::Compile,
      WasmSetupStep::Serialize,
      WasmSetupStep::Instantiate,
      WasmSetupStep::PluginInfo,
    ]
    .into_iter()
    .take(last.as_u8() as usize + 1)
    .map(|step| WorkerEvent::Message(WorkerMessage::Step(step)))
    .collect()
  }

  fn succeeds() -> Vec<WorkerEvent> {
    let mut events = steps_until(WasmSetupStep::PluginInfo);
    events.push(WorkerEvent::Message(WorkerMessage::Done(compilation_result())));
    events
  }

  struct Attempts {
    environment: TestEnvironment,
    limits: Limits,
    control: CompileControl,
    scripts: VecDeque<(Vec<WorkerEvent>, Then)>,
    spawned_optimized: Vec<bool>,
    killed: Vec<Arc<Mutex<bool>>>,
    kill_error: Option<&'static str>,
  }

  impl Attempts {
    fn new(scripts: Vec<(Vec<WorkerEvent>, Then)>) -> Self {
      let mut limits = limits();
      limits.no_progress = Duration::from_millis(50);
      // a spinning compile goes over this quickly
      limits.compile_cpu_base = Duration::from_millis(100);
      limits.compile_cpu_per_mib = Duration::ZERO;
      Self {
        environment: TestEnvironment::new(),
        limits,
        control: CompileControl::default(),
        scripts: scripts.into(),
        spawned_optimized: Vec::new(),
        killed: Vec::new(),
        kill_error: None,
      }
    }

    /// Runs the attempts, failing the test when that doesn't end soon rather
    /// than hanging.
    fn run(&mut self) -> Result<CompilationResult> {
      with_outer_deadline(|| self.run_attempts())
    }

    fn run_attempts(&mut self) -> Result<CompilationResult> {
      let Attempts {
        environment,
        limits,
        control,
        scripts,
        spawned_optimized,
        killed,
        kill_error,
      } = self;
      run_attempts(
        environment,
        "plugin.wasm",
        1024,
        limits,
        control,
        TimeBudget::start(limits, control),
        |optimize| {
          spawned_optimized.push(optimize);
          let worker_killed = Arc::new(Mutex::new(false));
          killed.push(worker_killed.clone());
          let (events, then) = scripts.pop_front().unwrap();
          Ok(Box::new(FakeWorker {
            events: events.into(),
            then,
            cpu: Duration::ZERO,
            killed: worker_killed,
            kill_error: *kill_error,
          }))
        },
      )
    }

    fn killed(&self) -> Vec<bool> {
      self.killed.iter().map(|killed| *killed.lock().unwrap()).collect()
    }
  }

  #[test]
  fn succeeds_on_first_attempt() {
    let mut attempts = Attempts::new(vec![(succeeds(), Then::Block)]);
    assert_eq!(attempts.run().unwrap(), compilation_result());
    assert_eq!(attempts.spawned_optimized, vec![true]);
    assert_eq!(attempts.killed(), vec![false]);
    assert!(attempts.environment.take_stderr_messages().is_empty());
  }

  #[test]
  fn kills_a_blocked_compile_and_retries_it() {
    // the first attempt starts compiling and then stops using the CPU
    let mut attempts = Attempts::new(vec![(steps_until(WasmSetupStep::Compile), Then::Block), (succeeds(), Then::Block)]);
    assert_eq!(attempts.run().unwrap(), compilation_result());
    // being blocked is likely transient, so keep optimizing
    assert_eq!(attempts.spawned_optimized, vec![true, true]);
    assert_eq!(attempts.killed(), vec![true, false]);
    let messages = attempts.environment.take_stderr_messages();
    assert_eq!(messages.len(), 1, "{:?}", messages);
    assert!(
      messages[0].starts_with("Compiling plugin.wasm stalled while compiling: no CPU progress for 0."),
      "{}",
      messages[0]
    );
    assert!(
      messages[0].ends_with("s (it's blocked, or isn't given CPU time). Killed it and retrying (attempt 2 of 3)."),
      "{}",
      messages[0]
    );
  }

  #[test]
  fn kills_a_spinning_compile_and_retries_without_optimizations() {
    let mut attempts = Attempts::new(vec![(steps_until(WasmSetupStep::Compile), Then::Spin), (succeeds(), Then::Block)]);
    assert_eq!(attempts.run().unwrap(), compilation_result());
    assert_eq!(attempts.spawned_optimized, vec![true, false]);
    assert_eq!(attempts.killed(), vec![true, false]);
    let messages = attempts.environment.take_stderr_messages();
    assert_eq!(messages.len(), 2, "{:?}", messages);
    assert!(
      messages[0].starts_with("Compiling plugin.wasm stalled while compiling: used 0.1s of CPU time, over its limit of 0.1s."),
      "{}",
      messages[0]
    );
    assert!(
      messages[0].ends_with("Killed it and retrying without optimizations (attempt 2 of 3)."),
      "{}",
      messages[0]
    );
    assert_eq!(
      messages[1],
      "Compiled plugin.wasm without optimizations, so it may format more slowly. Run `dprint clear-cache` to try an optimized compile again."
    );
  }

  #[test]
  fn kills_spinning_plugin_code_and_retries_it() {
    let mut attempts = Attempts::new(vec![(steps_until(WasmSetupStep::PluginInfo), Then::Spin), (succeeds(), Then::Block)]);
    attempts.limits.step_cpu = Duration::from_millis(100);
    assert_eq!(attempts.run().unwrap(), compilation_result());
    // the compile was fine, so keep optimizing
    assert_eq!(attempts.spawned_optimized, vec![true, true]);
    assert_eq!(attempts.killed(), vec![true, false]);
    let messages = attempts.environment.take_stderr_messages();
    assert!(
      messages[0].starts_with("Compiling plugin.wasm stalled while getting the plugin info: used 0.1s of CPU time"),
      "{}",
      messages[0]
    );
  }

  #[test]
  fn retries_a_crash_in_plugin_code_with_optimizations() {
    let mut crashed = steps_until(WasmSetupStep::PluginInfo);
    crashed.push(WorkerEvent::Exited("exit status: 1".to_string()));
    let mut attempts = Attempts::new(vec![(crashed, Then::Block), (succeeds(), Then::Block)]);
    assert_eq!(attempts.run().unwrap(), compilation_result());
    assert_eq!(attempts.spawned_optimized, vec![true, true]);
    assert_eq!(
      attempts.environment.take_stderr_messages(),
      vec!["Compiling plugin.wasm crashed while getting the plugin info: exit status: 1. Retrying (attempt 2 of 3).".to_string()]
    );
  }

  #[test]
  fn does_not_retry_a_plugin_error() {
    let mut events = steps_until(WasmSetupStep::Compile);
    events.push(WorkerEvent::Message(WorkerMessage::Error("Invalid schema version".to_string())));
    let mut attempts = Attempts::new(vec![(events, Then::Block)]);
    let err = attempts.run().unwrap_err();
    assert_eq!(err.to_string(), "Invalid schema version");
    assert_eq!(attempts.spawned_optimized, vec![true]);
    // nor should the plugin cache set it up again
    assert!(err.downcast_ref::<NoRetrySetupError>().is_some());
  }

  #[test]
  fn errors_after_all_attempts_fail() {
    let mut crashed = steps_until(WasmSetupStep::Compile);
    crashed.push(WorkerEvent::Exited("signal: 9 (SIGKILL)".to_string()));
    let mut attempts = Attempts::new(vec![
      (steps_until(WasmSetupStep::Compile), Then::Block),
      (crashed, Then::Block),
      (steps_until(WasmSetupStep::Instantiate), Then::Block),
    ]);
    let err = attempts.run().unwrap_err().to_string();
    let lines = err.lines().collect::<Vec<_>>();
    assert_eq!(lines[0], "Failed compiling plugin.wasm after 3 attempts:");
    assert!(lines[1].starts_with("  1. stalled while compiling: no CPU progress for"), "{}", lines[1]);
    assert_eq!(lines[2], "  2. crashed while compiling: signal: 9 (SIGKILL)");
    assert!(
      lines[3].starts_with("  3. stalled while instantiating the module: no CPU progress for"),
      "{}",
      lines[3]
    );
    // the compile failing a second time switches off optimizations
    assert_eq!(attempts.spawned_optimized, vec![true, true, false]);
    assert_eq!(attempts.killed(), vec![true, false, true]);
    // a warning for each retry, but not for the final failure
    assert_eq!(attempts.environment.take_stderr_messages().len(), 2);
  }

  /// Runs `f`, aborting the test process when it takes longer than 30s, so
  /// a regression fails rather than hangs.
  fn with_outer_deadline<T>(f: impl FnOnce() -> T) -> T {
    let (done, finished) = mpsc::channel::<()>();
    std::thread::spawn(move || {
      if let Err(mpsc::RecvTimeoutError::Timeout) = finished.recv_timeout(Duration::from_secs(30)) {
        #[allow(clippy::print_stderr)]
        {
          eprintln!("A compile supervision test didn't finish within 30s.");
        }
        std::process::abort();
      }
    });
    let result = f();
    drop(done);
    result
  }

  /// Attempts whose wall clock limits are short, and whose CPU time limits
  /// are long.
  fn attempts_timing_out(scripts: Vec<(Vec<WorkerEvent>, Then)>, attempt_wall: Duration, total_wall: Duration) -> Attempts {
    let mut attempts = Attempts::new(scripts);
    attempts.limits.no_progress = Duration::from_secs(60);
    attempts.limits.compile_cpu_base = Duration::from_secs(60);
    attempts.limits.attempt_wall = attempt_wall;
    attempts.limits.total_wall = total_wall;
    attempts
  }

  #[test]
  fn kills_an_attempt_that_keeps_making_tiny_progress_at_its_wall_limit() {
    let mut attempts = attempts_timing_out(
      vec![(steps_until(WasmSetupStep::Compile), Then::Trickle), (succeeds(), Then::Block)],
      Duration::from_millis(200),
      Duration::from_secs(10),
    );
    assert_eq!(attempts.run().unwrap(), compilation_result());
    assert_eq!(attempts.spawned_optimized, vec![true, true]);
    assert_eq!(attempts.killed(), vec![true, false]);
    let messages = attempts.environment.take_stderr_messages();
    assert_eq!(messages.len(), 1, "{:?}", messages);
    assert!(
      messages[0].starts_with("Compiling plugin.wasm timed out while compiling: took 0."),
      "{}",
      messages[0]
    );
    assert!(messages[0].ends_with("s. Killed it and retrying (attempt 2 of 3)."), "{}", messages[0]);
  }

  #[test]
  fn kills_an_attempt_without_cpu_times_at_its_wall_limit() {
    let mut attempts = attempts_timing_out(
      vec![(steps_until(WasmSetupStep::Compile), Then::Unmeasured), (succeeds(), Then::Block)],
      Duration::from_millis(200),
      Duration::from_secs(10),
    );
    assert_eq!(attempts.run().unwrap(), compilation_result());
    assert_eq!(attempts.killed(), vec![true, false]);
    let messages = attempts.environment.take_stderr_messages();
    assert!(messages[0].starts_with("Compiling plugin.wasm timed out while compiling"), "{}", messages[0]);
  }

  #[test]
  fn gives_up_once_the_attempts_take_all_the_time_they_have_together() {
    let script = || (steps_until(WasmSetupStep::Compile), Then::Trickle);
    let mut attempts = attempts_timing_out(vec![script(), script(), script()], Duration::from_millis(200), Duration::from_millis(300));
    let start = Instant::now();
    let err = attempts.run().unwrap_err();
    // rather than three attempts of 200ms each
    assert!(start.elapsed() < Duration::from_millis(600), "{:?}", start.elapsed());
    assert!(err.downcast_ref::<NoRetrySetupError>().is_some());
    let err = err.to_string();
    let lines = err.lines().collect::<Vec<_>>();
    assert_eq!(lines[0], "Failed compiling plugin.wasm within 0.3s:");
    assert!(lines[1].starts_with("  1. timed out while compiling: took 0.2"), "{}", lines[1]);
    assert!(lines[2].starts_with("  2. timed out while compiling: took 0.1"), "{}", lines[2]);
    assert_eq!(lines.len(), 3);
    assert_eq!(attempts.killed(), vec![true, true]);
    // the second attempt isn't followed by a retry
    assert_eq!(attempts.environment.take_stderr_messages().len(), 1);
  }

  #[test]
  fn stops_an_attempt_once_nothing_waits_for_it() {
    let mut attempts = Attempts::new(vec![(steps_until(WasmSetupStep::Compile), Then::Spin)]);
    attempts.limits.compile_cpu_base = Duration::from_secs(60);
    let control = attempts.control.clone();
    std::thread::spawn(move || {
      std::thread::sleep(Duration::from_millis(50));
      control.cancel();
    });
    let err = attempts.run().unwrap_err();
    assert_eq!(err.to_string(), "Stopped compiling plugin.wasm, as nothing waits for it anymore");
    // and the plugin cache doesn't set it up again
    assert!(err.downcast_ref::<NoRetrySetupError>().is_some());
    assert_eq!(attempts.killed(), vec![true]);
    assert_eq!(attempts.environment.take_stderr_messages(), Vec::<String>::new());
  }

  #[test]
  fn stops_an_attempt_at_the_deadline_of_what_its_for() {
    let mut attempts = Attempts::new(vec![(steps_until(WasmSetupStep::Compile), Then::Spin)]);
    attempts.limits.compile_cpu_base = Duration::from_secs(60);
    attempts.control = CompileControl::new(Some(Instant::now() + Duration::from_millis(50)));
    let err = attempts.run().unwrap_err();
    assert_eq!(err.to_string(), "Stopped compiling plugin.wasm, as what it's for ran out of time");
    assert_eq!(attempts.killed(), vec![true]);
  }

  #[test]
  fn says_when_killing_a_worker_fails() {
    let mut attempts = Attempts::new(vec![(steps_until(WasmSetupStep::Compile), Then::Block), (succeeds(), Then::Block)]);
    attempts.kill_error = Some("Operation not permitted");
    assert_eq!(attempts.run().unwrap(), compilation_result());
    let messages = attempts.environment.take_stderr_messages();
    assert!(
      messages[0].ends_with("(killing it failed: Operation not permitted). Killed it and retrying (attempt 2 of 3)."),
      "{}",
      messages[0]
    );
  }

  #[test]
  fn stops_waiting_for_a_worker_slot_once_cancelled_or_out_of_time() {
    static SLOTS: WorkerSlots = WorkerSlots {
      running: Mutex::new(0),
      freed: Condvar::new(),
    };
    let queue = || WorkerQueue { slots: &SLOTS, limit: 1 };
    let held = queue().wait(&CompileControl::default(), in_a_minute()).unwrap();
    with_outer_deadline(|| {
      let control = CompileControl::default();
      let start = Instant::now();
      std::thread::spawn({
        let control = control.clone();
        move || {
          std::thread::sleep(Duration::from_millis(50));
          control.cancel();
        }
      });
      assert_eq!(queue().wait(&control, in_a_minute()).err(), Some(Aborted::Cancelled));
      assert!(start.elapsed() < Duration::from_secs(5));

      let control = CompileControl::new(Some(Instant::now() + Duration::from_millis(50)));
      assert_eq!(queue().wait(&control, in_a_minute()).err(), Some(Aborted::DeadlinePassed));

      // or once its own time is up
      let deadline = Instant::now() + Duration::from_millis(50);
      assert_eq!(queue().wait(&CompileControl::default(), deadline).err(), Some(Aborted::OutOfTime));
    });
    // a compile that's already cancelled doesn't take a free slot either
    drop(held);
    let control = CompileControl::default();
    control.cancel();
    assert_eq!(queue().wait(&control, in_a_minute()).err(), Some(Aborted::Cancelled));
    assert_eq!(*SLOTS.running.lock().unwrap(), 0);
  }

  /// A deadline a test never reaches.
  fn in_a_minute() -> Instant {
    Instant::now() + Duration::from_secs(60)
  }

  #[test]
  fn caps_the_time_a_compile_gets_however_large_the_module() {
    const MIB: usize = 1024 * 1024;
    let limits = |wasm_len| Limits::for_module_scaled(wasm_len, 1, None);
    // what the size gives it, when that's within the cap: twice the CPU time
    // of its steps (10 + 30 + 30 + 3 * 3 seconds) for an attempt, and twice
    // that in all
    assert_eq!(limits(MIB).attempt_wall, Duration::from_secs(158));
    assert_eq!(limits(MIB).total_wall, Duration::from_secs(316));
    // the largest plugin there is (ruff) would get 28 minutes
    assert_eq!(limits(12 * MIB + 410 * 1024).total_wall, MAX_TOTAL_WALL);
    assert_eq!(limits(12 * MIB + 410 * 1024).attempt_wall, MAX_TOTAL_WALL);
    // and the largest module the protocol accepts 34 hours
    assert_eq!(limits(MAX_MODULE_LEN as usize).total_wall, MAX_TOTAL_WALL);
    assert_eq!(limits(MAX_MODULE_LEN as usize).attempt_wall, MAX_TOTAL_WALL);
    assert_eq!(limits(usize::MAX).total_wall, MAX_TOTAL_WALL);
    // the cap scales with the compile time of a debug build
    assert_eq!(Limits::for_module_scaled(MAX_MODULE_LEN as usize, 10, None).total_wall, MAX_TOTAL_WALL * 10);
    assert!(Limits::for_module(MAX_MODULE_LEN as usize, None).total_wall <= MAX_TOTAL_WALL * 10);
  }

  #[test]
  fn gives_a_compile_the_time_the_environment_says_instead() {
    const MIB: usize = 1024 * 1024;
    // more than the cap, for a module that needs it
    let limits = Limits::for_module_scaled(MAX_MODULE_LEN as usize, 1, Some(Duration::from_secs(3600)));
    assert_eq!(limits.total_wall, Duration::from_secs(3600));
    assert_eq!(limits.attempt_wall, Duration::from_secs(3600));
    // or less than its size gives it, which bounds an attempt too
    let limits = Limits::for_module_scaled(MIB, 1, Some(Duration::from_secs(60)));
    assert_eq!(limits.total_wall, Duration::from_secs(60));
    assert_eq!(limits.attempt_wall, Duration::from_secs(60));

    let environment = TestEnvironment::new();
    assert_eq!(compile_timeout(&environment), None);
    environment.set_env_var(TIMEOUT_ENV_VAR, Some("90"));
    assert_eq!(compile_timeout(&environment), Some(Duration::from_secs(90)));
    assert_eq!(environment.take_stderr_messages(), Vec::<String>::new());
    for value in ["0", "-1", "abc", "1.5", ""] {
      environment.set_env_var(TIMEOUT_ENV_VAR, Some(value));
      assert_eq!(compile_timeout(&environment), None, "{}", value);
      assert_eq!(
        environment.take_stderr_messages(),
        vec![format!(
          "Ignoring DPRINT_WASM_COMPILE_TIMEOUT={}, as it isn't a number of seconds above 0.",
          value
        )]
      );
    }
  }

  #[test]
  fn a_compiles_time_ends_at_the_deadline_of_what_its_for_when_thats_sooner() {
    let limits = limits();
    let budget = TimeBudget::start(&limits, &CompileControl::default());
    assert_eq!(budget.total(), limits.total_wall);
    let deadline = Instant::now() + Duration::from_secs(1);
    let budget = TimeBudget::start(&limits, &CompileControl::new(Some(deadline)));
    assert_eq!(budget.deadline, deadline);
    // and a later one doesn't extend it
    let budget = TimeBudget::start(&limits, &CompileControl::new(Some(Instant::now() + Duration::from_secs(3600))));
    assert_eq!(budget.total(), limits.total_wall);
  }

  #[test]
  fn runs_out_of_time_waiting_for_a_worker_slot_without_starting_a_worker() {
    use std::sync::atomic::AtomicUsize;

    static SLOTS: WorkerSlots = WorkerSlots {
      running: Mutex::new(0),
      freed: Condvar::new(),
    };
    // the compiles ahead of it hold every slot for longer than it has
    let _held = WorkerQueue { slots: &SLOTS, limit: 1 }.wait(&CompileControl::default(), in_a_minute()).unwrap();
    let mut limits = limits();
    limits.total_wall = Duration::from_millis(100);
    let spawned = Arc::new(AtomicUsize::new(0));
    let (done, finished) = mpsc::channel();
    std::thread::spawn({
      let spawned = spawned.clone();
      move || {
        let result = supervise_compile(
          &TestEnvironment::new(),
          "plugin.wasm",
          1024,
          &limits,
          &CompileControl::default(),
          WorkerQueue { slots: &SLOTS, limit: 1 },
          || Ok(std::path::PathBuf::from("/dprint")),
          |_, _| {
            spawned.fetch_add(1, Ordering::SeqCst);
            Err(anyhow::anyhow!("no worker should start"))
          },
        );
        done.send(result.unwrap_err().to_string()).unwrap();
      }
    });
    // rather than waiting for a slot for however long the compiles ahead
    // of it take
    let err = finished
      .recv_timeout(Duration::from_secs(5))
      .expect("the compile should have run out of time while waiting for a slot");
    assert_eq!(
      err,
      "Stopped compiling plugin.wasm while waiting to start, as the compiles ahead of it took the 0.1s it has"
    );
    assert_eq!(spawned.load(Ordering::SeqCst), 0);
  }

  /// Supervises a compile with fakes for finding the executable and
  /// spawning workers, which give the result. There's no way for this to
  /// compile in the dprint process.
  fn supervise_fake_compile(
    current_exe: Result<std::path::PathBuf>,
    mut spawn: impl FnMut(bool) -> Result<Box<dyn Worker>>,
  ) -> (Result<CompilationResult>, TestEnvironment) {
    static SLOTS: WorkerSlots = WorkerSlots {
      running: Mutex::new(0),
      freed: Condvar::new(),
    };
    let environment = TestEnvironment::new();
    let mut limits = limits();
    limits.no_progress = Duration::from_millis(50);
    let result = with_outer_deadline(|| {
      supervise_compile(
        &environment,
        "plugin.wasm",
        1024,
        &limits,
        &CompileControl::default(),
        WorkerQueue { slots: &SLOTS, limit: 1 },
        || current_exe,
        |executable, optimize| {
          assert_eq!(executable, Path::new("/dprint"));
          spawn(optimize)
        },
      )
    });
    (result, environment)
  }

  fn fake_worker(events: Vec<WorkerEvent>, then: Then, killed: &Arc<Mutex<bool>>) -> Box<dyn Worker> {
    Box::new(FakeWorker {
      events: events.into(),
      then,
      cpu: Duration::ZERO,
      killed: killed.clone(),
      kill_error: None,
    })
  }

  const OPT_OUT: &str = "dprint compiles a plugin in a separate process, so it can stop a compile that hangs. To compile it in the dprint process instead, where nothing can stop it, set DPRINT_WASM_COMPILE_WORKER=0.";

  #[test]
  fn fails_rather_than_compiles_in_process_when_the_executable_isnt_found() {
    let (result, _) = supervise_fake_compile(Err(anyhow::anyhow!("No such file or directory")), |_| unreachable!());
    let err = result.unwrap_err();
    assert!(err.downcast_ref::<NoRetrySetupError>().is_some());
    assert_eq!(
      err.to_string(),
      format!(
        "Error compiling plugin.wasm: Could not find the dprint executable to run it with: No such file or directory\n\n{}",
        OPT_OUT
      )
    );
  }

  #[test]
  fn fails_rather_than_compiles_in_process_when_a_worker_cant_be_started() {
    // ex. the OS refuses to put it in a job object or process group
    let mut spawned = 0;
    let start = Instant::now();
    let (result, _) = supervise_fake_compile(Ok("/dprint".into()), |_| {
      spawned += 1;
      Err(anyhow::anyhow!("Error assigning the process to a job object: Access is denied."))
    });
    let err = result.unwrap_err();
    assert!(start.elapsed() < Duration::from_secs(5));
    assert!(err.downcast_ref::<NoRetrySetupError>().is_some());
    assert_eq!(
      err.to_string(),
      format!(
        "Error compiling plugin.wasm: Could not start a process to compile it in: Error assigning the process to a job object: Access is denied.\n\n{}",
        OPT_OUT
      )
    );
    assert_eq!(spawned, 1);
  }

  #[test]
  fn fails_when_a_retry_cant_be_started_after_killing_the_last_worker() {
    let killed = Arc::new(Mutex::new(false));
    let mut spawned = 0;
    let (result, environment) = supervise_fake_compile(Ok("/dprint".into()), |_| {
      spawned += 1;
      match spawned {
        1 => Ok(fake_worker(steps_until(WasmSetupStep::Compile), Then::Block, &killed)),
        _ => Err(anyhow::anyhow!("Resource temporarily unavailable")),
      }
    });
    assert!(
      result
        .unwrap_err()
        .to_string()
        .starts_with("Error compiling plugin.wasm: Could not start a process to compile it in: Resource temporarily unavailable")
    );
    // the worker that was running is gone
    assert!(*killed.lock().unwrap());
    assert_eq!(environment.take_stderr_messages().len(), 1);
  }

  #[test]
  fn compiles_in_process_only_when_asked_to() {
    let environment = TestEnvironment::new();
    assert_eq!(compile_mode(&environment), CompileMode::Supervised);
    environment.set_env_var(WORKER_ENV_VAR, Some("1"));
    assert_eq!(compile_mode(&environment), CompileMode::Supervised);
    environment.set_env_var(WORKER_ENV_VAR, Some("0"));
    assert_eq!(compile_mode(&environment), CompileMode::InProcess);
  }

  #[test]
  fn protocol_rejects_output_over_its_limits() {
    let mut bytes = vec![ERROR_TAG];
    bytes.extend((MAX_ERROR_LEN + 1).to_le_bytes());
    let err = read_message(&mut bytes.as_slice()).err().unwrap();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(
      err.to_string(),
      format!("{} bytes is over the limit of {} bytes", MAX_ERROR_LEN + 1, MAX_ERROR_LEN)
    );

    let mut bytes = vec![DONE_TAG];
    bytes.extend(u64::MAX.to_le_bytes());
    assert_eq!(read_message(&mut bytes.as_slice()).err().unwrap().kind(), std::io::ErrorKind::InvalidData);
  }

  #[test]
  fn keeps_the_end_of_a_workers_stderr() {
    let stderr = (0..100_000).map(|i| (i % 10).to_string()).collect::<String>();
    assert_eq!(read_end_of(&mut stderr.as_bytes(), 10), stderr.as_bytes()[stderr.len() - 10..].to_vec());
    assert_eq!(read_end_of(&mut b"short".as_slice(), 10), b"short".to_vec());
  }

  #[cfg(unix)]
  #[test]
  #[allow(clippy::disallowed_methods)]
  fn kills_what_an_exited_worker_started_that_kept_its_output_open() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("pid");
    let worker_path = dir.path().join("worker");
    // a worker that starts something which keeps its output open, then exits
    std::fs::write(&worker_path, format!("#!/bin/sh\nsleep 60 &\necho $! > '{}'\nexit 3\n", pid_file.display())).unwrap();
    std::fs::set_permissions(&worker_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut worker = ProcessWorker::spawn(&worker_path, Arc::from(&b"\0asm"[..]), true, 1).unwrap();
    let mut limits = limits();
    limits.poll_interval = Duration::from_millis(10);
    let start = Instant::now();
    let outcome = with_outer_deadline(|| supervise_attempt(&mut worker, &limits, 4, start + Duration::from_secs(20), &CompileControl::default()));
    match outcome {
      AttemptOutcome::Failed(AttemptFailure::Exited { description, .. }) => {
        assert!(
          description.starts_with("exit status: 3, while something it started kept its output open"),
          "{}",
          description
        )
      }
      _ => panic!("expected the worker to have exited"),
    }
    // rather than once what it started exits
    assert!(start.elapsed() < Duration::from_secs(10), "{:?}", start.elapsed());
    let pid = std::fs::read_to_string(&pid_file).unwrap().trim().to_string();
    let state = std::process::Command::new("ps").args(["-o", "stat=", "-p", &pid]).output().unwrap();
    let state = String::from_utf8_lossy(&state.stdout).trim().to_string();
    // gone, or a zombie nothing has reaped yet
    assert!(state.is_empty() || state.starts_with('Z'), "{}", state);
  }
}
