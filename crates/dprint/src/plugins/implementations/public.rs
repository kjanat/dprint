use anyhow::Result;
use std::path::Path;
use std::path::PathBuf;

use dprint_core::plugins::PluginInfo;

use super::WasmModuleCreator;
use super::process;
use super::wasm;
use crate::environment::Environment;
use crate::plugins::Plugin;
use crate::plugins::PluginCache;
use crate::plugins::PluginCacheItem;
use crate::plugins::PluginSourceReference;
use crate::utils::PathSource;
use crate::utils::PluginKind;

/// Setting up a plugin failed in a way the plugin cache shouldn't retry by
/// setting it up again: the setup already retried what retrying can fix, or
/// the failure is the plugin's own (ex. it isn't a dprint plugin).
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct NoRetrySetupError(pub String);

pub struct SetupPluginResult {
  pub file_path: PathBuf,
  pub plugin_info: PluginInfo,
  /// For process plugins, the executable's path relative to its extract dir.
  /// Stored in the cache meta so the file path can be re-derived on a hit
  /// without re-extracting. `None` for wasm plugins.
  pub executable_sub_path: Option<String>,
}

/// Where a freshly set-up plugin's artifact should be written. Both paths are
/// derived from the plugin's cache hash so the layout stays flat — wasm plugins
/// write a single file and never need a per-plugin directory.
pub struct SetupPluginDest {
  /// File to write the compiled module to (wasm plugins).
  pub wasm_file_path: PathBuf,
  /// Directory to extract into (process plugins).
  pub process_dir_path: PathBuf,
}

/// Inputs to [`setup_plugin`] (everything but the environment).
pub struct SetupPluginOptions<'a> {
  /// The plugin's source, post-redirect, so process plugins can resolve
  /// relative paths in their manifest.
  pub resolved_source: &'a PathSource,
  pub file_bytes: Vec<u8>,
  pub plugin_kind: PluginKind,
  pub pre_resolved_tarball: Option<crate::plugins::npm_resolution::PreResolvedProcessPluginTarball>,
  pub dest: &'a SetupPluginDest,
}

pub async fn setup_plugin<TEnvironment: Environment>(options: SetupPluginOptions<'_>, environment: &TEnvironment) -> Result<SetupPluginResult> {
  let SetupPluginOptions {
    resolved_source,
    file_bytes,
    plugin_kind,
    pre_resolved_tarball,
    dest,
  } = options;
  match plugin_kind {
    PluginKind::Wasm => wasm::setup_wasm_plugin(resolved_source, file_bytes, &dest.wasm_file_path, environment).await,
    PluginKind::Process => process::setup_process_plugin(resolved_source, &file_bytes, pre_resolved_tarball, &dest.process_dir_path, environment).await,
  }
}

pub async fn create_plugin<TEnvironment: Environment>(
  plugin_cache: &PluginCache<TEnvironment>,
  environment: TEnvironment,
  plugin_reference: &PluginSourceReference,
  wasm_module_creator: &WasmModuleCreator,
) -> Result<Box<dyn Plugin>> {
  let cache_item = match plugin_cache.get_plugin_cache_item(plugin_reference).await {
    Ok(cache_item) => cache_item,
    Err(err) if err.downcast_ref::<NoRetrySetupError>().is_some() => return Err(err),
    Err(err) => {
      log_debug!(
        environment,
        "Error getting plugin from cache. Forgetting from cache and retrying. Message: {}",
        err.to_string()
      );

      // forget and try again
      plugin_cache.forget_and_recreate(plugin_reference).await?
    }
  };

  match cache_item.plugin_kind {
    PluginKind::Wasm => {
      // The cached compiled module can fail to read or deserialize (ex. it was
      // compiled for a CPU with different features, or by a different
      // wasm engine/rustc version, or the cache file is corrupt). When that happens,
      // forget the cache, recompile from source, and try once more.
      let plugin = match create_wasm_plugin(&environment, &cache_item, wasm_module_creator) {
        Ok(plugin) => plugin,
        Err(err) => {
          log_debug!(
            environment,
            "Error loading Wasm plugin from cache. Forgetting from cache and retrying. Message: {:#}",
            err
          );

          // forget and try again
          let cache_item = plugin_cache.forget_and_recreate(plugin_reference).await?;
          create_wasm_plugin(&environment, &cache_item, wasm_module_creator)?
        }
      };
      Ok(Box::new(plugin))
    }
    PluginKind::Process => {
      let cache_item = if !environment.path_exists(&cache_item.file_path) {
        log_debug!(
          environment,
          "Could not find process plugin at {}. Forgetting from cache and retrying.",
          cache_item.file_path.display()
        );

        // forget and try again
        plugin_cache.forget_and_recreate(plugin_reference).await?
      } else {
        cache_item
      };

      let executable_path = super::process::get_test_safe_executable_path(&cache_item.info.version, cache_item.file_path, &environment);
      Ok(Box::new(process::ProcessPlugin::new(environment, executable_path, cache_item.info)))
    }
  }
}

/// Reads the cached compiled Wasm module and loads it, verifying it can run on
/// this machine. Returns an error when the cache is unreadable or the module
/// can't be loaded so the caller can recompile from source.
fn create_wasm_plugin<TEnvironment: Environment>(
  environment: &TEnvironment,
  cache_item: &PluginCacheItem,
  wasm_module_creator: &WasmModuleCreator,
) -> Result<wasm::WasmPlugin<TEnvironment>> {
  let module = load_compiled_wasm_module(environment, &cache_item.file_path, wasm_module_creator)?;
  Ok(wasm::WasmPlugin::new(module, cache_item.info.clone(), environment.clone()))
}

fn load_compiled_wasm_module<TEnvironment: Environment>(
  environment: &TEnvironment,
  file_path: &Path,
  wasm_module_creator: &WasmModuleCreator,
) -> Result<wasm::WasmModule> {
  // Mapping the file instead of reading it skips copying the module twice
  // (into a buffer, then into executable memory), which is most of the time
  // it takes to load a large plugin. Not on Windows, where a mapped file can't
  // be deleted or replaced, so `dprint clear-cache` would fail while a
  // language server has the plugin loaded.
  if cfg!(unix) && environment.is_real() {
    match wasm_module_creator.create_from_serialized_file(file_path) {
      Ok(module) => return Ok(module),
      // ex. the cache directory is on a file system mounted noexec, which
      // doesn't allow mapping its files as executable
      Err(err) => log_debug!(
        environment,
        "Failed mapping compiled Wasm module {}, so reading it instead: {:#}",
        file_path.display(),
        err
      ),
    }
  }
  let file_bytes = environment.read_file_bytes(file_path)?;
  wasm_module_creator.create_from_serialized(&file_bytes)
}

#[cfg(test)]
mod test {
  use super::*;
  use crate::environment::TestEnvironment;
  use crate::test_helpers::WASM_PLUGIN_BYTES;

  #[test]
  #[allow(clippy::disallowed_methods)] // a real environment needs real files
  fn loads_compiled_wasm_modules_in_a_real_environment() {
    let environment = crate::environment::RealEnvironment::new(crate::environment::RealEnvironmentOptions {
      log_level: crate::utils::LogLevel::Info,
      is_stdout_machine_readable: false,
    })
    .unwrap();
    let wasm_module_creator = WasmModuleCreator::default();
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("plugin.cwasm");
    std::fs::write(&file_path, wasm::compile(WASM_PLUGIN_BYTES).unwrap().bytes).unwrap();
    assert!(load_compiled_wasm_module(&environment, &file_path, &wasm_module_creator).is_ok());

    // an invalid module fails both ways, so the caller recompiles it
    std::fs::write(&file_path, b"not a module").unwrap();
    assert!(load_compiled_wasm_module(&environment, &file_path, &wasm_module_creator).is_err());
  }

  // https://github.com/dprint/dprint/issues/734
  #[tokio::test]
  async fn should_recompile_when_cached_wasm_module_fails_to_load() {
    let environment = TestEnvironment::new();
    environment.add_remote_file("https://plugins.dprint.dev/test.wasm", WASM_PLUGIN_BYTES);
    let plugin_cache = PluginCache::new(environment.clone());
    let wasm_module_creator = WasmModuleCreator::default();
    let plugin_reference = PluginSourceReference::new_remote_from_str("https://plugins.dprint.dev/test.wasm");

    // populate the cache (compiles the plugin)
    let cache_item = plugin_cache.get_plugin_cache_item(&plugin_reference).await.unwrap();
    assert_eq!(environment.take_stderr_messages(), vec!["Compiling https://plugins.dprint.dev/test.wasm"]);

    // corrupt the cached compiled module so it can't be deserialized/instantiated
    environment.write_file_bytes(&cache_item.file_path, b"corrupt").unwrap();

    // creating the plugin should recompile from source instead of failing
    let plugin = create_plugin(&plugin_cache, environment.clone(), &plugin_reference, &wasm_module_creator)
      .await
      .unwrap();
    assert_eq!(plugin.info().name, "test-plugin");
    assert_eq!(environment.take_stderr_messages(), vec!["Compiling https://plugins.dprint.dev/test.wasm"]);
  }
}
