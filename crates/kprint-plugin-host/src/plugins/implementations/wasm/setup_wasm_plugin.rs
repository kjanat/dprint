use crate::utils::PathSource;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use kprint_plugin_types::PluginInfo;

use crate::environment::PluginEnvironment as Environment;

use super::super::NoRetrySetupError;
use super::super::SetupPluginResult;
use super::CompileControl;
use super::interpreter::InterpretedModule;

/// Cancels the compile when dropped.
struct CancelOnDrop<'a>(&'a CompileControl);

impl Drop for CancelOnDrop<'_> {
  fn drop(&mut self) {
    self.0.cancel();
  }
}

// cache-busting key for the serialized wasmtime artifact. wasmtime additionally
// validates engine/CPU compatibility on deserialize (recompiling on mismatch),
// so this only needs to bump when the wasm engine changes. keep it dot-numeric
// so it parses as a version; tracks the pinned wasmtime version.
pub const WASM_CACHE_VERSION: &str = "43.0.2";

/// Sets up a Wasm plugin: gets its plugin info by interpreting it and keeps
/// its module at `dest_file_path`.
///
/// Nothing is compiled. The module is compiled to native code the first time
/// the plugin formats (see [`compile_native_module`]), so a run that only
/// resolves configuration or lists files never compiles a plugin.
pub async fn setup_wasm_plugin<TEnvironment: Environment>(
  url_or_file_path: &PathSource,
  file_bytes: Vec<u8>,
  dest_file_path: &Path,
  environment: &TEnvironment,
) -> Result<SetupPluginResult> {
  let plugin_display = url_or_file_path.display().to_string();
  let (plugin_info, file_bytes) = kprint_async_runtime::spawn_blocking({
    let environment = environment.clone();
    let plugin_display = plugin_display.clone();
    move || (interpreted_plugin_info(&environment, &plugin_display, &file_bytes), file_bytes)
  })
  .await?;
  // the plugin fails the same way however often it's set up
  let plugin_info = plugin_info.map_err(|err| NoRetrySetupError(format!("Error setting up {}: {:#}", plugin_display, err)))?;
  environment.mk_dir_all(dest_file_path.parent().unwrap())?;
  environment.atomic_write_file_bytes(dest_file_path, &file_bytes)?;

  Ok(SetupPluginResult {
    plugin_info,
    file_path: dest_file_path.to_path_buf(),
    executable_sub_path: None,
  })
}

fn interpreted_plugin_info(environment: &impl Environment, plugin_display: &str, wasm_bytes: &[u8]) -> Result<PluginInfo> {
  let module = InterpretedModule::new(wasm_bytes)?;
  let log = {
    let environment = environment.clone();
    let plugin_display = plugin_display.to_string();
    Arc::new(move |text: &str| environment.log_stderr_with_context(text, &plugin_display))
  };
  module.instantiate(log)?.plugin_info()
}

/// Compiles a Wasm plugin's module to native code, showing that it does.
pub async fn compile_native_module<TEnvironment: Environment>(plugin_display: &str, wasm_bytes: Vec<u8>, environment: &TEnvironment) -> Result<Vec<u8>> {
  let guard = environment
    .progress_bars()
    .map(|pb| pb.add_progress(format!("Compiling {}", plugin_display), crate::utils::ProgressBarStyle::Action, 1));
  if guard.is_none() {
    log_stderr_info!(environment, "Compiling {}", plugin_display);
  }
  // what it's compiled for may have to be done by a deadline (ex. formatting
  // for an editor), which then stops the compile too
  let control = CompileControl::new(crate::utils::current_deadline());
  // the compile is stopped once nothing waits for it, rather than left
  // running in its blocking task
  let _cancel_on_drop = CancelOnDrop(&control);
  let (compile_result, elapsed) = kprint_async_runtime::spawn_blocking({
    let environment = environment.clone();
    let plugin_display = plugin_display.to_string();
    let control = control.clone();
    move || {
      let start = Instant::now();
      let result = environment.compile_wasm(&plugin_display, &wasm_bytes, &control);
      (result, start.elapsed())
    }
  })
  .await?;
  // Clear the transient progress display before leaving a permanent timing.
  drop(guard);
  match &compile_result {
    Ok(_) => log_stderr_info!(environment, "Compiled {} in {:.3}s", plugin_display, elapsed.as_secs_f64()),
    Err(_) => log_stderr_info!(environment, "Failed compiling {} after {:.3}s", plugin_display, elapsed.as_secs_f64()),
  }
  Ok(compile_result?.bytes)
}

#[cfg(test)]
mod test {
  use kprint_platform::environment::*;
  use std::path::Path;
  use std::time::Duration;
  use std::time::Instant;

  use super::*;
  use crate::environment::CanonicalizedPathBuf;
  use crate::environment::TestEnvironment;
  use crate::test_helpers::WASM_PLUGIN_0_1_0_BYTES;
  use crate::test_helpers::WASM_PLUGIN_BYTES;

  fn source() -> PathSource {
    PathSource::new_local(CanonicalizedPathBuf::new_for_testing("/plugin.wasm"))
  }

  #[tokio::test]
  async fn sets_up_without_compiling() {
    let environment = TestEnvironment::new();
    // a plugin of each schema version
    for (bytes, version) in [(WASM_PLUGIN_BYTES, "0.2.0"), (WASM_PLUGIN_0_1_0_BYTES, "0.1.0")] {
      let result = setup_wasm_plugin(&source(), bytes.to_vec(), Path::new("/cache/plugin.wasm"), &environment)
        .await
        .unwrap();
      assert_eq!(result.plugin_info.name, "test-plugin");
      assert_eq!(result.plugin_info.version, version);
      assert_eq!(result.plugin_info, crate::plugins::compile_wasm(bytes).unwrap().plugin_info);
      assert_eq!(environment.read_file_bytes("/cache/plugin.wasm").unwrap(), bytes);
    }
    assert_eq!(environment.take_wasm_compile_deadlines(), Vec::<Option<Instant>>::new());
    assert_eq!(environment.take_stderr_messages(), Vec::<String>::new());
  }

  #[tokio::test]
  async fn does_not_retry_setting_up_what_isnt_a_plugin() {
    let environment = TestEnvironment::new();
    let err = setup_wasm_plugin(&source(), b"\0asm\x01\0\0\0".to_vec(), Path::new("/cache/plugin.wasm"), &environment)
      .await
      .err()
      .unwrap();
    assert!(err.downcast_ref::<NoRetrySetupError>().is_some());
    assert!(
      err
        .to_string()
        .starts_with("Error setting up /plugin.wasm: Error determining plugin schema version."),
      "{}",
      err
    );
    assert!(!environment.path_exists("/cache/plugin.wasm"));
  }

  #[tokio::test]
  async fn reports_failed_compilation_without_claiming_success() {
    let environment = TestEnvironment::new();
    assert!(compile_native_module("broken 1.0", b"invalid wasm".to_vec(), &environment).await.is_err());
    assert_eq!(
      crate::test_helpers::normalize_compile_times(environment.take_stderr_messages()),
      vec!["Compiling broken 1.0", "Failed compiling broken 1.0 after <elapsed>"]
    );
  }

  #[tokio::test]
  async fn compiles_by_the_deadline_of_what_its_for() {
    let environment = TestEnvironment::new();
    let bytes = WASM_PLUGIN_BYTES.to_vec();
    compile_native_module("/plugin.wasm", bytes.clone(), &environment).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    crate::utils::run_before_deadline(deadline, compile_native_module("/plugin.wasm", bytes, &environment))
      .await
      .unwrap()
      .unwrap();
    assert_eq!(environment.take_wasm_compile_deadlines(), vec![None, Some(deadline)]);
    assert_eq!(
      crate::test_helpers::normalize_compile_times(environment.take_stderr_messages()),
      vec![
        "Compiling /plugin.wasm",
        "Compiled /plugin.wasm in <elapsed>",
        "Compiling /plugin.wasm",
        "Compiled /plugin.wasm in <elapsed>"
      ]
    );
  }
}
