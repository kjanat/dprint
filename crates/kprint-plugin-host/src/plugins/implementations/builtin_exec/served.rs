use std::cell::Cell;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::ChildStdin;
use std::process::ChildStdout;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use kprint_owned_child::OwnedChild;
use kprint_plugin_types::CancellationToken;
use kprint_plugin_types::FormatError;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::Semaphore;
use tokio::sync::oneshot;

use super::configuration::Serve;
use super::handler::MAX_STDERR_LEN;
use super::handler::exit_status_text;
use super::handler::max_output_len;

const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const EXIT_WAIT: Duration = Duration::from_millis(500);
const ANSWER_EXCERPT_LEN: usize = 200;

#[derive(Clone)]
pub struct ServedCommands {
  commands: Rc<RefCell<HashMap<ServedKey, Rc<ServedCommand>>>>,
  idle_timeout: Duration,
}

impl Default for ServedCommands {
  fn default() -> Self {
    Self {
      commands: Default::default(),
      idle_timeout: IDLE_TIMEOUT,
    }
  }
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ServedKey {
  pub executable: PathBuf,
  pub args: Vec<String>,
  pub env: BTreeMap<String, String>,
  pub cwd: PathBuf,
  pub serve: Serve,
}

pub struct ServedRequest<'a> {
  pub file_path: &'a Path,
  pub text: &'a [u8],
  pub line_width: u32,
  pub indent_width: u8,
  pub use_tabs: bool,
  pub timeout: Duration,
  pub token: &'a Arc<dyn CancellationToken>,
}

pub enum Served {
  Formatted(Vec<u8>),
  Cancelled,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Request<'a> {
  path: &'a str,
  text: &'a str,
  line_width: u32,
  indent_width: u8,
  use_tabs: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Answer {
  Text(String),
  Error(String),
}

struct ServedCommand {
  key: ServedKey,
  permits: Arc<Semaphore>,
  idle: RefCell<Vec<Worker>>,
  started: Cell<u64>,
}

struct Worker {
  process: Process,
  pipes: Pipes,
}

struct Process {
  id: u64,
  uses: u64,
  child: OwnedChild,
  stderr: Arc<Mutex<StderrTail>>,
  stderr_closed: Option<oneshot::Receiver<()>>,
}

struct Pipes {
  stdin: ChildStdin,
  stdout: BufReader<ChildStdout>,
}

#[derive(Default)]
struct StderrTail {
  bytes: Vec<u8>,
  truncated: bool,
}

enum Stop {
  ClosedOutput,
  StoppedReading(String),
  Unreadable(String),
  Unanswered(Duration),
  TooLong(usize),
  Garbled { error: String, answer: Vec<u8> },
}

impl Stop {
  fn exits(&self) -> bool {
    matches!(self, Stop::ClosedOutput | Stop::StoppedReading(_))
  }

  fn describe(&self) -> String {
    match self {
      Stop::ClosedOutput => "closed its output".to_string(),
      Stop::StoppedReading(err) => format!("stopped reading files ({})", err),
      Stop::Unreadable(err) => format!("couldn't be read from ({})", err),
      Stop::Unanswered(timeout) => format!("has not answered within {} seconds", timeout.as_secs()),
      Stop::TooLong(max_len) => format!("wrote more than {} bytes without ending its answer", max_len),
      Stop::Garbled { error, answer } => format!(
        "answered with something other than {{\"text\"}} or {{\"error\"}} ({}): {}",
        error,
        String::from_utf8_lossy(&answer[..answer.len().min(ANSWER_EXCERPT_LEN)])
      ),
    }
  }
}

impl ServedCommands {
  #[cfg(all(test, unix))]
  fn with_idle_timeout(idle_timeout: Duration) -> Self {
    Self {
      idle_timeout,
      ..Default::default()
    }
  }

  pub async fn format(&self, key: ServedKey, request: ServedRequest<'_>) -> Result<Served, FormatError> {
    let name = key.executable.display().to_string();
    let text = std::str::from_utf8(request.text).map_err(|_| {
      FormatError::new(format!(
        "Cannot send {} to the served command '{}', as the file isn't UTF-8.",
        request.file_path.display(),
        name
      ))
    })?;
    let path = request.file_path.to_string_lossy();
    let mut line = serde_json::to_vec(&Request {
      path: &path,
      text,
      line_width: request.line_width,
      indent_width: request.indent_width,
      use_tabs: request.use_tabs,
    })
    .map_err(|err| FormatError::new(err.to_string()))?;
    line.push(b'\n');
    let max_len = max_output_len(text.len());

    let command = self.command(key);
    let permit = tokio::select! {
      biased;
      _ = request.token.wait_cancellation() => return Ok(Served::Cancelled),
      permit = command.permits.clone().acquire_owned() => permit.map_err(|err| FormatError::new(err.to_string()))?,
    };
    // taken out first, so the borrow ends before starting a worker awaits
    let idle = command.idle.borrow_mut().pop();
    let worker = match idle {
      Some(worker) => worker,
      None => command.start().await.map_err(FormatError::new)?,
    };

    let (sender, receiver) = oneshot::channel();
    let commands = self.clone();
    let timeout = request.timeout;
    kprint_async_runtime::spawn(async move {
      let result = commands.exchange(&command, worker, line, max_len, timeout).await;
      drop(permit);
      let _ignore = sender.send(result);
    });
    tokio::select! {
      biased;
      _ = request.token.wait_cancellation() => Ok(Served::Cancelled),
      result = receiver => result.map_err(|_| FormatError::new(format!("The answer of the served command '{}' was lost.", name)))?,
    }
  }

  fn command(&self, key: ServedKey) -> Rc<ServedCommand> {
    self
      .commands
      .borrow_mut()
      .entry(key.clone())
      .or_insert_with(|| {
        Rc::new(ServedCommand {
          permits: Arc::new(Semaphore::new(key.serve.workers.get())),
          key,
          idle: Default::default(),
          started: Default::default(),
        })
      })
      .clone()
  }

  async fn exchange(&self, command: &Rc<ServedCommand>, worker: Worker, line: Vec<u8>, max_len: usize, timeout: Duration) -> Result<Served, FormatError> {
    let Worker { process, pipes } = worker;
    let answered = kprint_async_runtime::spawn_blocking(move || {
      let mut pipes = pipes;
      let answer = write_and_read(&mut pipes, &line, max_len);
      (pipes, answer)
    });
    let outcome = tokio::select! {
      joined = answered => match joined {
        Ok((pipes, Ok(answer))) => match serde_json::from_slice::<Answer>(&answer) {
          Ok(answer) => Ok((pipes, answer)),
          Err(error) => Err(Stop::Garbled { error: error.to_string(), answer }),
        },
        Ok((_, Err(stop))) => Err(stop),
        Err(err) => Err(Stop::Unreadable(err.to_string())),
      },
      _ = tokio::time::sleep(timeout) => Err(Stop::Unanswered(timeout)),
    };
    match outcome {
      Ok((pipes, answer)) => {
        self.release(command, Worker { process, pipes });
        match answer {
          Answer::Text(text) => Ok(Served::Formatted(text.into_bytes())),
          Answer::Error(error) => Err(FormatError::new(format!(
            "The served command '{}' didn't format the file: {}",
            command.name(),
            error
          ))),
        }
      }
      Err(stop) => Err(FormatError::new(process.stopped_message(&command.name(), stop).await)),
    }
  }

  fn release(&self, command: &Rc<ServedCommand>, mut worker: Worker) {
    worker.process.uses += 1;
    let use_of = (worker.process.id, worker.process.uses);
    command.idle.borrow_mut().push(worker);
    let command = Rc::downgrade(command);
    let idle_timeout = self.idle_timeout;
    kprint_async_runtime::spawn(async move {
      tokio::time::sleep(idle_timeout).await;
      if let Some(command) = command.upgrade() {
        command.idle.borrow_mut().retain(|worker| (worker.process.id, worker.process.uses) != use_of);
      }
    });
  }
}

impl ServedCommand {
  fn name(&self) -> String {
    self.key.executable.display().to_string()
  }

  async fn start(&self) -> Result<Worker, String> {
    let mut command = Command::new(&self.key.executable);
    command
      .current_dir(&self.key.cwd)
      .args(&self.key.args)
      .envs(&self.key.env)
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped());
    // spawned on a blocking thread, as spawning blocks every file being
    // formatted otherwise, from the owner thread, as the blocking thread may
    // end before the served command does
    let spawned = kprint_async_runtime::spawn_blocking(move || OwnedChild::spawn_from_owner_thread(command))
      .await
      .map_err(|err| err.to_string())
      .and_then(|spawned| spawned.map_err(|err| err.to_string()));
    let mut child = spawned.map_err(|err| format!("Cannot start the served command '{}': {}", self.name(), err))?;
    let (Some(stdin), Some(stdout), Some(stderr)) = (child.take_stdin(), child.take_stdout(), child.take_stderr()) else {
      return Err(format!("The served command '{}' has no pipes to send it files over.", self.name()));
    };
    let tail = Arc::new(Mutex::new(StderrTail::default()));
    let (closed, stderr_closed) = oneshot::channel();
    std::thread::spawn({
      let tail = tail.clone();
      move || {
        keep_stderr_tail(stderr, &tail);
        let _ignore = closed.send(());
      }
    });
    let id = self.started.get();
    self.started.set(id + 1);
    Ok(Worker {
      process: Process {
        id,
        uses: 0,
        child,
        stderr: tail,
        stderr_closed: Some(stderr_closed),
      },
      pipes: Pipes {
        stdin,
        stdout: BufReader::new(stdout),
      },
    })
  }
}

impl Process {
  async fn stopped_message(mut self, name: &str, stop: Stop) -> String {
    let status = if stop.exits() { exit_status_soon(&mut self.child).await } else { None };
    let _ = self.child.kill();
    if let Some(closed) = self.stderr_closed.take() {
      let _ = tokio::time::timeout(Duration::from_secs(1), closed).await;
    }
    let mut message = format!("The served command '{}' {}", name, stop.describe());
    if let Some(status) = status {
      message.push_str(&format!(" and exited with {}", exit_status_text(status)));
    }
    let stderr = self.stderr.lock().map(|tail| tail.text()).unwrap_or_default();
    let stderr = stderr.trim_end();
    if stderr.is_empty() {
      message.push('.');
    } else {
      message.push_str(": ");
      message.push_str(stderr);
    }
    message
  }
}

/// The child's exit status once it exits within [`EXIT_WAIT`], waited for on a
/// blocking thread while the child stays here (see `wait_for_exit`).
async fn exit_status_soon(child: &mut OwnedChild) -> Option<ExitStatus> {
  let waiter = child.exit_waiter().ok()?;
  let exited = kprint_async_runtime::spawn_blocking(move || waiter.wait_timeout(EXIT_WAIT)).await;
  match exited {
    Ok(Ok(true)) => child.try_wait().ok().flatten(),
    _ => None,
  }
}

fn write_and_read(pipes: &mut Pipes, line: &[u8], max_len: usize) -> Result<Vec<u8>, Stop> {
  pipes
    .stdin
    .write_all(line)
    .and_then(|()| pipes.stdin.flush())
    .map_err(|err| Stop::StoppedReading(err.to_string()))?;
  let mut answer = Vec::new();
  let read = (&mut pipes.stdout)
    .take((max_len as u64).saturating_add(1))
    .read_until(b'\n', &mut answer)
    .map_err(|err| Stop::Unreadable(err.to_string()))?;
  if read == 0 {
    return Err(Stop::ClosedOutput);
  }
  if answer.pop() != Some(b'\n') {
    return Err(if answer.len() >= max_len {
      Stop::TooLong(max_len)
    } else {
      Stop::ClosedOutput
    });
  }
  if answer.last() == Some(&b'\r') {
    answer.pop();
  }
  Ok(answer)
}

fn keep_stderr_tail(mut stderr: impl Read, tail: &Mutex<StderrTail>) {
  let mut buffer = vec![0; 64 * 1024];
  loop {
    let read = match stderr.read(&mut buffer) {
      Ok(0) => return,
      Ok(read) => read,
      Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
      Err(_) => return,
    };
    let Ok(mut tail) = tail.lock() else {
      return;
    };
    tail.bytes.extend_from_slice(&buffer[..read]);
    if tail.bytes.len() > 2 * MAX_STDERR_LEN {
      let excess = tail.bytes.len() - MAX_STDERR_LEN;
      tail.bytes.drain(..excess);
      tail.truncated = true;
    }
  }
}

impl StderrTail {
  fn text(&self) -> String {
    let start = self.bytes.len().saturating_sub(MAX_STDERR_LEN);
    let text = String::from_utf8_lossy(&self.bytes[start..]);
    if self.truncated || start > 0 {
      format!("[...] {}", text)
    } else {
      text.into_owned()
    }
  }
}

#[cfg(all(test, unix))]
mod test {
  use std::io::Read;
  use std::io::Write;
  use std::path::PathBuf;
  use std::sync::Arc;
  use std::time::Duration;
  use std::time::Instant;

  use kprint_configuration::ConfigKeyMap;
  use kprint_plugin_types::CancellationToken;
  use kprint_plugin_types::NullCancellationToken;

  use super::ServedCommands;
  use crate::plugins::implementations::builtin_exec::configuration::Configuration;
  use crate::plugins::implementations::builtin_exec::handler::SetupState;
  use crate::plugins::implementations::builtin_exec::handler::format_bytes;

  const UPPERCASE: &str = r#"
/"text":"crash"/ { print "oops" > "/dev/stderr"; exit 3 }
/"text":"bad"/ { print "{\"error\":\"bad syntax\"}"; fflush(); next }
/"text":"garble"/ { print "not json"; fflush(); next }
/"text":"silence"/ { next }
{
  match($0, /"text":"[^"]*"/)
  printf "{\"text\":\"%s\"}\n", toupper(substr($0, RSTART + 8, RLENGTH - 9))
  fflush()
}
"#;

  const FIELDS: &str = r#"
function field(name,   value) {
  match($0, "\"" name "\":(\"[^\"]*\"|[^,}]*)")
  value = substr($0, RSTART + length(name) + 3, RLENGTH - length(name) - 3)
  gsub(/"/, "", value)
  return value
}
{
  printf "{\"text\":\"%s|%s|%s|%s|%s\"}\n", field("path"), field("text"), field("lineWidth"), field("indentWidth"), field("useTabs")
  fflush()
}
"#;

  struct Worker {
    dir: tempfile::TempDir,
  }

  impl Worker {
    fn new(program: &str) -> Self {
      let dir = tempfile::tempdir().unwrap();
      std::fs::File::create(dir.path().join("worker.awk"))
        .unwrap()
        .write_all(program.as_bytes())
        .unwrap();
      Self { dir }
    }

    fn config(&self, exec: serde_json::Value, command: serde_json::Value) -> Arc<Configuration> {
      let mut command_config = serde_json::json!({
        "command": format!(
          "sh -c \"echo $$ >> {}; exec awk -f {}\"",
          self.dir.path().join("starts").display(),
          self.dir.path().join("worker.awk").display()
        ),
        "exts": ["txt"],
        "serve": true,
      });
      command_config.as_object_mut().unwrap().extend(command.as_object().unwrap().clone());
      let mut config = exec;
      config["commands"] = serde_json::json!([command_config]);
      let config: ConfigKeyMap = serde_json::from_value(config).unwrap();
      let result = Configuration::resolve(config, &Default::default(), &crate::environment::CanonicalizedPathBuf::new_for_testing("/"));
      assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
      Arc::new(result.config)
    }

    fn starts(&self) -> Vec<String> {
      let mut text = String::new();
      if let Ok(mut file) = std::fs::File::open(self.dir.path().join("starts")) {
        file.read_to_string(&mut text).unwrap();
      }
      text.lines().map(ToOwned::to_owned).collect()
    }
  }

  fn is_running(process_id: &str) -> bool {
    std::process::Command::new("kill")
      .args(["-0", process_id])
      .stderr(std::process::Stdio::null())
      .status()
      .unwrap()
      .success()
  }

  async fn format(config: &Arc<Configuration>, served: &ServedCommands, path: &str, text: &[u8]) -> Result<Option<String>, String> {
    format_with_token(config, served, path, text, Arc::new(NullCancellationToken)).await
  }

  async fn format_with_token(
    config: &Arc<Configuration>,
    served: &ServedCommands,
    path: &str,
    text: &[u8],
    token: Arc<dyn CancellationToken>,
  ) -> Result<Option<String>, String> {
    format_bytes(PathBuf::from(path), text.to_vec(), config.clone(), token, &SetupState::default(), served)
      .await
      .map(|bytes| bytes.map(|bytes| String::from_utf8(bytes).unwrap()))
      .map_err(|err| err.to_string())
  }

  fn run<T>(future: impl std::future::Future<Output = T>) -> T {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let result = runtime.block_on(future);
    let (dropped, was_dropped) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
      drop(runtime);
      let _ignore = dropped.send(());
    });
    assert!(
      was_dropped.recv_timeout(Duration::from_secs(5)).is_ok(),
      "something it started was left running"
    );
    result
  }

  #[test]
  fn formats_every_file_with_one_process() {
    let worker = Worker::new(UPPERCASE);
    let config = worker.config(serde_json::json!({}), serde_json::json!({}));
    run(async {
      let served = ServedCommands::default();
      assert_eq!(format(&config, &served, "a.txt", b"hello").await, Ok(Some("HELLO".to_string())));
      assert_eq!(format(&config, &served, "b.txt", b"world").await, Ok(Some("WORLD".to_string())));
      assert_eq!(format(&config, &served, "c.txt", b"SAME").await, Ok(None));
    });
    assert_eq!(worker.starts().len(), 1);
    assert!(!is_running(&worker.starts()[0]), "the process should end with kprint");
  }

  #[test]
  fn sends_each_file_with_its_path_and_settings() {
    let worker = Worker::new(FIELDS);
    let config = worker.config(serde_json::json!({ "lineWidth": 80, "indentWidth": 4, "useTabs": true }), serde_json::json!({}));
    run(async {
      let served = ServedCommands::default();
      assert_eq!(
        format(&config, &served, "dir/a b.txt", b"one\ntwo").await,
        Ok(Some("dir/a b.txt|one\ntwo|80|4|true".to_string()))
      );
    });
  }

  #[test]
  fn starts_a_command_with_its_environment() {
    let worker = Worker::new(r#"{ printf "{\"text\":\"%s\"}\n", ENVIRON["GREETING"]; fflush() }"#);
    let script = format!("exec awk -f {}", worker.dir.path().join("worker.awk").display());
    let config = worker.config(
      serde_json::json!({ "lineWidth": 80 }),
      serde_json::json!({ "command": { "executable": "sh", "args": ["-c", script], "env": { "GREETING": "width {{line_width}}" } } }),
    );
    run(async {
      let served = ServedCommands::default();
      assert_eq!(format(&config, &served, "a.txt", b"text").await, Ok(Some("width 80".to_string())));
    });
  }

  #[test]
  fn fails_a_file_the_command_answers_with_an_error_for() {
    let worker = Worker::new(UPPERCASE);
    let config = worker.config(serde_json::json!({}), serde_json::json!({}));
    run(async {
      let served = ServedCommands::default();
      assert_eq!(
        format(&config, &served, "a.txt", b"bad").await,
        Err("The served command 'sh' didn't format the file: bad syntax".to_string())
      );
      assert_eq!(format(&config, &served, "a.txt", b"ok").await, Ok(Some("OK".to_string())));
    });
    assert_eq!(worker.starts().len(), 1);
  }

  #[test]
  fn starts_a_new_process_for_the_files_after_one_that_stopped() {
    let worker = Worker::new(UPPERCASE);
    let config = worker.config(serde_json::json!({}), serde_json::json!({}));
    run(async {
      let served = ServedCommands::default();
      assert_eq!(
        format(&config, &served, "a.txt", b"crash").await,
        Err("The served command 'sh' closed its output and exited with code 3: oops".to_string())
      );
      assert_eq!(format(&config, &served, "b.txt", b"ok").await, Ok(Some("OK".to_string())));
    });
    assert_eq!(worker.starts().len(), 2);
  }

  #[test]
  fn kills_a_command_that_doesnt_answer_in_time() {
    let worker = Worker::new(UPPERCASE);
    let config = worker.config(serde_json::json!({ "timeout": 1 }), serde_json::json!({}));
    run(async {
      let served = ServedCommands::default();
      let start = Instant::now();
      let message = format(&config, &served, "a.txt", b"silence").await.unwrap_err();
      assert_eq!(message, "The served command 'sh' has not answered within 1 seconds.");
      assert!(start.elapsed() < Duration::from_secs(3), "{:?}", start.elapsed());
      assert!(!is_running(&worker.starts()[0]), "the command should have been killed");
    });
  }

  #[test]
  fn stops_a_command_that_answers_with_something_else() {
    let worker = Worker::new(UPPERCASE);
    let config = worker.config(serde_json::json!({}), serde_json::json!({}));
    run(async {
      let served = ServedCommands::default();
      let message = format(&config, &served, "a.txt", b"garble").await.unwrap_err();
      assert_eq!(
        message,
        "The served command 'sh' answered with something other than {\"text\"} or {\"error\"} (expected value at line 1 column 1): not json."
      );
      assert_eq!(format(&config, &served, "b.txt", b"ok").await, Ok(Some("OK".to_string())));
    });
    assert_eq!(worker.starts().len(), 2);
  }

  #[test]
  fn runs_as_many_processes_at_once_as_its_workers_say() {
    for (workers, starts) in [(1, 1), (2, 2)] {
      let worker = Worker::new(UPPERCASE);
      let config = worker.config(serde_json::json!({}), serde_json::json!({ "workers": workers }));
      run(async {
        let served = ServedCommands::default();
        let results = kprint_async_runtime::future::join_all((0..4).map(|_| format(&config, &served, "a.txt", b"text"))).await;
        assert_eq!(results, vec![Ok(Some("TEXT".to_string())); 4]);
      });
      assert_eq!(worker.starts().len(), starts, "{} workers", workers);
    }
  }

  #[test]
  fn ends_a_process_that_was_idle_for_a_while() {
    let worker = Worker::new(UPPERCASE);
    let config = worker.config(serde_json::json!({}), serde_json::json!({}));
    run(async {
      let served = ServedCommands::with_idle_timeout(Duration::from_millis(200));
      assert_eq!(format(&config, &served, "a.txt", b"one").await, Ok(Some("ONE".to_string())));
      let first = worker.starts()[0].clone();
      assert!(is_running(&first));
      tokio::time::sleep(Duration::from_millis(600)).await;
      assert!(!is_running(&first), "the idle process should have been ended");
      assert_eq!(format(&config, &served, "a.txt", b"two").await, Ok(Some("TWO".to_string())));
    });
    assert_eq!(worker.starts().len(), 2);
  }

  #[test]
  fn stops_waiting_for_a_cancelled_file() {
    let worker = Worker::new(UPPERCASE);
    let config = worker.config(serde_json::json!({}), serde_json::json!({ "workers": 2 }));
    run(async {
      let served = ServedCommands::default();
      let token = tokio_util::sync::CancellationToken::new();
      let start = Instant::now();
      let cancel = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        token.cancel();
      };
      let (result, ()) = tokio::join!(format_with_token(&config, &served, "a.txt", b"silence", Arc::new(token.clone())), cancel);
      assert_eq!(result, Ok(None));
      assert!(start.elapsed() < Duration::from_secs(2), "{:?}", start.elapsed());
      assert_eq!(format(&config, &served, "a.txt", b"next").await, Ok(Some("NEXT".to_string())));
    });
  }

  #[test]
  fn fails_a_file_that_isnt_utf8() {
    let worker = Worker::new(UPPERCASE);
    let config = worker.config(serde_json::json!({}), serde_json::json!({}));
    run(async {
      let served = ServedCommands::default();
      assert_eq!(
        format(&config, &served, "a.txt", &[0xff, 0xfe]).await,
        Err("Cannot send a.txt to the served command 'sh', as the file isn't UTF-8.".to_string())
      );
    });
    assert_eq!(worker.starts().len(), 0);
  }
}
