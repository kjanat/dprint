use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write;
use std::ops::Deref;
use std::ops::DerefMut;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use dprint_core::async_runtime::FutureExt;
use dprint_core::async_runtime::LocalBoxFuture;
use dprint_core::async_runtime::async_trait;
use dprint_core::async_runtime::future::WeakShared;
use dprint_core::configuration::ConfigKeyMap;
use dprint_core::configuration::GlobalConfiguration;
use dprint_core::plugins::AsyncPluginHandler;
use dprint_core::plugins::CancellationToken;
use dprint_core::plugins::FileMatchingInfo;
use dprint_core::plugins::FormatError;
use dprint_core::plugins::FormatRequest;
use dprint_core::plugins::FormatResult;
use dprint_core::plugins::HostFormatRequest;
use dprint_core::plugins::PluginInfo;
use dprint_core::plugins::PluginResolveConfigurationResult;
use tokio::sync::oneshot;
use tokio::sync::oneshot::Receiver;
use tokio::sync::oneshot::Sender;

use super::configuration::CommandConfiguration;
use super::configuration::Configuration;
use super::configuration::SetupCommand;
use super::executable::resolve_executable;
use super::template::TemplateValues;
use super::template::render_template;

struct ChildKillOnDrop(std::process::Child);

impl Drop for ChildKillOnDrop {
  fn drop(&mut self) {
    // both are no-ops for a child that already exited and was waited on.
    // waiting reaps a killed child so it doesn't linger as a zombie
    if self.0.kill().is_ok() {
      let _ignore = self.0.wait();
    }
  }
}

impl Deref for ChildKillOnDrop {
  type Target = std::process::Child;

  fn deref(&self) -> &Self::Target {
    &self.0
  }
}

impl DerefMut for ChildKillOnDrop {
  fn deref_mut(&mut self) -> &mut Self::Target {
    &mut self.0
  }
}

#[derive(Default)]
pub struct ExecHandler {
  /// Tracks setup commands that have already run so they only run once
  /// for the lifetime of the process, even while formatting in parallel.
  setup_state: SetupState,
}

#[async_trait(?Send)]
impl AsyncPluginHandler for ExecHandler {
  type Configuration = Configuration;

  fn plugin_info(&self) -> PluginInfo {
    // the plugin this was built from, so incremental caches and config
    // tooling see the same plugin as when it was downloaded
    let name = super::EXEC_PLUGIN_NAME.to_string();
    let version = super::EXEC_PLUGIN_VERSION.to_string();
    PluginInfo {
      name: name.clone(),
      version: version.clone(),
      config_key: "exec".to_string(),
      help_url: "https://github.com/dprint/dprint-plugin-exec".to_string(),
      config_schema_url: format!("https://plugins.dprint.dev/dprint/{}/{}/schema.json", name, version),
      update_url: Some(format!("https://plugins.dprint.dev/dprint/{}/latest.json", name)),
    }
  }

  fn license_text(&self) -> String {
    include_str!("LICENSE").to_string()
  }

  async fn resolve_config(&self, config: ConfigKeyMap, global_config: GlobalConfiguration) -> PluginResolveConfigurationResult<Configuration> {
    let result = Configuration::resolve(config, &global_config);
    let config = result.config;
    PluginResolveConfigurationResult {
      file_matching: FileMatchingInfo {
        file_extensions: config
          .commands
          .iter()
          .flat_map(|c| c.file_extensions.iter())
          .map(|s| s.trim_start_matches('.').to_string())
          .collect(),
        file_names: config.commands.iter().flat_map(|c| c.file_names.iter()).map(|s| s.to_string()).collect(),
        additive: false,
      },
      config,
      diagnostics: result.diagnostics,
    }
  }

  async fn format(
    &self,
    request: FormatRequest<Self::Configuration>,
    _format_with_host: impl FnMut(HostFormatRequest) -> LocalBoxFuture<'static, FormatResult> + 'static,
  ) -> FormatResult {
    if request.range.is_some() {
      // we don't support range formatting for this plugin
      return Ok(None);
    }

    format_bytes(request.file_path, request.file_bytes, request.config, request.token.clone(), &self.setup_state).await
  }
}

pub async fn format_bytes(
  file_path: PathBuf,
  original_file_bytes: Vec<u8>,
  config: Arc<Configuration>,
  token: Arc<dyn CancellationToken>,
  setup_state: &SetupState,
) -> FormatResult {
  fn trim_bytes_len(bytes: &[u8]) -> usize {
    let mut start = 0;
    let mut end = bytes.len();

    while start < end && bytes[start].is_ascii_whitespace() {
      start += 1;
    }

    if start == end {
      return 0;
    }

    while end > start && bytes[end - 1].is_ascii_whitespace() {
      end -= 1;
    }

    end.saturating_sub(start)
  }

  let mut file_bytes: Cow<[u8]> = Cow::Borrowed(&original_file_bytes);
  for command in select_commands(&config, &file_path)? {
    // run the command's setup once before formatting with it for the first time
    if let Some(setup_command) = &command.setup_command {
      match setup_state
        .run_once(&command.cwd, setup_command, Duration::from_secs(config.setup_timeout as u64), &token)
        .await?
      {
        SetupRun::Completed => {}
        SetupRun::Cancelled => return Ok(None),
      }
    }
    // the request may have been cancelled while it waited
    if token.is_cancelled() {
      return Ok(None);
    }

    // format here
    let args = maybe_substitute_variables(&file_path, &config, command)?;

    let mut child = ChildKillOnDrop(
      Command::new(setup_state.resolve_executable(&command.executable, &command.cwd))
        .current_dir(&command.cwd)
        .stdout(Stdio::piped())
        .stdin(if command.stdin { Stdio::piped() } else { Stdio::null() })
        .stderr(Stdio::piped())
        .args(args)
        .spawn()
        .map_err(|e| FormatError::new(format!("Cannot start formatter process: {}", e)))?,
    );

    // capturing stdout
    let (out_tx, out_rx) = oneshot::channel();
    let mut handles = Vec::with_capacity(2);
    if let Some(stdout) = child.stdout.take() {
      handles.push(dprint_core::async_runtime::spawn_blocking(|| read_stream_lines(stdout, out_tx)));
    } else {
      let _ = child.kill();
      return Err(FormatError::new("Formatter did not have a handle for stdout"));
    }

    // capturing stderr
    let (err_tx, err_rx) = oneshot::channel();
    if let Some(stderr) = child.stderr.take() {
      handles.push(dprint_core::async_runtime::spawn_blocking(|| read_stream_lines(stderr, err_tx)));
    }

    // write file text into child's stdin. this happens within the timeout
    // because a command that never reads its stdin would block the write
    let stdin_write = if command.stdin {
      let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| FormatError::new("Cannot open the command's stdin. Perhaps you meant to set the command's \"stdin\" configuration to false?"))?;
      let file_bytes = file_bytes.into_owned();
      Some(dprint_core::async_runtime::spawn_blocking(move || match stdin.write_all(&file_bytes) {
        Ok(()) => Ok(()),
        // the command exited without reading all its input, so let its exit
        // status and output decide the result
        Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(err) => Err(FormatError::new(format!("Cannot write into the command's stdin. {}", err))),
      }))
    } else {
      None
    };

    // the child stays owned by this function, so returning early on a timeout
    // or cancellation drops it, which kills the process
    let result_future = async {
      if let Some(stdin_write) = stdin_write {
        stdin_write.await??;
      }
      // the output streams end when the formatter exits
      let handles_future = dprint_core::async_runtime::future::join_all(handles);
      let (output_result, handle_results) = tokio::join!(out_rx, handles_future);
      for handle_result in handle_results {
        handle_result??; // surface any errors capturing
      }
      let output = output_result?;
      let exit_status = wait_for_exit(&mut child, "formatter").await?;
      Ok::<_, FormatError>((output, exit_status))
    };

    tokio::select! {
      _ = token.wait_cancellation() => {
        // return back the original text when cancelled
        return Ok(None);
      }
      _ = tokio::time::sleep(Duration::from_secs(config.timeout as u64)) => {
        return Err(timeout_err(&config));
      }
      result = result_future => {
        let (ok_text, exit_status) = result?;
        file_bytes = Cow::Owned(handle_child_exit_status(ok_text, err_rx, exit_status).await?)
      }
    }
  }

  const MIN_CHARS_TO_EMPTY: usize = 100;
  Ok(if *file_bytes == original_file_bytes {
    None
  } else if trim_bytes_len(&original_file_bytes) > MIN_CHARS_TO_EMPTY && trim_bytes_len(&file_bytes) == 0 {
    // prevent someone formatting all their files to empty files
    return Err(FormatError::new(format!(
      concat!(
        "The original file text was greater than {} characters, but the formatted text was empty. ",
        "Perhaps dprint-plugin-exec has been misconfigured?",
      ),
      MIN_CHARS_TO_EMPTY
    )));
  } else {
    Some(file_bytes.into_owned())
  })
}

fn select_commands<'a>(config: &'a Configuration, file_path: &Path) -> Result<Vec<&'a CommandConfiguration>, FormatError> {
  if !config.is_valid {
    return Err(FormatError::new("Cannot format because the configuration was not valid."));
  }

  let mut binaries = Vec::new();

  for command in &config.commands {
    if let Some(associations) = &command.associations {
      if associations.is_match(file_path) {
        binaries.push(command);
      }
    } else if binaries.is_empty() && command.matches_exts_or_filenames(file_path) {
      binaries.push(command);
      break;
    }
  }

  Ok(binaries)
}

async fn handle_child_exit_status(ok_text: Vec<u8>, err_rx: Receiver<Vec<u8>>, exit_status: ExitStatus) -> Result<Vec<u8>, FormatError> {
  if exit_status.success() {
    return Ok(ok_text);
  }
  Err(FormatError::new(format!(
    "Child process exited with {}: {}",
    exit_status_text(exit_status),
    String::from_utf8_lossy(&err_rx.await.unwrap_or_default())
  )))
}

/// How a process that didn't succeed ended. A process killed by a signal has
/// no exit code.
fn exit_status_text(exit_status: ExitStatus) -> String {
  if let Some(code) = exit_status.code() {
    return format!("code {}", code);
  }
  #[cfg(unix)]
  {
    use std::os::unix::process::ExitStatusExt;
    if let Some(signal) = exit_status.signal() {
      return format!("signal {}", signal);
    }
  }
  "an unknown status".to_string()
}

fn timeout_err(config: &Configuration) -> FormatError {
  FormatError::new(format!("Child process has not returned a result within {} seconds.", config.timeout,))
}

/// Runs each command's `setupCommand` once before formatting with the command,
/// even when many files are formatted in parallel (see
/// https://github.com/dprint/dprint/issues/1023).
///
/// The requests that need a setup command share one attempt at running it.
/// Each waits for it until its own `setupTimeout` passes (counted from when
/// the attempt started) or the request is cancelled, either of which only
/// stops that request from waiting. The attempt runs as long as a request
/// waits for it, and is killed once none does. Then:
///
/// - When it succeeded, that's final.
/// - When it failed (it couldn't start, it exited unsuccessfully, or the last
///   request waiting for it timed out), every request gets that failure
///   until a delay after it, after which the next one runs it again. The
///   delay doubles with each failure in a row (see [`SetupState::retry_delay`]),
///   so however many files are formatted, a failing setup command is run at
///   most once per delay, while a long running process (ex. an editor's
///   language server) recovers from a failure that was transient. After
///   [`MAX_SETUP_ATTEMPTS`] failures in a row, its failure is final.
/// - When every request was cancelled, that isn't the setup command's
///   failure, so the next request runs it again.
#[derive(Clone)]
pub struct SetupState {
  setups: Rc<RefCell<HashMap<SetupKey, SetupEntry>>>,
  /// The delay before running a setup command again after it failed once.
  first_retry_delay: Duration,
  /// Executables found through PATHEXT, keyed by the executable and its cwd.
  /// Only found ones are kept, since a setup command may install one later.
  #[cfg_attr(not(windows), allow(dead_code))]
  executables: Rc<RefCell<HashMap<(String, PathBuf), PathBuf>>>,
}

/// How long after a setup command failed once it's run again.
const FIRST_SETUP_RETRY_DELAY: Duration = Duration::from_secs(10);
/// How many times in a row a setup command may fail before its failure is
/// final, which with the delays between them is after about two and a half
/// minutes.
const MAX_SETUP_ATTEMPTS: u32 = 5;

impl Default for SetupState {
  fn default() -> Self {
    Self {
      setups: Default::default(),
      first_retry_delay: FIRST_SETUP_RETRY_DELAY,
      executables: Default::default(),
    }
  }
}

/// A setup command, by what it runs and where. Its arguments are kept apart
/// so that ex. `tool "a b" c` and `tool a "b c"` are different commands.
#[derive(Clone, PartialEq, Eq, Hash)]
struct SetupKey {
  cwd: PathBuf,
  executable: String,
  args: Vec<String>,
}

enum SetupRun {
  Completed,
  Cancelled,
}

/// Where a setup command is at.
enum SetupEntry {
  /// Being run, for as long as a request waits for it.
  Running(RunningSetup),
  /// Run, which is final.
  Succeeded,
  /// Failed, until it's run again.
  Failed(SetupFailure),
}

struct RunningSetup {
  /// The attempt, which the requests waiting for it own, so that it's dropped
  /// (killing its process) once none does.
  outcome: WeakShared<LocalBoxFuture<'static, SetupOutcome>>,
  started: Instant,
  /// How many times in a row it failed before this attempt.
  failures: u32,
}

struct SetupFailure {
  message: String,
  at: Instant,
  /// How many times in a row it failed, this time included.
  failures: u32,
}

impl SetupFailure {
  fn new(mut message: String, failures: u32) -> Self {
    if failures >= MAX_SETUP_ATTEMPTS {
      message.push_str(&format!(
        "\n\nIt failed {} times in a row, so it isn't run again until the plugin restarts (ex. dprint, or an editor's language server).",
        failures
      ));
    }
    Self {
      message,
      at: Instant::now(),
      failures,
    }
  }

  /// Whether it's final, or else when it's run again.
  fn is_final(&self) -> bool {
    self.failures >= MAX_SETUP_ATTEMPTS
  }
}

#[derive(Clone)]
enum SetupOutcome {
  Succeeded,
  /// Couldn't start or exited unsuccessfully, with why.
  Failed(String),
}

impl SetupOutcome {
  fn into_result(self) -> Result<SetupRun, FormatError> {
    match self {
      SetupOutcome::Succeeded => Ok(SetupRun::Completed),
      SetupOutcome::Failed(message) => Err(FormatError::new(message)),
    }
  }
}

/// What a request waiting for a setup command got.
enum Waited {
  Outcome(SetupOutcome),
  Cancelled,
  TimedOut,
}

impl SetupState {
  #[cfg(all(test, unix))]
  fn with_first_retry_delay(first_retry_delay: Duration) -> Self {
    Self {
      first_retry_delay,
      ..Default::default()
    }
  }

  /// How long after failing `failures` times in a row a setup command is run
  /// again: the first delay, doubled for each failure after the first.
  fn retry_delay(&self, failures: u32) -> Duration {
    let factor = 2u32.checked_pow(failures.saturating_sub(1)).unwrap_or(u32::MAX);
    self.first_retry_delay.saturating_mul(factor)
  }

  fn resolve_executable(&self, executable: &str, cwd: &Path) -> PathBuf {
    if !cfg!(windows) {
      // nothing to resolve
      return PathBuf::from(executable);
    }
    let key = (executable.to_string(), cwd.to_path_buf());
    if let Some(path) = self.executables.borrow().get(&key) {
      return path.clone();
    }
    let path = resolve_executable(executable, cwd);
    if path.as_os_str() != executable {
      self.executables.borrow_mut().insert(key, path.clone());
    }
    path
  }

  /// Runs the setup command, or waits for the attempt at it that's running,
  /// until it's run, the request's `timeout` passes or the request is
  /// cancelled (see [`SetupState`]).
  async fn run_once(&self, cwd: &Path, setup_command: &SetupCommand, timeout: Duration, token: &Arc<dyn CancellationToken>) -> Result<SetupRun, FormatError> {
    if token.is_cancelled() {
      return Ok(SetupRun::Cancelled);
    }
    // the cwd is part of the key because the same command run in different
    // directories may produce different results
    let key = SetupKey {
      cwd: cwd.to_path_buf(),
      executable: setup_command.executable.clone(),
      args: setup_command.args.clone(),
    };
    let (mut attempt, started) = {
      let mut setups = self.setups.borrow_mut();
      let now = Instant::now();
      let (running, failures) = match setups.get(&key) {
        Some(SetupEntry::Succeeded) => return Ok(SetupRun::Completed),
        Some(SetupEntry::Failed(failure)) => {
          let retry_at = failure.at.checked_add(self.retry_delay(failure.failures));
          if failure.is_final() || retry_at.is_none_or(|retry_at| now < retry_at) {
            return Err(FormatError::new(failure.message.clone()));
          }
          (None, failure.failures)
        }
        Some(SetupEntry::Running(running)) => (running.outcome.upgrade().map(|attempt| (attempt, running.started)), running.failures),
        None => (None, 0),
      };
      match running {
        Some(running) => running,
        // not run yet, it's time to run it again, or every request that
        // waited for it stopped waiting
        None => {
          let attempt = run_setup_command(SetupAttempt {
            setups: self.setups.clone(),
            key: key.clone(),
            cwd: cwd.to_path_buf(),
            executable: self.resolve_executable(&setup_command.executable, cwd),
            setup_command: setup_command.clone(),
            failures,
          })
          .boxed_local()
          .shared();
          setups.insert(
            key.clone(),
            SetupEntry::Running(RunningSetup {
              outcome: attempt.downgrade().expect("not polled yet"),
              started: now,
              failures,
            }),
          );
          (attempt, now)
        }
      }
    };
    // a request that's cancelled or out of time stops waiting, which leaves
    // the attempt to the others, and drops it when there are none
    let waited = tokio::select! {
      biased;
      outcome = &mut attempt => Waited::Outcome(outcome),
      _ = token.wait_cancellation() => Waited::Cancelled,
      _ = tokio::time::sleep_until((started + timeout).into()) => Waited::TimedOut,
    };
    match waited {
      Waited::Outcome(outcome) => outcome.into_result(),
      Waited::Cancelled => Ok(SetupRun::Cancelled),
      Waited::TimedOut => {
        let message = format!(
          "Setup command '{}' did not finish within {} seconds. Increase the \"setupTimeout\" configuration if it needs longer.",
          setup_command.executable,
          timeout.as_secs(),
        );
        // no request waits for it any longer, so this is how it ended
        if attempt.strong_count() == Some(1) {
          return Err(FormatError::new(self.record_failure(&key, message)));
        }
        Err(FormatError::new(message))
      }
    }
  }

  /// Records that the setup command failed, and gives what each request gets
  /// for it.
  fn record_failure(&self, key: &SetupKey, message: String) -> String {
    let mut setups = self.setups.borrow_mut();
    let failures = match setups.get(key) {
      Some(SetupEntry::Running(running)) => running.failures,
      _ => 0,
    };
    let failure = SetupFailure::new(message, failures + 1);
    let message = failure.message.clone();
    setups.insert(key.clone(), SetupEntry::Failed(failure));
    message
  }
}

/// An attempt at running a setup command.
struct SetupAttempt {
  setups: Rc<RefCell<HashMap<SetupKey, SetupEntry>>>,
  key: SetupKey,
  cwd: PathBuf,
  executable: PathBuf,
  setup_command: SetupCommand,
  /// How many times in a row it failed before this attempt.
  failures: u32,
}

/// Runs a setup command, and records its outcome before any request sees it
/// (see [`SetupState`]).
async fn run_setup_command(run: SetupAttempt) -> SetupOutcome {
  let (entry, outcome) = match run_setup_command_process(&run).await {
    SetupOutcome::Succeeded => (SetupEntry::Succeeded, SetupOutcome::Succeeded),
    SetupOutcome::Failed(message) => {
      let failure = SetupFailure::new(message, run.failures + 1);
      let message = failure.message.clone();
      (SetupEntry::Failed(failure), SetupOutcome::Failed(message))
    }
  };
  run.setups.borrow_mut().insert(run.key.clone(), entry);
  outcome
}

/// Runs the setup command's process until it exits, unless it's dropped first
/// (once no request waits for it), which kills it.
async fn run_setup_command_process(run: &SetupAttempt) -> SetupOutcome {
  let SetupAttempt {
    cwd,
    executable,
    setup_command,
    ..
  } = run;
  let mut child = match Command::new(executable)
    .current_dir(cwd)
    .stdin(Stdio::null())
    // a plugin must not write to stdout (it's the protocol channel)
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .args(&setup_command.args)
    .spawn()
  {
    Ok(child) => ChildKillOnDrop(child),
    Err(err) => return SetupOutcome::Failed(format!("Cannot start setup command process: {}", err)),
  };

  // capture stderr to surface it if the command fails
  let (err_tx, err_rx) = oneshot::channel();
  let mut handles = Vec::with_capacity(1);
  if let Some(stderr) = child.stderr.take() {
    handles.push(dprint_core::async_runtime::spawn_blocking(|| read_stream_lines(stderr, err_tx)));
  }

  let result = async {
    let handles_future = dprint_core::async_runtime::future::join_all(handles);
    let handle_results = handles_future.await;
    for handle_result in handle_results {
      handle_result??; // surface any errors capturing
    }
    wait_for_exit(&mut child, "setup command").await
  }
  .await;
  match result {
    Ok(exit_status) if exit_status.success() => SetupOutcome::Succeeded,
    Ok(exit_status) => SetupOutcome::Failed(format!(
      "Setup command '{}' exited with {}: {}",
      setup_command.executable,
      exit_status_text(exit_status),
      String::from_utf8_lossy(&err_rx.await.unwrap_or_default())
    )),
    Err(err) => SetupOutcome::Failed(err.to_string()),
  }
}

/// Waits for a child that has closed its output streams to exit. It's polled
/// rather than waited on from another thread so that the child stays owned by
/// the caller, whose drop kills it.
async fn wait_for_exit(child: &mut ChildKillOnDrop, description: &str) -> Result<ExitStatus, FormatError> {
  let mut delay = Duration::from_millis(1);
  loop {
    match child.try_wait() {
      Ok(Some(status)) => return Ok(status),
      Ok(None) => {
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_millis(50));
      }
      Err(err) => {
        return Err(FormatError::new(format!("Error while waiting for {} to complete: {}", description, err)));
      }
    }
  }
}

fn read_stream_lines<R>(mut readable: R, sender: Sender<Vec<u8>>) -> Result<(), FormatError>
where
  R: std::io::Read + Unpin,
{
  let mut bytes = Vec::new();
  readable.read_to_end(&mut bytes)?;
  let _ignore = sender.send(bytes); // ignore error as that means the other end is closed
  Ok(())
}

fn maybe_substitute_variables(file_path: &Path, config: &Configuration, command: &CommandConfiguration) -> Result<Vec<String>, FormatError> {
  let values = TemplateValues {
    file_path,
    line_width: config.line_width,
    use_tabs: config.use_tabs,
    indent_width: config.indent_width,
    cwd: &command.cwd,
    timeout: config.timeout,
  };
  // the configuration only has valid templates, but say what's wrong if not
  command
    .args
    .iter()
    .map(|arg| render_template(arg, &values).map_err(|err| FormatError::new(format!("Cannot substitute the variables in argument '{}': {}", arg, err))))
    .collect()
}

// the commands they run are unix ones
#[cfg(all(test, unix))]
#[allow(clippy::disallowed_methods)] // tests run real commands against real files
mod test {
  use std::path::PathBuf;
  use std::sync::Arc;
  use std::time::Duration;
  use std::time::Instant;

  use dprint_core::configuration::ConfigKeyMap;
  use dprint_core::plugins::CancellationToken;
  use dprint_core::plugins::NullCancellationToken;

  use super::SetupState;
  use super::format_bytes;
  use crate::plugins::implementations::builtin_exec::configuration::Configuration;

  fn resolve(config: serde_json::Value) -> Arc<Configuration> {
    let config: ConfigKeyMap = serde_json::from_value(config).unwrap();
    let result = Configuration::resolve(config, &Default::default());
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    Arc::new(result.config)
  }

  async fn format(config: &Arc<Configuration>, text: &str, setup_state: &SetupState) -> Result<Option<String>, String> {
    format_with_token(config, text, setup_state, Arc::new(NullCancellationToken)).await
  }

  async fn format_with_token(
    config: &Arc<Configuration>,
    text: &str,
    setup_state: &SetupState,
    token: Arc<dyn CancellationToken>,
  ) -> Result<Option<String>, String> {
    format_bytes(PathBuf::from("file.txt"), text.as_bytes().to_vec(), config.clone(), token, setup_state)
      .await
      .map(|bytes| bytes.map(|bytes| String::from_utf8(bytes).unwrap()))
      .map_err(|err| err.to_string())
  }

  #[tokio::test]
  async fn formats_with_stdin_and_stdout() {
    let config = resolve(serde_json::json!({ "commands": [{ "command": "tr a-z A-Z", "exts": ["txt"] }] }));
    assert_eq!(format(&config, "hello\n", &SetupState::default()).await, Ok(Some("HELLO\n".to_string())));
  }

  #[tokio::test]
  async fn should_error_output_empty_file() {
    // `true` exits without reading its input, which used to fail writing the
    // input (a broken pipe) whenever it exited first
    let config = resolve(serde_json::json!({ "commands": [{ "command": "true", "exts": ["txt"] }] }));
    assert_eq!(
      format(&config, &"1".repeat(101), &SetupState::default()).await,
      Err(
        concat!(
          "The original file text was greater than 100 characters, ",
          "but the formatted text was empty. ",
          "Perhaps dprint-plugin-exec has been misconfigured?"
        )
        .to_string()
      )
    );
  }

  #[tokio::test]
  async fn passes_variables_to_the_command_as_they_are() {
    // not escaped for HTML, like Handlebars did
    let file_path = r#"/dir/a&b <"c"> 'd'.txt"#;
    let config = resolve(serde_json::json!({ "commands": [{ "command": "printf %s {{file_path}}", "exts": ["txt"] }] }));
    let formatted = format_bytes(
      PathBuf::from(file_path),
      b"text".to_vec(),
      config,
      Arc::new(NullCancellationToken),
      &SetupState::default(),
    )
    .await
    .unwrap();
    assert_eq!(formatted, Some(file_path.as_bytes().to_vec()));
  }

  #[tokio::test]
  async fn errors_for_a_formatter_killed_by_a_signal() {
    let config = resolve(serde_json::json!({ "commands": [{ "command": "sh -c \"kill -TERM $$\"", "exts": ["txt"] }] }));
    assert_eq!(
      format(&config, "text", &SetupState::default()).await,
      Err("Child process exited with signal 15: ".to_string())
    );
  }

  #[tokio::test]
  async fn runs_setup_command_once_across_formats() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("marker.txt");
    let config = resolve(serde_json::json!({
      "commands": [{
        "command": "cat",
        "setupCommand": format!("sh -c \"printf x >> {}\"", marker.display()),
        "exts": ["txt"]
      }]
    }));
    let setup_state = SetupState::default();
    for _ in 0..2 {
      assert_eq!(format(&config, "text", &setup_state).await, Ok(None));
    }
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "x");
  }

  #[tokio::test]
  async fn runs_setup_commands_whose_arguments_only_split_differently() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("marker.txt");
    // the same text when the arguments are joined with spaces
    let setup_command = |args: &str| format!("sh -c \"echo ran >> {}\" {}", marker.display(), args);
    let config = resolve(serde_json::json!({
      "commands": [
        { "command": "cat", "setupCommand": setup_command("\"a b\" c"), "associations": "**/*.txt" },
        { "command": "cat", "setupCommand": setup_command("a \"b c\""), "associations": "**/*.txt" }
      ]
    }));
    assert_eq!(format(&config, "text", &SetupState::default()).await, Ok(None));
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "ran\nran\n");
  }

  /// Gets whether the process running `sleep <seconds>` is still alive.
  fn sleep_is_running(seconds: &str) -> bool {
    let output = std::process::Command::new("ps").args(["-eo", "args"]).output().unwrap();
    String::from_utf8_lossy(&output.stdout)
      .lines()
      .any(|line| line.trim() == format!("sleep {}", seconds))
  }

  #[tokio::test]
  async fn kills_a_formatter_that_times_out() {
    // a unique duration so this test finds its own process
    let config = resolve(serde_json::json!({ "timeout": 1, "commands": [{ "command": "sleep 31.7", "exts": ["txt"] }] }));
    let start = Instant::now();
    assert_eq!(
      format(&config, "text", &SetupState::default()).await,
      Err("Child process has not returned a result within 1 seconds.".to_string())
    );
    assert!(start.elapsed() < Duration::from_secs(5));
    assert!(!sleep_is_running("31.7"), "the formatter should have been killed");
  }

  #[tokio::test]
  async fn times_out_a_formatter_that_never_reads_its_stdin() {
    // more than a pipe buffer, so writing it blocks until the formatter reads
    let config = resolve(serde_json::json!({ "timeout": 1, "commands": [{ "command": "sleep 32.7", "exts": ["txt"] }] }));
    let text = "a".repeat(1024 * 1024);
    let start = Instant::now();
    assert!(format(&config, &text, &SetupState::default()).await.is_err());
    assert!(start.elapsed() < Duration::from_secs(5));
    assert!(!sleep_is_running("32.7"), "the formatter should have been killed");
  }

  #[tokio::test]
  async fn kills_a_setup_command_that_times_out_and_does_not_rerun_it() {
    let config = resolve(serde_json::json!({
      "setupTimeout": 1,
      "commands": [{ "command": "cat", "setupCommand": "sleep 33.7", "exts": ["txt"] }]
    }));
    let setup_state = SetupState::default();
    let expected = Err("Setup command 'sleep' did not finish within 1 seconds. Increase the \"setupTimeout\" configuration if it needs longer.".to_string());
    let start = Instant::now();
    assert_eq!(format(&config, "text", &setup_state).await, expected);
    assert!(!sleep_is_running("33.7"), "the setup command should have been killed");
    // the next file fails right away instead of waiting out the timeout again
    assert_eq!(format(&config, "text", &setup_state).await, expected);
    assert!(start.elapsed() < Duration::from_secs(3));
  }

  /// A setup command that records each start of it in `dir`, as the id of the
  /// process that then runs for `seconds`.
  fn counting_setup_command(dir: &std::path::Path, seconds: u32) -> String {
    format!("sh -c \"echo $$ >> {}; exec sleep {}\"", dir.join("starts").display(), seconds)
  }

  /// The process ids of the setup command's starts, in order.
  fn setup_starts(dir: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("starts"))
      .unwrap_or_default()
      .lines()
      .map(ToOwned::to_owned)
      .collect()
  }

  fn is_running(process_id: &str) -> bool {
    std::process::Command::new("kill")
      .args(["-0", process_id])
      .stderr(std::process::Stdio::null())
      .status()
      .unwrap()
      .success()
  }

  /// Runs the future on a runtime of its own, then checks that nothing it
  /// started (ex. a reader of a process's output) keeps running.
  fn run_leaving_nothing_running<T>(future: impl std::future::Future<Output = T>) -> T {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let result = runtime.block_on(future);
    let start = Instant::now();
    runtime.shutdown_timeout(Duration::from_secs(10));
    assert!(start.elapsed() < Duration::from_secs(5), "something it started was left running");
    result
  }

  #[test]
  fn times_out_a_setup_command_once_for_every_request_waiting_for_it() {
    let dir = tempfile::tempdir().unwrap();
    let config = resolve(serde_json::json!({
      "setupTimeout": 1,
      "commands": [{ "command": "cat", "setupCommand": counting_setup_command(dir.path(), 30), "exts": ["txt"] }]
    }));
    let expected = Err("Setup command 'sh' did not finish within 1 seconds. Increase the \"setupTimeout\" configuration if it needs longer.".to_string());
    run_leaving_nothing_running(async {
      let setup_state = SetupState::default();
      // requests that all wait for the setup command
      let barrier = tokio::sync::Barrier::new(3);
      let start = Instant::now();
      let results = dprint_core::async_runtime::future::join_all((0..3).map(|_| async {
        barrier.wait().await;
        format(&config, "text", &setup_state).await
      }))
      .await;
      // all get the outcome of one attempt, rather than each waiting out one
      assert_eq!(results, vec![expected.clone(); 3]);
      assert!(start.elapsed() < Duration::from_secs(3), "{:?}", start.elapsed());
      // and so do later files, without running it again
      assert_eq!(format(&config, "text", &setup_state).await, expected);
    });
    let starts = setup_starts(dir.path());
    assert_eq!(starts.len(), 1);
    assert!(!is_running(&starts[0]));
  }

  /// How a request went, and how long after the start it returned.
  type RequestResult = (Result<Option<String>, String>, Duration);

  /// Formats with requests that wait for a setup command that takes a second,
  /// cancelling the ones that say so after 300ms. Gives what they returned and
  /// the starts of the setup command.
  fn format_cancelling(cancelled: &[bool]) -> (Vec<RequestResult>, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let config = resolve(serde_json::json!({
      "commands": [{ "command": "tr a-z A-Z", "setupCommand": counting_setup_command(dir.path(), 1), "exts": ["txt"] }]
    }));
    let results = run_leaving_nothing_running(async {
      let setup_state = SetupState::default();
      let tokens = cancelled.iter().map(|_| tokio_util::sync::CancellationToken::new()).collect::<Vec<_>>();
      let start = Instant::now();
      let requests = dprint_core::async_runtime::future::join_all(tokens.iter().map(|token| {
        let token = Arc::new(token.clone());
        let (config, setup_state) = (&config, &setup_state);
        async move { (format_with_token(config, "text", setup_state, token).await, start.elapsed()) }
      }));
      let cancel = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        for (token, cancelled) in tokens.iter().zip(cancelled) {
          if *cancelled {
            token.cancel();
          }
        }
      };
      tokio::join!(requests, cancel).0
    });
    (results, setup_starts(dir.path()))
  }

  #[track_caller]
  fn assert_returned_once_cancelled(result: &RequestResult) {
    assert_eq!(result.0, Ok(None));
    assert!(result.1 < Duration::from_millis(800), "{:?}", result.1);
  }

  #[track_caller]
  fn assert_formatted_after_setup(result: &RequestResult) {
    assert_eq!(result.0, Ok(Some("TEXT".to_string())));
    assert!(result.1 >= Duration::from_secs(1), "{:?}", result.1);
  }

  #[test]
  fn a_cancelled_request_stops_waiting_for_a_setup_command_another_waits_for() {
    let (results, starts) = format_cancelling(&[false, true]);
    assert_returned_once_cancelled(&results[1]);
    assert_formatted_after_setup(&results[0]);
    assert_eq!(starts.len(), 1);
  }

  #[test]
  fn the_request_that_started_a_setup_command_can_stop_waiting_for_it() {
    // the setup command keeps running for the other request
    let (results, starts) = format_cancelling(&[true, false]);
    assert_returned_once_cancelled(&results[0]);
    assert_formatted_after_setup(&results[1]);
    assert_eq!(starts.len(), 1);
  }

  #[test]
  fn kills_a_setup_command_no_request_waits_for() {
    let (results, starts) = format_cancelling(&[true, true]);
    assert_returned_once_cancelled(&results[0]);
    assert_returned_once_cancelled(&results[1]);
    assert_eq!(starts.len(), 1);
    assert!(!is_running(&starts[0]));
  }

  #[test]
  fn runs_a_setup_command_again_once_no_request_waited_for_it() {
    let dir = tempfile::tempdir().unwrap();
    let config = resolve(serde_json::json!({
      "commands": [{ "command": "tr a-z A-Z", "setupCommand": counting_setup_command(dir.path(), 1), "exts": ["txt"] }]
    }));
    run_leaving_nothing_running(async {
      let setup_state = SetupState::default();
      let token = tokio_util::sync::CancellationToken::new();
      let cancel = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        token.cancel();
      };
      let (result, ()) = tokio::join!(format_with_token(&config, "text", &setup_state, Arc::new(token.clone())), cancel);
      assert_eq!(result, Ok(None));
      // it has no outcome, so it's run for the next request
      assert_eq!(format(&config, "text", &setup_state).await, Ok(Some("TEXT".to_string())));
      // which is final
      assert_eq!(format(&config, "text", &setup_state).await, Ok(Some("TEXT".to_string())));
    });
    assert_eq!(setup_starts(dir.path()).len(), 2);
  }

  #[test]
  fn does_nothing_for_a_request_cancelled_before_it_starts() {
    let dir = tempfile::tempdir().unwrap();
    let formatted = dir.path().join("formatted");
    let config = resolve(serde_json::json!({
      "commands": [{
        "command": format!("sh -c \"echo x >> {}; cat\"", formatted.display()),
        "setupCommand": counting_setup_command(dir.path(), 0),
        "exts": ["txt"],
      }]
    }));
    run_leaving_nothing_running(async {
      let token = tokio_util::sync::CancellationToken::new();
      token.cancel();
      assert_eq!(format_with_token(&config, "text", &SetupState::default(), Arc::new(token)).await, Ok(None));
    });
    assert_eq!(setup_starts(dir.path()).len(), 0);
    assert!(!formatted.exists());
  }

  /// A configuration whose setup command takes the seconds to finish, for
  /// requests that wait for it for `setup_timeout` seconds.
  fn config_with_setup_timeout(dir: &std::path::Path, setup_timeout: u32, setup_seconds: u32) -> Arc<Configuration> {
    resolve(serde_json::json!({
      "setupTimeout": setup_timeout,
      "commands": [{ "command": "tr a-z A-Z", "setupCommand": counting_setup_command(dir, setup_seconds), "exts": ["txt"] }]
    }))
  }

  #[test]
  fn stops_waiting_for_a_setup_command_at_each_requests_own_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let (short, long) = (config_with_setup_timeout(dir.path(), 1, 2), config_with_setup_timeout(dir.path(), 5, 2));
    run_leaving_nothing_running(async {
      let setup_state = SetupState::default();
      let start = Instant::now();
      let timed = |config| {
        let setup_state = &setup_state;
        async move { (format(config, "text", setup_state).await, start.elapsed()) }
      };
      let (short_result, long_result) = tokio::join!(timed(&short), timed(&long));
      // the one with the short timeout gives up on it after its own timeout...
      assert_eq!(
        short_result.0,
        Err("Setup command 'sh' did not finish within 1 seconds. Increase the \"setupTimeout\" configuration if it needs longer.".to_string())
      );
      assert!(
        short_result.1 >= Duration::from_secs(1) && short_result.1 < Duration::from_millis(1800),
        "{:?}",
        short_result.1
      );
      // ...while it keeps running for the other, which it finishes for
      assert_eq!(long_result.0, Ok(Some("TEXT".to_string())));
      assert!(long_result.1 >= Duration::from_secs(2), "{:?}", long_result.1);
      // and which is final for every request
      assert_eq!(format(&short, "text", &setup_state).await, Ok(Some("TEXT".to_string())));
    });
    assert_eq!(setup_starts(dir.path()).len(), 1);
  }

  #[test]
  fn fails_a_setup_command_once_the_last_request_waiting_for_it_times_out() {
    // the request that would wait longer is cancelled, so the other one is
    // the last that waits for it
    let dir = tempfile::tempdir().unwrap();
    let (short, long) = (config_with_setup_timeout(dir.path(), 1, 30), config_with_setup_timeout(dir.path(), 5, 30));
    let expected = Err("Setup command 'sh' did not finish within 1 seconds. Increase the \"setupTimeout\" configuration if it needs longer.".to_string());
    run_leaving_nothing_running(async {
      let setup_state = SetupState::default();
      let token = tokio_util::sync::CancellationToken::new();
      let cancel = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        token.cancel();
      };
      let (short_result, long_result, ()) = tokio::join!(
        format(&short, "text", &setup_state),
        format_with_token(&long, "text", &setup_state, Arc::new(token.clone())),
        cancel
      );
      assert_eq!(long_result, Ok(None));
      assert_eq!(short_result, expected);
      // which is its outcome, rather than running it again for the next file
      let start = Instant::now();
      assert_eq!(format(&long, "text", &setup_state).await, expected);
      assert!(start.elapsed() < Duration::from_millis(500));
    });
    let starts = setup_starts(dir.path());
    assert_eq!(starts.len(), 1);
    assert!(!is_running(&starts[0]));
  }

  #[test]
  fn runs_a_failed_setup_command_again_after_a_delay() {
    let dir = tempfile::tempdir().unwrap();
    let ok = dir.path().join("ok");
    // fails until there's a file called "ok"
    let config = resolve(serde_json::json!({
      "commands": [{
        "command": "tr a-z A-Z",
        "setupCommand": format!("sh -c \"echo $$ >> {}; test -f {}\"", dir.path().join("starts").display(), ok.display()),
        "exts": ["txt"]
      }]
    }));
    let failed = Err("Setup command 'sh' exited with code 1: ".to_string());
    run_leaving_nothing_running(async {
      let setup_state = SetupState::with_first_retry_delay(Duration::from_millis(500));
      let format_files = |count: usize| dprint_core::async_runtime::future::join_all((0..count).map(|_| format(&config, "text", &setup_state)));
      // however many files there are, it's run once...
      assert_eq!(format_files(5).await, vec![failed.clone(); 5]);
      assert_eq!(format_files(5).await, vec![failed.clone(); 5]);
      assert_eq!(setup_starts(dir.path()).len(), 1);
      // ...until the delay after it failed, even when it'd succeed now
      std::fs::write(&ok, "").unwrap();
      assert_eq!(format_files(5).await, vec![failed.clone(); 5]);
      assert_eq!(setup_starts(dir.path()).len(), 1);
      // after which it's run again, once
      tokio::time::sleep(Duration::from_millis(600)).await;
      assert_eq!(format_files(5).await, vec![Ok(Some("TEXT".to_string())); 5]);
      // which is final
      assert_eq!(format_files(5).await, vec![Ok(Some("TEXT".to_string())); 5]);
    });
    assert_eq!(setup_starts(dir.path()).len(), 2);
  }

  #[test]
  fn waits_longer_to_run_a_setup_command_again_the_more_it_failed() {
    let setup_state = SetupState::default();
    let delays = (1..super::MAX_SETUP_ATTEMPTS)
      .map(|failures| setup_state.retry_delay(failures).as_secs())
      .collect::<Vec<_>>();
    assert_eq!(delays, vec![10, 20, 40, 80]);
  }

  #[test]
  fn stops_running_a_setup_command_that_keeps_failing() {
    let dir = tempfile::tempdir().unwrap();
    let config = resolve(serde_json::json!({
      "commands": [{
        "command": "tr a-z A-Z",
        "setupCommand": format!("sh -c \"echo $$ >> {}; exit 1\"", dir.path().join("starts").display()),
        "exts": ["txt"]
      }]
    }));
    let failed = "Setup command 'sh' exited with code 1: ";
    let last = format!(
      "{}\n\nIt failed 5 times in a row, so it isn't run again until the plugin restarts (ex. dprint, or an editor's language server).",
      failed
    );
    run_leaving_nothing_running(async {
      let first_retry_delay = Duration::from_millis(50);
      let setup_state = SetupState::with_first_retry_delay(first_retry_delay);
      for attempt in 1..=super::MAX_SETUP_ATTEMPTS {
        let expected = if attempt == super::MAX_SETUP_ATTEMPTS { &last } else { failed };
        assert_eq!(format(&config, "text", &setup_state).await, Err(expected.to_string()), "{}", attempt);
        assert_eq!(setup_starts(dir.path()).len(), attempt as usize);
        tokio::time::sleep(setup_state.retry_delay(attempt).min(Duration::from_secs(1)) + first_retry_delay).await;
      }
      // after which it fails for good
      tokio::time::sleep(Duration::from_secs(1)).await;
      assert_eq!(format(&config, "text", &setup_state).await, Err(last.clone()));
    });
    assert_eq!(setup_starts(dir.path()).len(), super::MAX_SETUP_ATTEMPTS as usize);
  }

  #[test]
  fn doesnt_count_a_setup_command_no_request_waited_for_as_failing() {
    let dir = tempfile::tempdir().unwrap();
    let config = resolve(serde_json::json!({
      "commands": [{ "command": "tr a-z A-Z", "setupCommand": counting_setup_command(dir.path(), 1), "exts": ["txt"] }]
    }));
    let attempts = super::MAX_SETUP_ATTEMPTS as usize + 1;
    run_leaving_nothing_running(async {
      let setup_state = SetupState::default();
      // more times than it may fail
      for _ in 0..attempts {
        let token = tokio_util::sync::CancellationToken::new();
        let cancel = async {
          tokio::time::sleep(Duration::from_millis(100)).await;
          token.cancel();
        };
        let (result, ()) = tokio::join!(format_with_token(&config, "text", &setup_state, Arc::new(token.clone())), cancel);
        assert_eq!(result, Ok(None));
      }
      // it still runs, and right away rather than after a delay
      assert_eq!(format(&config, "text", &setup_state).await, Ok(Some("TEXT".to_string())));
    });
    assert_eq!(setup_starts(dir.path()).len(), attempts + 1);
  }
}
