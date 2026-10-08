use anyhow::Context;
use anyhow::Result;
use dprint_async_runtime::FutureExt;
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;

use dprint_plugin_types::PluginInfo;

use super::WasmModuleCreator;
use super::process;
use super::wasm;
use crate::environment::PluginEnvironment as Environment;
use crate::plugins::Plugin;
use crate::plugins::PluginCache;
use crate::plugins::PluginSourceReference;
use crate::plugins::cache_meta::format_rate_path;
use crate::plugins::cache_meta::native_module_path;
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
  plugin_cache: &Rc<PluginCache<TEnvironment>>,
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
      // the modules are loaded once the plugin is used, so a plugin that has
      // nothing to do (ex. its resolutions are cached and its files are
      // unchanged) is never loaded
      let loader = Rc::new(WasmModuleLoader {
        environment: environment.clone(),
        plugin_cache: plugin_cache.clone(),
        plugin_reference: plugin_reference.clone(),
        file_path: cache_item.file_path.clone(),
        info: cache_item.info.clone(),
        build_id: cache_item.build_id,
        wasm_module_creator: wasm_module_creator.clone(),
      });
      let modules = wasm::WasmPluginModules {
        load_interpreted: Box::new({
          let loader = loader.clone();
          move || {
            let loader = loader.clone();
            async move { loader.load_interpreted().await }.boxed_local()
          }
        }),
        load_native: Box::new({
          let loader = loader.clone();
          move || {
            let loader = loader.clone();
            async move { loader.load_native().await }.boxed_local()
          }
        }),
        load_cached_native: Box::new({
          let loader = loader.clone();
          move || {
            let loader = loader.clone();
            async move { loader.load_cached_native().await }.boxed_local()
          }
        }),
        wasm_module_path: cache_item.file_path.clone(),
        native_module_path: native_module_path(&cache_item.file_path),
        format_rate_path: format_rate_path(&cache_item.file_path),
      };
      Ok(Box::new(wasm::WasmPlugin::new(
        cache_item.info,
        modules,
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

/// Loads a Wasm plugin's modules from the plugin cache: the module the
/// interpreter runs, and the native module that formats.
///
/// `info` and `build_id` are of the build the run started with, whose info
/// and resolutions it already matched files and resolved configuration with.
/// The modules loaded must be of that build.
struct WasmModuleLoader<TEnvironment: Environment> {
  environment: TEnvironment,
  plugin_cache: Rc<PluginCache<TEnvironment>>,
  plugin_reference: PluginSourceReference,
  /// The build's module (see `cache_meta::wasm_module_path`).
  file_path: PathBuf,
  info: PluginInfo,
  build_id: u64,
  wasm_module_creator: WasmModuleCreator,
}

impl<TEnvironment: Environment> WasmModuleLoader<TEnvironment> {
  async fn load_cached_native(&self) -> Result<Option<wasm::WasmModule>> {
    let native_path = native_module_path(&self.file_path);
    if !self.environment.path_exists(&native_path) {
      return Ok(None);
    }
    load_compiled_wasm_module_in_background(&self.environment, native_path, &self.wasm_module_creator)
      .await
      .map(Some)
  }

  async fn load_interpreted(&self) -> Result<wasm::InterpretedModule> {
    self
      .load_from_cache(|environment, file_path| wasm::InterpretedModule::new(&environment.read_file_bytes(file_path)?))
      .await
  }

  /// Loads the native module compiled from the build's module, compiling it
  /// first when it isn't yet.
  async fn load_native(&self) -> Result<wasm::WasmModule> {
    let native_path = native_module_path(&self.file_path);
    if self.environment.path_exists(&native_path) {
      match load_compiled_wasm_module_in_background(&self.environment, native_path.clone(), &self.wasm_module_creator).await {
        Ok(module) => return Ok(module),
        // ex. it was compiled for a CPU with different features, or by a
        // different wasm engine or rustc version, or the file is corrupt
        Err(err) => log_debug!(
          self.environment,
          "Error loading compiled Wasm module {}, so compiling it again. Message: {:#}",
          native_path.display(),
          err
        ),
      }
    }
    let (file_path, wasm_bytes) = self
      .load_from_cache(|environment, file_path| Ok((file_path.to_path_buf(), environment.read_file_bytes(file_path)?)))
      .await?;
    let plugin_display = self.plugin_reference.display();
    let plugin_name = format!("{} {}", self.info.name, self.info.version);
    let compiled = wasm::compile_native_module(&plugin_name, wasm_bytes, &self.environment)
      .await
      .with_context(|| format!("Error compiling plugin {}", plugin_display))?;
    let native_path = native_module_path(&file_path);
    if let Err(err) = self.environment.atomic_write_file_bytes(&native_path, &compiled) {
      // it's compiled again next time
      log_debug!(self.environment, "Error writing compiled Wasm module {}: {:#}", native_path.display(), err);
    }
    dprint_async_runtime::spawn_blocking({
      let wasm_module_creator = self.wasm_module_creator.clone();
      move || wasm_module_creator.create_from_serialized(&compiled)
    })
    .await?
  }

  /// Loads from the build's module file with `load`.
  ///
  /// The file can be missing or fail to load (ex. the cache file is
  /// corrupt). When that happens, this forgets the cache, sets the plugin up
  /// again, and tries once more.
  async fn load_from_cache<T: Send + 'static>(&self, load: fn(&TEnvironment, &Path) -> Result<T>) -> Result<T> {
    // Each build's module is a file of its own that no other build replaces,
    // so what loads from it is the build the run started with.
    let result = match self.load_in_background(load, self.file_path.clone()).await {
      Ok(module) => Ok(module),
      Err(err) => {
        // Another dprint process may have set up a different build since this
        // run created the plugin (ex. a local plugin that was rebuilt while
        // `dprint lsp` runs), which removes this build's module. That build
        // stays cached for the next run.
        if self
          .plugin_cache
          .cached_build_id(&self.plugin_reference)
          .is_some_and(|current| current != self.build_id)
        {
          return Err(changed_while_running_error(&self.plugin_reference, &self.info, None));
        }
        log_debug!(
          self.environment,
          "Error loading Wasm plugin from cache. Forgetting from cache and retrying. Message: {:#}",
          err
        );
        match self.plugin_cache.forget_and_recreate(&self.plugin_reference).await {
          // the source is a different plugin now (ex. a url without a checksum
          // that serves a new version), which stays cached for the next run
          Ok(cache_item) if cache_item.build_id != self.build_id => {
            return Err(changed_while_running_error(&self.plugin_reference, &self.info, Some(&cache_item.info)));
          }
          Ok(cache_item) => self.load_in_background(load, cache_item.file_path).await,
          Err(err) => Err(err),
        }
      }
    };
    match result {
      Ok(module) => Ok(module),
      Err(err) => {
        if let Err(forget_err) = self.plugin_cache.forget(&self.plugin_reference).await {
          log_debug!(self.environment, "Error forgetting {}: {:#}", self.plugin_reference.display(), forget_err);
        }
        Err(err).with_context(|| format!("Error loading plugin {}", self.plugin_reference.display()))
      }
    }
  }

  async fn load_in_background<T: Send + 'static>(&self, load: fn(&TEnvironment, &Path) -> Result<T>, file_path: PathBuf) -> Result<T> {
    // the plugins are used concurrently on one thread, so this loads them in
    // parallel rather than one after the other
    let environment = self.environment.clone();
    dprint_async_runtime::spawn_blocking(move || load(&environment, &file_path)).await?
  }
}

/// The error for a plugin whose build changed while dprint was running. The
/// run can't switch to the new build midway, as it matched files and resolved
/// configuration with the old one.
fn changed_while_running_error(plugin_reference: &PluginSourceReference, info: &PluginInfo, new_info: Option<&PluginInfo>) -> anyhow::Error {
  let change = match new_info {
    Some(new_info) if new_info.name != info.name || new_info.version != info.version => {
      format!(" ({} {} is now {} {})", info.name, info.version, new_info.name, new_info.version)
    }
    _ => String::new(),
  };
  anyhow::anyhow!(
    "Error loading plugin {}: it changed while dprint was running{}. Run dprint again.",
    plugin_reference.display(),
    change
  )
}

async fn load_compiled_wasm_module_in_background<TEnvironment: Environment>(
  environment: &TEnvironment,
  file_path: PathBuf,
  wasm_module_creator: &WasmModuleCreator,
) -> Result<wasm::WasmModule> {
  // the plugins are used concurrently on one thread, so this loads them in
  // parallel rather than one after the other
  dprint_async_runtime::spawn_blocking({
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
  use crate::test_helpers::WASM_PLUGIN_0_1_0_BYTES;
  use crate::test_helpers::WASM_PLUGIN_BYTES;
  use dprint_platform::environment::*;

  #[test]
  #[allow(clippy::disallowed_methods)] // a real environment needs real files
  fn loads_compiled_wasm_modules_in_a_real_environment() {
    let environment = crate::environment::RealEnvironment::new(crate::environment::HeadlessServices, "test").unwrap();
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

    // neither the cached module nor the plugin it was set up from load
    environment.write_file_bytes(&cache_item.file_path, b"corrupt").unwrap();
    environment.add_remote_file("https://plugins.dprint.dev/test.wasm", b"corrupt");
    assert!(plugin.initialize().await.is_err());
    // it isn't set up again, even once it could be
    environment.add_remote_file("https://plugins.dprint.dev/test.wasm", WASM_PLUGIN_BYTES);
    assert!(plugin.initialize().await.is_err());
    assert!(!environment.path_exists(&cache_item.file_path));
    environment.take_stderr_messages();
  }

  #[tokio::test]
  async fn should_error_when_a_recompiled_plugin_is_a_different_one() {
    let environment = TestEnvironment::new();
    environment.add_remote_file("https://plugins.dprint.dev/test.wasm", WASM_PLUGIN_BYTES);
    let plugin_cache = Rc::new(PluginCache::new(environment.clone()));
    let plugin_reference = PluginSourceReference::new_remote_from_str("https://plugins.dprint.dev/test.wasm");
    let cache_item = plugin_cache.get_plugin_cache_item(&plugin_reference).await.unwrap();
    let plugin = create_plugin(&plugin_cache, environment.clone(), &plugin_reference, &WasmModuleCreator::default())
      .await
      .unwrap();
    assert_eq!(plugin.info().version, "0.2.0");

    // the compiled module needs recompiling, and the url now serves another version
    environment.write_file_bytes(&cache_item.file_path, b"corrupt").unwrap();
    environment.add_remote_file("https://plugins.dprint.dev/test.wasm", WASM_PLUGIN_0_1_0_BYTES);
    let err = plugin.initialize().await.err().unwrap();
    assert_eq!(
      err.to_string(),
      concat!(
        "Error loading plugin https://plugins.dprint.dev/test.wasm: it changed while dprint was running ",
        "(test-plugin 0.2.0 is now test-plugin 0.1.0). Run dprint again."
      )
    );

    // the new one stays cached for the next run
    environment.take_stderr_messages();
    let cache_item = plugin_cache.get_plugin_cache_item(&plugin_reference).await.unwrap();
    assert_eq!(cache_item.info.version, "0.1.0");
    assert_eq!(environment.take_stderr_messages(), Vec::<String>::new());
  }

  #[tokio::test]
  async fn should_error_when_another_process_set_up_a_different_build() {
    let environment = TestEnvironment::new();
    environment.add_remote_file("https://plugins.dprint.dev/test.wasm", WASM_PLUGIN_BYTES);
    let plugin_cache = Rc::new(PluginCache::new(environment.clone()));
    let plugin_reference = PluginSourceReference::new_remote_from_str("https://plugins.dprint.dev/test.wasm");
    let plugin = create_plugin(&plugin_cache, environment.clone(), &plugin_reference, &WasmModuleCreator::default())
      .await
      .unwrap();

    // ex. another dprint process sets it up again after the url changed,
    // replacing the module this plugin would load
    environment.add_remote_file("https://plugins.dprint.dev/test.wasm", WASM_PLUGIN_0_1_0_BYTES);
    PluginCache::new(environment.clone()).forget_and_recreate(&plugin_reference).await.unwrap();
    let err = plugin.initialize().await.err().unwrap();
    assert_eq!(
      err.to_string(),
      "Error loading plugin https://plugins.dprint.dev/test.wasm: it changed while dprint was running. Run dprint again."
    );
    environment.take_stderr_messages();
  }

  #[tokio::test]
  async fn should_error_when_another_process_set_up_another_build_of_the_same_version() {
    let environment = TestEnvironment::new();
    environment.add_remote_file("https://plugins.dprint.dev/test.wasm", WASM_PLUGIN_BYTES);
    let plugin_cache = Rc::new(PluginCache::new(environment.clone()));
    let plugin_reference = PluginSourceReference::new_remote_from_str("https://plugins.dprint.dev/test.wasm");
    let plugin = create_plugin(&plugin_cache, environment.clone(), &plugin_reference, &WasmModuleCreator::default())
      .await
      .unwrap();

    // the same plugin name and version, but other contents (an empty custom section)
    let mut other_build = WASM_PLUGIN_BYTES.to_vec();
    other_build.extend([0x00, 0x05, 0x04, b't', b'e', b's', b't']);
    environment.add_remote_file("https://plugins.dprint.dev/test.wasm", other_build.leak());
    PluginCache::new(environment.clone()).forget_and_recreate(&plugin_reference).await.unwrap();
    let err = plugin.initialize().await.err().unwrap();
    assert_eq!(
      err.to_string(),
      "Error loading plugin https://plugins.dprint.dev/test.wasm: it changed while dprint was running. Run dprint again."
    );
    environment.take_stderr_messages();
  }

  #[tokio::test]
  async fn should_load_a_module_cached_before_builds_had_files_of_their_own() {
    let environment = TestEnvironment::new();
    environment.add_remote_file("https://plugins.dprint.dev/test.wasm", WASM_PLUGIN_BYTES);
    let plugin_cache = Rc::new(PluginCache::new(environment.clone()));
    let plugin_reference = PluginSourceReference::new_remote_from_str("https://plugins.dprint.dev/test.wasm");
    let cache_item = plugin_cache.get_plugin_cache_item(&plugin_reference).await.unwrap();
    environment.take_stderr_messages();

    // make the entry look like one an earlier dprint set up
    let cache_key = format!("remote:{}", "https://plugins.dprint.dev/test.wasm");
    let hash = crate::plugins::cache_meta::entry_hash(&cache_key, &environment);
    let mut meta = crate::plugins::cache_meta::read_meta(&hash, &environment).unwrap();
    meta.source_checksum = None;
    let legacy_file_path = crate::plugins::cache_meta::wasm_module_path(&hash, None, &environment);
    environment
      .write_file_bytes(&legacy_file_path, &environment.read_file_bytes(&cache_item.file_path).unwrap())
      .unwrap();
    environment.remove_file(&cache_item.file_path).unwrap();
    crate::plugins::cache_meta::write_meta(&hash, &meta, &environment).unwrap();

    let plugin = create_plugin(
      &Rc::new(PluginCache::new(environment.clone())),
      environment.clone(),
      &plugin_reference,
      &WasmModuleCreator::default(),
    )
    .await
    .unwrap();
    let plugin = plugin.initialize().await.unwrap();
    assert!(plugin.license_text().await.is_ok());
    assert_eq!(environment.take_stderr_messages(), Vec::<String>::new());
  }

  #[tokio::test]
  async fn should_load_the_build_it_was_created_with_after_a_local_rebuild() {
    let environment = TestEnvironment::new();
    environment.write_file_bytes("/test.wasm", WASM_PLUGIN_BYTES).unwrap();
    let plugin_cache = Rc::new(PluginCache::new(environment.clone()));
    let plugin_reference = PluginSourceReference::new_local(PathBuf::from("/test.wasm"));
    let wasm_module_creator = WasmModuleCreator::default();
    let create = || create_plugin(&plugin_cache, environment.clone(), &plugin_reference, &wasm_module_creator);
    let first_build_plugin = create().await.unwrap();
    let other_first_build_plugin = create().await.unwrap();

    // ex. rebuilt while `dprint lsp` runs, and set up again by another process
    environment.write_file_bytes("/test.wasm", WASM_PLUGIN_0_1_0_BYTES).unwrap();
    PluginCache::new(environment.clone()).get_plugin_cache_item(&plugin_reference).await.unwrap();
    let plugin = first_build_plugin.initialize().await.unwrap();
    assert!(plugin.license_text().await.is_ok());
    assert_eq!(first_build_plugin.info().version, "0.2.0");

    // two builds later, the first build's module is gone
    let third_build = [WASM_PLUGIN_0_1_0_BYTES, &[0x00, 0x05, 0x04, b't', b'e', b's', b't']].concat();
    environment.write_file_bytes("/test.wasm", &third_build).unwrap();
    PluginCache::new(environment.clone()).get_plugin_cache_item(&plugin_reference).await.unwrap();
    let err = other_first_build_plugin.initialize().await.err().unwrap();
    assert_eq!(
      err.to_string(),
      "Error loading plugin /test.wasm: it changed while dprint was running. Run dprint again."
    );
    environment.take_stderr_messages();
  }

  #[tokio::test]
  async fn should_load_a_plugin_compiled_again_from_the_same_source() {
    let environment = TestEnvironment::new();
    environment.add_remote_file("https://plugins.dprint.dev/test.wasm", WASM_PLUGIN_BYTES);
    let plugin_cache = Rc::new(PluginCache::new(environment.clone()));
    let plugin_reference = PluginSourceReference::new_remote_from_str("https://plugins.dprint.dev/test.wasm");
    let plugin = create_plugin(&plugin_cache, environment.clone(), &plugin_reference, &WasmModuleCreator::default())
      .await
      .unwrap();

    // the same build compiled again isn't a change
    PluginCache::new(environment.clone()).forget_and_recreate(&plugin_reference).await.unwrap();
    let plugin = plugin.initialize().await.unwrap();
    assert!(plugin.license_text().await.is_ok());
    environment.take_stderr_messages();
  }

  /// Formats a file with the test plugin.
  async fn format_with(plugin: &Rc<dyn crate::plugins::InitializedPlugin>) -> dprint_plugin_types::FormatResult {
    plugin
      .format_text(crate::plugins::InitializedPluginFormatRequest {
        file_path: PathBuf::from("/file.txt"),
        file_text: b"text".to_vec(),
        range: None,
        config: std::sync::Arc::new(crate::plugins::FormatConfig {
          id: dprint_plugin_types::FormatConfigId::from_raw(1),
          global: Default::default(),
          plugin: Default::default(),
        }),
        override_config: Default::default(),
        on_host_format: Rc::new(|_| async { Ok(None) }.boxed_local()),
        token: std::sync::Arc::new(dprint_plugin_types::NullCancellationToken),
      })
      .await
  }

  #[tokio::test]
  async fn should_format_in_the_interpreter_until_it_chooses_to_compile() {
    let environment = TestEnvironment::new();
    environment.add_remote_file("https://plugins.dprint.dev/test.wasm", WASM_PLUGIN_BYTES);
    let plugin_cache = Rc::new(PluginCache::new(environment.clone()));
    let plugin_reference = PluginSourceReference::new_remote_from_str("https://plugins.dprint.dev/test.wasm");
    let cache_item = plugin_cache.get_plugin_cache_item(&plugin_reference).await.unwrap();
    let plugin = create_plugin(&plugin_cache, environment.clone(), &plugin_reference, &WasmModuleCreator::default())
      .await
      .unwrap();
    assert!(plugin.chooses_format_engine());
    assert!(!plugin.compiles_to_format());

    // what comes before formatting doesn't compile it, and neither does
    // formatting a little
    let initialized = plugin.initialize().await.unwrap();
    assert!(initialized.license_text().await.is_ok());
    plugin.choose_format_engine(4);
    assert!(!plugin.compiles_to_format());
    assert_eq!(format_with(&initialized).await.unwrap(), Some(b"text_formatted".to_vec()));
    assert_eq!(environment.take_stderr_messages(), Vec::<String>::new());
    assert!(environment.take_wasm_compile_deadlines().is_empty());

    // formatting a lot does, once
    plugin.choose_format_engine(100 * 1024 * 1024);
    assert!(plugin.compiles_to_format());
    assert_eq!(format_with(&initialized).await.unwrap(), Some(b"text_formatted".to_vec()));
    assert_eq!(format_with(&initialized).await.unwrap(), Some(b"text_formatted".to_vec()));
    assert_eq!(
      crate::test_helpers::normalize_compile_times(environment.take_stderr_messages()),
      vec!["Compiling test-plugin 0.2.0", "Compiled test-plugin 0.2.0 in <elapsed>"]
    );
    assert_eq!(environment.take_wasm_compile_deadlines().len(), 1);
    assert!(environment.path_exists(native_module_path(&cache_item.file_path)));
    assert!(!plugin.compiles_to_format());
    assert!(!plugin.chooses_format_engine());

    // and later runs load the native code, however little they format
    let plugin = create_plugin(&plugin_cache, environment.clone(), &plugin_reference, &WasmModuleCreator::default())
      .await
      .unwrap();
    assert!(!plugin.chooses_format_engine());
    assert!(!plugin.compiles_to_format());
    let initialized = plugin.initialize().await.unwrap();
    assert_eq!(format_with(&initialized).await.unwrap(), Some(b"text_formatted".to_vec()));
    assert_eq!(environment.take_stderr_messages(), Vec::<String>::new());
    assert!(environment.take_wasm_compile_deadlines().is_empty());
  }

  // https://github.com/dprint/dprint/issues/734
  #[tokio::test]
  async fn should_compile_again_when_the_compiled_module_fails_to_load() {
    let environment = TestEnvironment::new();
    environment.add_remote_file("https://plugins.dprint.dev/test.wasm", WASM_PLUGIN_BYTES);
    let plugin_cache = Rc::new(PluginCache::new(environment.clone()));
    let plugin_reference = PluginSourceReference::new_remote_from_str("https://plugins.dprint.dev/test.wasm");
    let cache_item = plugin_cache.get_plugin_cache_item(&plugin_reference).await.unwrap();
    let wasm_module_creator = WasmModuleCreator::default();
    let create = || create_plugin(&plugin_cache, environment.clone(), &plugin_reference, &wasm_module_creator);
    let plugin = create().await.unwrap();
    plugin.choose_format_engine(100 * 1024 * 1024);
    assert!(format_with(&plugin.initialize().await.unwrap()).await.is_ok());
    assert_eq!(
      crate::test_helpers::normalize_compile_times(environment.take_stderr_messages()),
      vec!["Compiling test-plugin 0.2.0", "Compiled test-plugin 0.2.0 in <elapsed>"]
    );

    // ex. it was compiled for a CPU with different features, or the file is corrupt
    environment.write_file_bytes(native_module_path(&cache_item.file_path), b"corrupt").unwrap();
    // nothing was downloaded again
    environment.add_remote_file("https://plugins.dprint.dev/test.wasm", b"corrupt");
    let plugin = create().await.unwrap().initialize().await.unwrap();
    assert_eq!(format_with(&plugin).await.unwrap(), Some(b"text_formatted".to_vec()));
    assert_eq!(
      crate::test_helpers::normalize_compile_times(environment.take_stderr_messages()),
      vec!["Compiling test-plugin 0.2.0", "Compiled test-plugin 0.2.0 in <elapsed>"]
    );
  }

  #[tokio::test]
  async fn should_set_up_again_when_the_cached_module_fails_to_load() {
    let environment = TestEnvironment::new();
    environment.add_remote_file("https://plugins.dprint.dev/test.wasm", WASM_PLUGIN_BYTES);
    let plugin_cache = Rc::new(PluginCache::new(environment.clone()));
    let plugin_reference = PluginSourceReference::new_remote_from_str("https://plugins.dprint.dev/test.wasm");
    let cache_item = plugin_cache.get_plugin_cache_item(&plugin_reference).await.unwrap();
    environment.write_file_bytes(&cache_item.file_path, b"corrupt").unwrap();

    // creating the plugin doesn't load the module
    let plugin = create_plugin(&plugin_cache, environment.clone(), &plugin_reference, &WasmModuleCreator::default())
      .await
      .unwrap();
    assert_eq!(plugin.info().name, "test-plugin");

    // initializing it does, which sets it up again from its source instead of failing
    let plugin = plugin.initialize().await.unwrap();
    assert!(plugin.license_text().await.is_ok());
    assert_eq!(environment.read_file_bytes(&cache_item.file_path).unwrap(), WASM_PLUGIN_BYTES);
    assert_eq!(environment.take_stderr_messages(), Vec::<String>::new());
  }
}
