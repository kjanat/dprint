use anyhow::Result;
use dprint_async_runtime::async_trait;
use dprint_configuration::ConfigurationDiagnostic;
use dprint_plugin_types::CheckConfigUpdatesMessage;
use dprint_plugin_types::ConfigChange;
use dprint_plugin_types::FileMatchingInfo;
use dprint_plugin_types::FormatConfigId;
use dprint_plugin_types::FormatResult;
use dprint_plugin_types::PluginInfo;
use parking_lot::Mutex;
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use crate::environment::PluginEnvironment as Environment;
use crate::plugins::FormatConfig;
use crate::plugins::InitializedPlugin;
use crate::plugins::InitializedPluginFormatRequest;
use crate::plugins::Plugin;

use super::InitializedProcessPluginCommunicator;

/// Use this to get an executable file name that also works in the tests.
///
/// In the test environment the cached "executable" is an in-memory copy of the
/// real test process plugin binary, which can't actually be launched. Copy it
/// out to a real file under the target directory (named per version, since the
/// test binary reports its version from its own path — e.g.
/// `temp-plugin-0.3.0`) and run that. The cache layout no longer carries the
/// version in the path, so it's passed in.
pub fn get_test_safe_executable_path(version: &str, executable_file_path: PathBuf, environment: &impl Environment) -> PathBuf {
  if environment.is_real() {
    return executable_file_path;
  }

  static CREATED_TEMP_FILES: once_cell::sync::Lazy<Mutex<std::collections::HashSet<PathBuf>>> = once_cell::sync::Lazy::new(Default::default);

  let file_name = if cfg!(target_os = "windows") {
    format!("temp-plugin-{version}.exe")
  } else {
    format!("temp-plugin-{version}")
  };
  let temp_file = test_plugins_dir().join(file_name);

  // create the per-version temp executable once, from the (identical across
  // every test process plugin) binary bytes
  let mut created = CREATED_TEMP_FILES.lock();
  if created.insert(temp_file.clone()) {
    let bytes = environment.read_file_bytes(&executable_file_path).unwrap();
    write_executable(&temp_file, &bytes).unwrap();
  }
  temp_file
}

/// `target/<profile>/dprint-test-plugins`, next to the test binary's `deps`.
fn test_plugins_dir() -> PathBuf {
  let exe = std::env::current_exe().unwrap();
  exe.parent().and_then(Path::parent).unwrap_or(&exe).join("dprint-test-plugins")
}

/// Writes an executable file through a child process, then moves it into place.
#[cfg(unix)]
fn write_executable(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
  // Linux refuses to run a file that any process holds open for writing, and a child forked by another thread holds this process's descriptors until it execs.
  use std::io::Read;
  use std::io::Write;
  let mut child = dprint_owned_child::OwnedChild::spawn(
    std::process::Command::new("sh")
      .args([
        "-c",
        r#"set -e; temp="$1.$$"; trap 'rm -f "${temp}"' EXIT; trap 'exit 1' HUP INT TERM; mkdir -p "$(dirname "$1")"; cat > "${temp}"; chmod +x "${temp}"; mv -f "${temp}" "$1""#,
        "sh",
      ])
      .arg(path)
      .stdin(std::process::Stdio::piped())
      .stderr(std::process::Stdio::piped()),
  )?;
  let mut stdin = child.take_stdin().ok_or_else(|| std::io::Error::other("the shell has no stdin"))?;
  let mut stderr = child.take_stderr().ok_or_else(|| std::io::Error::other("the shell has no stderr"))?;
  let written = stdin.write_all(bytes);
  drop(stdin);
  let mut message = String::new();
  stderr.read_to_string(&mut message)?;
  let status = child.wait()?;
  if status.success() {
    written
  } else {
    Err(std::io::Error::other(format!(
      "writing {} failed: {}: {}",
      path.display(),
      status,
      message.trim_end()
    )))
  }
}

#[cfg(windows)]
fn write_executable(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
  if let Some(dir) = path.parent() {
    #[allow(clippy::disallowed_methods)]
    std::fs::create_dir_all(dir)?;
  }
  #[allow(clippy::disallowed_methods)]
  std::fs::write(path, bytes)
}

pub struct ProcessPlugin<TEnvironment: Environment> {
  environment: TEnvironment,
  executable_file_path: PathBuf,
  plugin_info: PluginInfo,
}

impl<TEnvironment: Environment> ProcessPlugin<TEnvironment> {
  pub fn new(environment: TEnvironment, executable_file_path: PathBuf, plugin_info: PluginInfo) -> Self {
    ProcessPlugin {
      environment,
      executable_file_path,
      plugin_info,
    }
  }
}

#[async_trait(?Send)]
impl<TEnvironment: Environment> Plugin for ProcessPlugin<TEnvironment> {
  fn info(&self) -> &PluginInfo {
    &self.plugin_info
  }

  fn is_process_plugin(&self) -> bool {
    true
  }

  async fn initialize(&self) -> Result<Rc<dyn InitializedPlugin>> {
    let start_instant = Instant::now();
    let plugin_name = &self.info().name;
    log_debug!(self.environment, "Creating instance of {}", plugin_name);
    let communicator = InitializedProcessPluginCommunicator::new(plugin_name.to_string(), self.executable_file_path.clone(), self.environment.clone()).await?;
    let process_plugin = InitializedProcessPlugin::new(communicator)?;

    let result: Rc<dyn InitializedPlugin> = Rc::new(process_plugin);
    log_debug!(
      self.environment,
      "Created instance of {} in {}ms",
      plugin_name,
      start_instant.elapsed().as_millis() as u64
    );
    Ok(result)
  }
}

pub struct InitializedProcessPlugin<TEnvironment: Environment> {
  communicator: Rc<InitializedProcessPluginCommunicator<TEnvironment>>,
}

impl<TEnvironment: Environment> InitializedProcessPlugin<TEnvironment> {
  pub fn new(communicator: InitializedProcessPluginCommunicator<TEnvironment>) -> Result<Self> {
    Ok(Self {
      communicator: Rc::new(communicator),
    })
  }
}

#[async_trait(?Send)]
impl<TEnvironment: Environment> InitializedPlugin for InitializedProcessPlugin<TEnvironment> {
  async fn license_text(&self) -> Result<String> {
    self.communicator.get_license_text().await
  }

  async fn resolved_config(&self, config: Arc<FormatConfig>) -> Result<String> {
    self.communicator.get_resolved_config(&config).await
  }

  async fn file_matching_info(&self, config: Arc<FormatConfig>) -> Result<FileMatchingInfo> {
    self.communicator.get_file_matching_info(&config).await
  }

  async fn config_diagnostics(&self, config: Arc<FormatConfig>) -> Result<Vec<ConfigurationDiagnostic>> {
    self.communicator.get_config_diagnostics(&config).await
  }

  async fn check_config_updates(&self, message: CheckConfigUpdatesMessage) -> Result<Vec<ConfigChange>> {
    self.communicator.check_config_updates(&message).await
  }

  async fn format_text(&self, request: InitializedPluginFormatRequest) -> FormatResult {
    self.communicator.format_text(request).await
  }

  async fn release_config(&self, config_id: FormatConfigId) -> Result<()> {
    self.communicator.release_config(config_id).await
  }

  async fn shutdown(&self) -> () {
    self.communicator.shutdown().await
  }
}

#[cfg(all(test, unix))]
mod test {
  use std::path::Path;
  use std::process::Command;
  use std::sync::Arc;
  use std::sync::atomic::AtomicBool;
  use std::sync::atomic::Ordering;
  use std::thread::JoinHandle;

  use super::write_executable;

  /// Threads that start processes until dropped.
  struct Spawners {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
  }

  impl Spawners {
    fn start(count: usize) -> Self {
      let stop = Arc::new(AtomicBool::new(false));
      let threads = (0..count)
        .map(|_| {
          let stop = stop.clone();
          std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
              Command::new("true").status().unwrap();
            }
          })
        })
        .collect();
      Spawners { stop, threads }
    }
  }

  impl Drop for Spawners {
    fn drop(&mut self) {
      self.stop.store(true, Ordering::Relaxed);
      for thread in self.threads.drain(..) {
        let _ = thread.join();
      }
    }
  }

  fn dir_entries(dir: &Path) -> String {
    let output = Command::new("ls").arg("-A").arg(dir).output().unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap()
  }

  #[test]
  fn runs_a_freshly_written_executable_while_other_threads_spawn() {
    let dir = tempfile::tempdir().unwrap();
    let _spawners = Spawners::start(8);
    for i in 0..300 {
      let path = dir.path().join("sub").join(format!("script-{i}"));
      write_executable(&path, b"#!/bin/sh\nexit 0\n").unwrap();
      assert!(Command::new(&path).status().unwrap().success(), "{}", path.display());
    }
  }

  #[test]
  fn removes_the_temporary_file_when_writing_fails() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    assert!(Command::new("mkdir").arg("-m").arg("555").arg(&target).status().unwrap().success());
    let result = write_executable(&target, b"#!/bin/sh\nexit 0\n");
    assert!(Command::new("chmod").arg("755").arg(&target).status().unwrap().success());
    let message = result.unwrap_err().to_string();
    assert!(message.contains("Permission denied"), "{message}");
    assert_eq!(dir_entries(dir.path()), "target\n");
    assert_eq!(dir_entries(&target), "");
  }
}
