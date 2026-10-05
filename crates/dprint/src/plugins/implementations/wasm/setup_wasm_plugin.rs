use crate::utils::PathSource;
use std::path::Path;

use anyhow::Result;

use crate::environment::Environment;

use super::super::SetupPluginResult;
use super::CompileControl;

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

pub async fn setup_wasm_plugin<TEnvironment: Environment>(
  url_or_file_path: &PathSource,
  file_bytes: Vec<u8>,
  dest_file_path: &Path,
  environment: &TEnvironment,
) -> Result<SetupPluginResult> {
  let guard = environment
    .progress_bars()
    .map(|pb| pb.add_progress(format!("Compiling {}", url_or_file_path.display()), crate::utils::ProgressBarStyle::Action, 1));
  if guard.is_none() {
    log_stderr_info!(environment, "Compiling {}", url_or_file_path.display());
  }
  // what it's set up for may have to be done by a deadline (ex. regenerating
  // the schema file), which then stops the compile too
  let control = CompileControl::new(crate::utils::current_deadline());
  // the compile is stopped once nothing waits for it, rather than left
  // running in its blocking task
  let _cancel_on_drop = CancelOnDrop(&control);
  let compile_result = dprint_core::async_runtime::spawn_blocking({
    let environment = environment.clone();
    let plugin_display = url_or_file_path.display().to_string();
    let control = control.clone();
    move || environment.compile_wasm(&plugin_display, &file_bytes, &control)
  })
  .await??;
  drop(guard);
  environment.mk_dir_all(dest_file_path.parent().unwrap())?;
  environment.atomic_write_file_bytes(dest_file_path, &compile_result.bytes)?;

  Ok(SetupPluginResult {
    plugin_info: compile_result.plugin_info,
    file_path: dest_file_path.to_path_buf(),
    executable_sub_path: None,
  })
}

#[cfg(test)]
mod test {
  use std::path::Path;
  use std::time::Duration;
  use std::time::Instant;

  use super::*;
  use crate::environment::CanonicalizedPathBuf;
  use crate::environment::TestEnvironment;

  #[tokio::test]
  async fn compiles_by_the_deadline_of_what_its_for() {
    let environment = TestEnvironment::new();
    let source = PathSource::new_local(CanonicalizedPathBuf::new_for_testing("/plugin.wasm"));
    let bytes = crate::test_helpers::WASM_PLUGIN_BYTES.to_vec();
    setup_wasm_plugin(&source, bytes.clone(), Path::new("/cache/plugin.compiled"), &environment)
      .await
      .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    crate::utils::run_before_deadline(deadline, setup_wasm_plugin(&source, bytes, Path::new("/cache/plugin.compiled"), &environment))
      .await
      .unwrap()
      .unwrap();
    assert_eq!(environment.take_wasm_compile_deadlines(), vec![None, Some(deadline)]);
    environment.take_stderr_messages();
  }
}
