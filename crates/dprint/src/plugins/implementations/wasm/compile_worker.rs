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
//! then retries. A compile that keeps the CPU busy past its limit is retried
//! without Cranelift's optimizations, because retrying the same deterministic
//! compile would only spin the same way again. A worker that crashed or got
//! blocked is first retried as is, since that's more likely to be transient.

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
const MAX_ATTEMPTS: usize = 3;

/// Compiles a wasm plugin in a supervised worker process, retrying when the
/// worker stalls or crashes.
pub fn compile_supervised<TEnvironment: Environment>(environment: &TEnvironment, plugin_display: &str, wasm_bytes: &[u8]) -> Result<CompilationResult> {
  // a test binary can't run as the worker
  if cfg!(test) || environment.env_var(WORKER_ENV_VAR).is_some_and(|value| value == "0") {
    return super::compile(wasm_bytes);
  }
  let executable = match environment.current_exe() {
    Ok(executable) => executable,
    Err(err) => {
      log_debug!(
        environment,
        "Compiling {} in process. Could not resolve the current executable: {:#}",
        plugin_display,
        err
      );
      return super::compile(wasm_bytes);
    }
  };
  let wasm_bytes: Arc<[u8]> = Arc::from(wasm_bytes);
  let (max_workers, threads_per_worker) = worker_parallelism(environment.max_threads());
  let _slot = WORKER_SLOTS.acquire(max_workers);
  let mut attempt = 0;
  run_attempts(environment, plugin_display, wasm_bytes.len(), &Limits::default(), |optimize| {
    attempt += 1;
    match ProcessWorker::spawn(&executable, wasm_bytes.clone(), optimize, threads_per_worker) {
      Ok(worker) => Ok(SpawnedWorker::Worker(Box::new(worker))),
      // the worker couldn't start at all, so there's nothing to supervise
      Err(err) if attempt == 1 => {
        log_debug!(
          environment,
          "Compiling {} in process. Could not start a compile worker: {:#}",
          plugin_display,
          err
        );
        Ok(SpawnedWorker::InProcess(super::compile(&wasm_bytes)?))
      }
      Err(err) => Err(err),
    }
  })
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

impl WorkerSlots {
  fn acquire(&'static self, limit: usize) -> WorkerSlot {
    let mut running = self.running.lock().unwrap_or_else(|err| err.into_inner());
    while *running >= limit {
      running = self.freed.wait(running).unwrap_or_else(|err| err.into_inner());
    }
    *running += 1;
    WorkerSlot(self)
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
  read_chunk(reader)
}

// ---- protocol (worker stdout) ----

enum WorkerMessage {
  /// A step of the setup started.
  Step(WasmSetupStep),
  /// The setup failed for a reason retrying won't fix (ex. not a dprint plugin).
  Error(String),
  Done(CompilationResult),
}

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
    ERROR_TAG => WorkerMessage::Error(String::from_utf8_lossy(&read_chunk(reader)?).into_owned()),
    DONE_TAG => {
      let plugin_info: PluginInfo = serde_json::from_slice(&read_chunk(reader)?).map_err(invalid_data)?;
      let bytes = read_chunk(reader)?;
      WorkerMessage::Done(CompilationResult { bytes, plugin_info })
    }
    tag => return Err(invalid_data(format!("unknown message tag {}", tag))),
  };
  Ok(Some(message))
}

fn read_chunk(reader: &mut impl Read) -> std::io::Result<Vec<u8>> {
  let mut len = [0; 8];
  reader.read_exact(&mut len)?;
  let len = u64::from_le_bytes(len);
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

/// How long a worker may go without progress, and how much CPU time each step
/// may use, before it's considered stalled.
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
}

impl Default for Limits {
  fn default() -> Self {
    // debug builds of dprint compile with a debug build of Cranelift, which is
    // about 10x slower
    let compile_scale = if cfg!(debug_assertions) { 10 } else { 1 };
    Self {
      poll_interval: Duration::from_millis(100),
      no_progress: Duration::from_secs(5),
      startup_cpu: Duration::from_secs(10),
      // compiling needs at most ~1.5s of CPU time per MiB of wasm (ex. ruff's
      // 12.4 MiB takes ~10s), so this allows for CPUs ~20x slower than that
      compile_cpu_base: Duration::from_secs(30) * compile_scale,
      compile_cpu_per_mib: Duration::from_secs(30) * compile_scale,
      // serializing and the plugin code take milliseconds
      step_cpu: Duration::from_secs(3),
    }
  }
}

impl Limits {
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
  fn kill(&mut self);
}

enum WorkerEvent {
  Message(WorkerMessage),
  /// Nothing arrived within the timeout.
  Idle,
  /// The worker's output ended without a result. Describes how it exited.
  Exited(String),
}

enum SpawnedWorker {
  Worker(Box<dyn Worker>),
  /// No worker could be started, so the setup already ran in this process.
  InProcess(CompilationResult),
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
  Stalled { step: Option<WasmSetupStep>, reason: StallReason },
  Exited { step: Option<WasmSetupStep>, description: String },
}

impl AttemptFailure {
  fn step(&self) -> Option<WasmSetupStep> {
    match self {
      AttemptFailure::Stalled { step, .. } | AttemptFailure::Exited { step, .. } => *step,
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
}

fn step_description(step: Option<WasmSetupStep>) -> &'static str {
  step.map(|step| step.description()).unwrap_or("starting up")
}

impl std::fmt::Display for AttemptFailure {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      AttemptFailure::Stalled {
        step,
        reason: StallReason::NoProgress(idle),
      } => write!(f, "stalled while {}: no CPU progress for {:.1}s", step_description(*step), idle.as_secs_f64()),
      AttemptFailure::Stalled {
        step,
        reason: StallReason::OverBudget { used, budget },
      } => write!(
        f,
        "stalled while {}: used {:.1}s of CPU time, over its limit of {:.1}s",
        step_description(*step),
        used.as_secs_f64(),
        budget.as_secs_f64()
      ),
      AttemptFailure::Exited { step, description } => write!(f, "crashed while {}: {}", step_description(*step), description),
    }
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
}

fn supervise_attempt(worker: &mut dyn Worker, limits: &Limits, wasm_len: usize) -> AttemptOutcome {
  let mut monitor = StallMonitor::new(limits, wasm_len, Instant::now(), worker.cpu_time());
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
    if let Some(reason) = monitor.check(Instant::now(), worker.cpu_time()) {
      worker.kill();
      return AttemptOutcome::Failed(AttemptFailure::Stalled { step: monitor.step, reason });
    }
  }
}

fn run_attempts<TEnvironment: Environment>(
  environment: &TEnvironment,
  plugin_display: &str,
  wasm_len: usize,
  limits: &Limits,
  mut spawn: impl FnMut(bool) -> Result<SpawnedWorker>,
) -> Result<CompilationResult> {
  let start = Instant::now();
  let mut optimize = true;
  let mut compile_failures = 0;
  let mut failures = Vec::new();
  for attempt in 1..=MAX_ATTEMPTS {
    let mut worker = match spawn(optimize)? {
      SpawnedWorker::Worker(worker) => worker,
      SpawnedWorker::InProcess(result) => return Ok(result),
    };
    match supervise_attempt(worker.as_mut(), limits, wasm_len) {
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
      AttemptOutcome::Failed(failure) => {
        if failure.step() == Some(WasmSetupStep::Compile) {
          compile_failures += 1;
          // spinning is likely an optimizer pathology that would happen again,
          // as is failing to compile twice
          if failure.is_over_budget() || compile_failures >= 2 {
            optimize = false;
          }
        }
        if attempt < MAX_ATTEMPTS {
          log_warn!(
            environment,
            "Compiling {} {}. {} (attempt {} of {}).",
            plugin_display,
            failure,
            match (&failure, optimize) {
              (AttemptFailure::Stalled { .. }, true) => "Killed it and retrying",
              (AttemptFailure::Stalled { .. }, false) => "Killed it and retrying without optimizations",
              (AttemptFailure::Exited { .. }, true) => "Retrying",
              (AttemptFailure::Exited { .. }, false) => "Retrying without optimizations",
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
      "Failed compiling {} after {} attempts:\n{}",
      plugin_display,
      MAX_ATTEMPTS,
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
    let stderr = std::thread::spawn(move || {
      let mut bytes = Vec::new();
      let _ = stderr.read_to_end(&mut bytes);
      bytes
    });

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
        _ => {
          self.kill();
          break None;
        }
      }
    };
    let mut text = match status {
      Some(status) => status.to_string(),
      None => "closed its output without exiting".to_string(),
    };
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
        self.kill();
        WorkerEvent::Exited(format!("unreadable output ({:#})", err))
      }
      Err(mpsc::RecvTimeoutError::Timeout) => WorkerEvent::Idle,
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

  fn kill(&mut self) {
    // also waits for it to exit
    let _ = self.child.kill();
  }
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
          let _slot = SLOTS.acquire(2);
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
  }

  /// A worker that plays back a script of events.
  struct FakeWorker {
    events: VecDeque<WorkerEvent>,
    then: Then,
    cpu: Duration,
    killed: Arc<Mutex<bool>>,
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
          if matches!(self.then, Then::Spin) {
            self.cpu += Duration::from_millis(10);
          }
          WorkerEvent::Idle
        }
      }
    }

    fn cpu_time(&mut self) -> Option<Duration> {
      Some(self.cpu)
    }

    fn kill(&mut self) {
      *self.killed.lock().unwrap() = true;
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
    scripts: VecDeque<(Vec<WorkerEvent>, Then)>,
    spawned_optimized: Vec<bool>,
    killed: Vec<Arc<Mutex<bool>>>,
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
        scripts: scripts.into(),
        spawned_optimized: Vec::new(),
        killed: Vec::new(),
      }
    }

    fn run(&mut self) -> Result<CompilationResult> {
      let Attempts {
        environment,
        limits,
        scripts,
        spawned_optimized,
        killed,
      } = self;
      run_attempts(environment, "plugin.wasm", 1024, limits, |optimize| {
        spawned_optimized.push(optimize);
        let worker_killed = Arc::new(Mutex::new(false));
        killed.push(worker_killed.clone());
        let (events, then) = scripts.pop_front().unwrap();
        Ok(SpawnedWorker::Worker(Box::new(FakeWorker {
          events: events.into(),
          then,
          cpu: Duration::ZERO,
          killed: worker_killed,
        })))
      })
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
    assert!(messages[0].ends_with("s. Killed it and retrying (attempt 2 of 3)."), "{}", messages[0]);
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
}
