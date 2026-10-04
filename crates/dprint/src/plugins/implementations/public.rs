use anyhow::Context;
use anyhow::Result;
use dprint_core::async_runtime::FutureExt;
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;

use dprint_core::plugins::PluginInfo;

use super::WasmModuleCreator;
use super::process;
use super::wasm;
use crate::environment::Environment;
use crate::plugins::Plugin;
use crate::plugins::PluginCache;
use crate::plugins::PluginSourceReference;
use crate::utils::PathSource;
use crate::utils::PluginKind;

/// Setting up a plugin failed even though the setup was already retried, so
/// it shouldn't be retried again.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct SetupRetriesExhaustedError(pub String);

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
  plugin_cache: &Rc<PluginCache<TEnvironment>>,
  environment: TEnvironment,
  plugin_reference: &PluginSourceReference,
  wasm_module_creator: &WasmModuleCreator,
) -> Result<Box<dyn Plugin>> {
  let cache_item = match plugin_cache.get_plugin_cache_item(plugin_reference).await {
    Ok(cache_item) => cache_item,
    Err(err) if err.downcast_ref::<SetupRetriesExhaustedError>().is_some() => return Err(err),
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
      // the module is loaded once the plugin is used, so a plugin that has
      // nothing to format (ex. its files are unchanged) is never loaded
      let load_module = {
        let environment = environment.clone();
        let plugin_cache = plugin_cache.clone();
        let plugin_reference = plugin_reference.clone();
        let file_path = cache_item.file_path.clone();
        let wasm_module_creator = wasm_module_creator.clone();
        move || {
          load_wasm_module(
            environment.clone(),
            plugin_cache.clone(),
            plugin_reference.clone(),
            file_path.clone(),
            wasm_module_creator.clone(),
          )
          .boxed_local()
        }
      };
      Ok(Box::new(wasm::WasmPlugin::new(
        cache_item.info,
        Box::new(load_module),
        cache_item.resolution_cache,
        environment,
      )))
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

/// Loads a Wasm plugin's cached compiled module.
///
/// The module can fail to read or deserialize (ex. it was compiled for a CPU
/// with different features, or by a different wasm engine/rustc version, or
/// the cache file is corrupt). When that happens, this forgets the cache,
/// recompiles from source, and tries once more.
async fn load_wasm_module<TEnvironment: Environment>(
  environment: TEnvironment,
  plugin_cache: Rc<PluginCache<TEnvironment>>,
  plugin_reference: PluginSourceReference,
  file_path: PathBuf,
  wasm_module_creator: WasmModuleCreator,
) -> Result<wasm::WasmModule> {
  let result = match load_compiled_wasm_module_in_background(&environment, file_path, &wasm_module_creator).await {
    Ok(module) => Ok(module),
    Err(err) => {
      log_debug!(
        environment,
        "Error loading Wasm plugin from cache. Forgetting from cache and retrying. Message: {:#}",
        err
      );
      match plugin_cache.forget_and_recreate(&plugin_reference).await {
        Ok(cache_item) => load_compiled_wasm_module_in_background(&environment, cache_item.file_path, &wasm_module_creator).await,
        Err(err) => Err(err),
      }
    }
  };
  match result {
    Ok(module) => Ok(module),
    Err(err) => {
      if let Err(forget_err) = plugin_cache.forget(&plugin_reference).await {
        log_debug!(environment, "Error forgetting {}: {:#}", plugin_reference.display(), forget_err);
      }
      Err(err).with_context(|| format!("Error loading plugin {}", plugin_reference.display()))
    }
  }
}

async fn load_compiled_wasm_module_in_background<TEnvironment: Environment>(
  environment: &TEnvironment,
  file_path: PathBuf,
  wasm_module_creator: &WasmModuleCreator,
) -> Result<wasm::WasmModule> {
  // the plugins are used concurrently on one thread, so this loads them in
  // parallel rather than one after the other
  dprint_core::async_runtime::spawn_blocking({
    let environment = environment.clone();
    let wasm_module_creator = wasm_module_creator.clone();
    move || load_compiled_wasm_module(&environment, &file_path, &wasm_module_creator)
  })
  .await?
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

  #[tokio::test]
  async fn should_not_retry_loading_a_plugin_that_failed_to_load() {
    let environment = TestEnvironment::new();
    environment.add_remote_file("https://plugins.dprint.dev/test.wasm", WASM_PLUGIN_BYTES);
    let plugin_cache = Rc::new(PluginCache::new(environment.clone()));
    let plugin_reference = PluginSourceReference::new_remote_from_str("https://plugins.dprint.dev/test.wasm");
    let cache_item = plugin_cache.get_plugin_cache_item(&plugin_reference).await.unwrap();
    let plugin = create_plugin(&plugin_cache, environment.clone(), &plugin_reference, &WasmModuleCreator::default())
      .await
      .unwrap();
    environment.take_stderr_messages();

    // neither the compiled module nor the plugin it's compiled from load
    environment.write_file_bytes(&cache_item.file_path, b"corrupt").unwrap();
    environment.add_remote_file("https://plugins.dprint.dev/test.wasm", b"corrupt");
    assert!(plugin.initialize().await.is_err());
    assert!(!environment.take_stderr_messages().is_empty());
    // so it isn't recompiled again
    assert!(plugin.initialize().await.is_err());
    assert_eq!(environment.take_stderr_messages(), Vec::<String>::new());
  }

  // https://github.com/dprint/dprint/issues/734
  #[tokio::test]
  async fn should_recompile_when_cached_wasm_module_fails_to_load() {
    let environment = TestEnvironment::new();
    environment.add_remote_file("https://plugins.dprint.dev/test.wasm", WASM_PLUGIN_BYTES);
    let plugin_cache = Rc::new(PluginCache::new(environment.clone()));
    let wasm_module_creator = WasmModuleCreator::default();
    let plugin_reference = PluginSourceReference::new_remote_from_str("https://plugins.dprint.dev/test.wasm");

    // populate the cache (compiles the plugin)
    let cache_item = plugin_cache.get_plugin_cache_item(&plugin_reference).await.unwrap();
    assert_eq!(environment.take_stderr_messages(), vec!["Compiling https://plugins.dprint.dev/test.wasm"]);

    // corrupt the cached compiled module so it can't be deserialized/instantiated
    environment.write_file_bytes(&cache_item.file_path, b"corrupt").unwrap();

    // creating the plugin doesn't load the module
    let plugin = create_plugin(&plugin_cache, environment.clone(), &plugin_reference, &wasm_module_creator)
      .await
      .unwrap();
    assert_eq!(plugin.info().name, "test-plugin");
    assert_eq!(environment.take_stderr_messages(), Vec::<String>::new());

    // initializing it does, which recompiles from source instead of failing
    let plugin = plugin.initialize().await.unwrap();
    assert!(plugin.license_text().await.is_ok());
    assert_eq!(environment.take_stderr_messages(), vec!["Compiling https://plugins.dprint.dev/test.wasm"]);
  }
}
