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

use dprint_core::async_runtime::LocalBoxFuture;
use dprint_core::async_runtime::async_trait;
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
use handlebars::Handlebars;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::OnceCell;
use tokio::sync::oneshot;
use tokio::sync::oneshot::Receiver;
use tokio::sync::oneshot::Sender;

use super::configuration::CommandConfiguration;
use super::configuration::Configuration;
use super::configuration::SetupCommand;

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

    // format here
    let args = maybe_substitute_variables(&file_path, &config, command);

    let mut child = ChildKillOnDrop(
      Command::new(&command.executable)
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
    "Child process exited with code {}: {}",
    exit_status.code().unwrap(),
    String::from_utf8_lossy(&err_rx.await.expect("Could not propagate error message from child process"))
  )))
}

fn timeout_err(config: &Configuration) -> FormatError {
  FormatError::new(format!("Child process has not returned a result within {} seconds.", config.timeout,))
}

/// Remembers which setup commands have already been run so that a command's
/// `setupCommand` only runs a single time, even when many files are being
/// formatted in parallel (see https://github.com/dprint/dprint/issues/1023).
#[derive(Default, Clone)]
pub struct SetupState {
  cells: Rc<RefCell<HashMap<String, Rc<OnceCell<()>>>>>,
  /// Setup commands that timed out, so they aren't retried for every file.
  timed_out: Rc<RefCell<HashMap<String, String>>>,
}

enum SetupRun {
  Completed,
  Cancelled,
}

enum SetupInitError {
  Cancelled,
  Failed(FormatError),
  TimedOut(String),
}

impl SetupState {
  async fn run_once(&self, cwd: &Path, setup_command: &SetupCommand, timeout: Duration, token: &Arc<dyn CancellationToken>) -> Result<SetupRun, FormatError> {
    // the cwd is part of the key because the same command run in different
    // directories may produce different results
    let key = format!("{}\0{} {}", cwd.display(), setup_command.executable, setup_command.args.join(" "));
    if let Some(message) = self.timed_out.borrow().get(&key) {
      return Err(FormatError::new(message.clone()));
    }
    let cell = {
      let mut cells = self.cells.borrow_mut();
      cells.entry(key.clone()).or_default().clone()
    };
    // get_or_try_init ensures only one caller runs the setup at a time and that
    // the others wait for it to finish; a failure is not cached so it can be
    // retried by the next file rather than poisoning all formatting
    match cell.get_or_try_init(|| run_setup_command(cwd, setup_command, timeout, token)).await {
      Ok(()) => Ok(SetupRun::Completed),
      Err(SetupInitError::Cancelled) => Ok(SetupRun::Cancelled),
      Err(SetupInitError::Failed(err)) => Err(err),
      Err(SetupInitError::TimedOut(message)) => {
        self.timed_out.borrow_mut().insert(key, message.clone());
        Err(FormatError::new(message))
      }
    }
  }
}

async fn run_setup_command(cwd: &Path, setup_command: &SetupCommand, timeout: Duration, token: &Arc<dyn CancellationToken>) -> Result<(), SetupInitError> {
  let mut child = ChildKillOnDrop(
    Command::new(&setup_command.executable)
      .current_dir(cwd)
      .stdin(Stdio::null())
      // a plugin must not write to stdout (it's the protocol channel)
      .stdout(Stdio::null())
      .stderr(Stdio::piped())
      .args(&setup_command.args)
      .spawn()
      .map_err(|e| SetupInitError::Failed(FormatError::new(format!("Cannot start setup command process: {}", e))))?,
  );

  // capture stderr to surface it if the command fails
  let (err_tx, err_rx) = oneshot::channel();
  let mut handles = Vec::with_capacity(1);
  if let Some(stderr) = child.stderr.take() {
    handles.push(dprint_core::async_runtime::spawn_blocking(|| read_stream_lines(stderr, err_tx)));
  }

  // the child stays owned by this function, so returning on a timeout or
  // cancellation kills it
  let result_future = async {
    let handles_future = dprint_core::async_runtime::future::join_all(handles);
    let handle_results = handles_future.await;
    for handle_result in handle_results {
      handle_result??; // surface any errors capturing
    }
    wait_for_exit(&mut child, "setup command").await
  };

  tokio::select! {
    _ = token.wait_cancellation() => Err(SetupInitError::Cancelled),
    _ = tokio::time::sleep(timeout) => Err(SetupInitError::TimedOut(format!(
      "Setup command '{}' did not finish within {} seconds, so it was killed. Increase the \"setupTimeout\" configuration if it needs longer.",
      setup_command.executable,
      timeout.as_secs(),
    ))),
    result = result_future => match result {
      Ok(exit_status) if exit_status.success() => Ok(()),
      Ok(exit_status) => Err(SetupInitError::Failed(FormatError::new(format!(
        "Setup command '{}' exited with code {}: {}",
        setup_command.executable,
        exit_status
          .code()
          .map(|code| code.to_string())
          .unwrap_or_else(|| "unknown".to_string()),
        String::from_utf8_lossy(&err_rx.await.unwrap_or_default())
      )))),
      Err(err) => Err(SetupInitError::Failed(err)),
    }
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

fn maybe_substitute_variables(file_path: &Path, config: &Configuration, command: &CommandConfiguration) -> Vec<String> {
  let mut handlebars = Handlebars::new();
  handlebars.set_strict_mode(true);

  #[derive(Clone, Serialize, Deserialize)]
  struct TemplateVariables {
    file_path: String,
    line_width: u32,
    use_tabs: bool,
    indent_width: u8,
    cwd: String,
    timeout: u32,
  }

  let vars = TemplateVariables {
    file_path: file_path.to_string_lossy().to_string(),
    line_width: config.line_width,
    use_tabs: config.use_tabs,
    indent_width: config.indent_width,
    cwd: command.cwd.to_string_lossy().to_string(),
    timeout: config.timeout,
  };

  let mut c_args = vec![];
  for arg in &command.args {
    let formatted = handlebars
      .render_template(arg, &vars)
      .unwrap_or_else(|err| panic!("Cannot format: {}\n\n{}", arg, err));
    c_args.push(formatted);
  }
  c_args
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // tests run real commands against real files
mod test {
  use std::path::PathBuf;
  use std::sync::Arc;
  use std::time::Duration;
  use std::time::Instant;

  use dprint_core::configuration::ConfigKeyMap;
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
    format_bytes(
      PathBuf::from("file.txt"),
      text.as_bytes().to_vec(),
      config.clone(),
      Arc::new(NullCancellationToken),
      setup_state,
    )
    .await
    .map(|bytes| bytes.map(|bytes| String::from_utf8(bytes).unwrap()))
    .map_err(|err| err.to_string())
  }

  #[cfg(unix)]
  #[tokio::test]
  async fn formats_with_stdin_and_stdout() {
    let config = resolve(serde_json::json!({ "commands": [{ "command": "tr a-z A-Z", "exts": ["txt"] }] }));
    assert_eq!(format(&config, "hello\n", &SetupState::default()).await, Ok(Some("HELLO\n".to_string())));
  }

  #[cfg(unix)]
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

  #[cfg(unix)]
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

  /// Gets whether the process running `sleep <seconds>` is still alive.
  #[cfg(unix)]
  fn sleep_is_running(seconds: &str) -> bool {
    let output = std::process::Command::new("ps").args(["-eo", "args"]).output().unwrap();
    String::from_utf8_lossy(&output.stdout)
      .lines()
      .any(|line| line.trim() == format!("sleep {}", seconds))
  }

  #[cfg(unix)]
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

  #[cfg(unix)]
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

  #[cfg(unix)]
  #[tokio::test]
  async fn kills_a_setup_command_that_times_out_and_does_not_rerun_it() {
    let config = resolve(serde_json::json!({
      "setupTimeout": 1,
      "commands": [{ "command": "cat", "setupCommand": "sleep 33.7", "exts": ["txt"] }]
    }));
    let setup_state = SetupState::default();
    let expected = Err(
      "Setup command 'sleep' did not finish within 1 seconds, so it was killed. Increase the \"setupTimeout\" configuration if it needs longer.".to_string(),
    );
    let start = Instant::now();
    assert_eq!(format(&config, "text", &setup_state).await, expected);
    assert!(!sleep_is_running("33.7"), "the setup command should have been killed");
    // the next file fails right away instead of waiting out the timeout again
    assert_eq!(format(&config, "text", &setup_state).await, expected);
    assert!(start.elapsed() < Duration::from_secs(3));
  }
}
