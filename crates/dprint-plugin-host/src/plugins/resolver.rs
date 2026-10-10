use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use dprint_async_runtime::future;
use dprint_communication::IdGenerator;
use dprint_plugin_types::FormatConfigId;
use dprint_plugin_types::PluginInfo;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use super::InitializedPlugin;
use super::implementations::ExecFormatter;
use super::implementations::WasmModuleCreator;
use super::implementations::create_builtin_exec_plugin;
use super::implementations::create_plugin;
use crate::environment::PluginEnvironment as Environment;
use crate::plugins::BuiltInFormatter;
use crate::plugins::Plugin;
use crate::plugins::PluginCache;
use crate::plugins::PluginResolutionCache;
use crate::plugins::PluginSourceReference;
use crate::plugins::referenced_plugin_name;
use crate::utils::AsyncMutex;

pub struct PluginWrapper {
  plugin: Box<dyn Plugin>,
  initialized_plugin: AsyncMutex<Option<Rc<dyn InitializedPlugin>>>,
}

impl PluginWrapper {
  pub fn new(plugin: Box<dyn Plugin>) -> Self {
    Self {
      plugin,
      initialized_plugin: Default::default(),
    }
  }

  pub fn info(&self) -> &PluginInfo {
    self.plugin.info()
  }

  pub fn is_process_plugin(&self) -> bool {
    self.plugin.is_process_plugin()
  }

  /// Set when the plugin is a formatter built into dprint.
  pub fn built_in(&self) -> Option<&'static BuiltInFormatter> {
    self.plugin.built_in()
  }

  pub async fn prepare_format_engine(&self) -> Result<()> {
    self.plugin.prepare_format_engine().await
  }

  /// Whether how the plugin formats depends on how much it formats.
  pub fn chooses_format_engine(&self) -> bool {
    self.plugin.chooses_format_engine()
  }

  /// Chooses how the plugin formats in this run, from the bytes of the
  /// files it will format.
  pub fn choose_format_engine(&self, bytes_to_format: u64) {
    self.plugin.choose_format_engine(bytes_to_format)
  }

  /// Whether formatting with the plugin first compiles it to native code.
  pub fn compiles_to_format(&self) -> bool {
    self.plugin.compiles_to_format()
  }

  /// Name of the plugin a configuration file refers to for this one (see
  /// [`referenced_plugin_name`]).
  pub fn referenced_plugin_name(&self) -> &str {
    referenced_plugin_name(self.plugin.as_ref())
  }

  /// The schema of the plugin's configuration when it's built into dprint.
  pub fn config_schema(&self) -> Option<&'static str> {
    self.plugin.config_schema()
  }

  pub fn resolution_cache(&self) -> Option<&PluginResolutionCache> {
    self.plugin.resolution_cache()
  }

  pub async fn initialize(&self) -> Result<Rc<dyn InitializedPlugin>> {
    let mut initialized = self.initialized_plugin.lock().await;
    if let Some(plugin) = initialized.as_ref() {
      return Ok(plugin.clone());
    }
    let plugin = self.plugin.initialize().await?;
    *initialized = Some(plugin.clone());
    Ok(plugin)
  }

  /// The initialized plugin, when it has been initialized.
  pub async fn initialized(&self) -> Option<Rc<dyn InitializedPlugin>> {
    self.initialized_plugin.lock().await.clone()
  }

  pub async fn shutdown(&self) {
    let mut initialized = self.initialized_plugin.lock().await;
    if let Some(plugin) = initialized.take() {
      plugin.shutdown().await;
    }
  }
}

/// Configurations whose last owner dropped, to release from the plugins
/// that registered them.
#[derive(Default)]
pub struct ConfigReleaseQueue {
  pending: RefCell<Vec<(Rc<PluginWrapper>, FormatConfigId)>>,
}

impl ConfigReleaseQueue {
  pub fn push(&self, plugin: Rc<PluginWrapper>, config_ids: impl IntoIterator<Item = FormatConfigId>) {
    let mut pending = self.pending.borrow_mut();
    for config_id in config_ids {
      pending.push((plugin.clone(), config_id));
    }
  }

  pub fn take(&self) -> Vec<(Rc<PluginWrapper>, FormatConfigId)> {
    std::mem::take(&mut *self.pending.borrow_mut())
  }
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum PluginCacheKey {
  Source(PluginSourceReference),
  BuiltinExec,
}

pub struct PluginResolver<TEnvironment: Environment> {
  environment: TEnvironment,
  plugin_cache: Rc<PluginCache<TEnvironment>>,
  memory_cache: RefCell<HashMap<PluginCacheKey, Rc<tokio::sync::OnceCell<Rc<PluginWrapper>>>>>,
  wasm_module_creator: WasmModuleCreator,
  next_config_id: IdGenerator,
  config_releases: Rc<ConfigReleaseQueue>,
}

impl<TEnvironment: Environment> PluginResolver<TEnvironment> {
  pub fn new(environment: TEnvironment, plugin_cache: PluginCache<TEnvironment>) -> Self {
    PluginResolver {
      environment,
      plugin_cache: Rc::new(plugin_cache),
      memory_cache: Default::default(),
      wasm_module_creator: Default::default(),
      next_config_id: Default::default(),
      config_releases: Default::default(),
    }
  }

  /// The queue a configuration's last owner pushes it on when it drops.
  pub fn config_release_queue(&self) -> Rc<ConfigReleaseQueue> {
    self.config_releases.clone()
  }

  /// Releases the queued configurations from the plugins that registered
  /// them.
  pub async fn release_queued_configs(&self) {
    for (plugin, config_id) in self.config_releases.take() {
      let Some(instance) = plugin.initialized().await else {
        continue;
      };
      if let Err(err) = instance.release_config(config_id).await {
        log_debug!(self.environment, "Error releasing config {:?} of {}: {:#}", config_id, plugin.info().name, err);
      }
    }
  }

  pub async fn clear_and_shutdown_initialized(&self) {
    self.config_releases.take();
    let plugins = self.memory_cache.borrow_mut().drain().collect::<Vec<_>>();
    let futures = plugins.iter().filter_map(|p| p.1.get()).map(|p| p.shutdown());
    future::join_all(futures).await;
  }

  /// Retires cached plugins only when no scope or request holds their wrapper.
  /// Pending resolutions also retain their cache cell and cannot be retired.
  pub async fn shutdown_unused(&self) {
    let candidates = self
      .memory_cache
      .borrow()
      .iter()
      .map(|(key, cell)| (key.clone(), cell.clone()))
      .collect::<Vec<_>>();
    for (key, cell) in candidates {
      let Some(wrapper) = cell.get() else {
        continue;
      };
      let mut initialized = wrapper.initialized_plugin.lock().await;
      // Cache + this candidate are the only owners. Requests may retain the
      // initialized instance independently of a scope or wrapper.
      if Rc::strong_count(&cell) != 2 || Rc::strong_count(wrapper) != 1 || initialized.as_ref().is_some_and(|plugin| Rc::strong_count(plugin) != 1) {
        continue;
      }
      self.memory_cache.borrow_mut().remove(&key);
      if let Some(plugin) = initialized.take() {
        plugin.shutdown().await;
      }
    }
  }

  pub fn next_config_id(&self) -> FormatConfigId {
    // + 1 because 0 is reserved for uninitialized
    FormatConfigId::from_raw(self.next_config_id.next() + 1)
  }

  /// Resolves exec without inventing an external dependency. It shares the
  /// resolver's lifecycle management with downloaded plugins.
  pub async fn resolve_builtin_exec(&self) -> Rc<PluginWrapper> {
    let cell = self
      .memory_cache
      .borrow_mut()
      .entry(PluginCacheKey::BuiltinExec)
      .or_insert_with(|| Rc::new(tokio::sync::OnceCell::new()))
      .clone();
    cell
      .get_or_init(|| async { Rc::new(PluginWrapper::new(Box::new(ExecFormatter::default()))) })
      .await
      .clone()
  }

  pub async fn resolve_plugins(self: &Rc<Self>, plugin_references: Vec<PluginSourceReference>) -> Result<Vec<Rc<PluginWrapper>>> {
    let handles = plugin_references
      .into_iter()
      .map(|plugin_ref| {
        let resolver = self.clone();
        dprint_async_runtime::spawn(async move { resolver.resolve_plugin(plugin_ref).await })
      })
      .collect::<Vec<_>>();

    let results = future::join_all(handles).await;
    let mut plugins = Vec::with_capacity(results.len());
    for result in results {
      plugins.push(result??);
    }

    Ok(plugins)
  }

  /// Sets up a versioned npm plugin for `dprint add` — downloads, resolves the
  /// plugin file (detecting it when the specifier has no path), computes the
  /// checksum, and warms the cache — returning the resolved path + checksum to
  /// write into config. See [`PluginCache::resolve_npm_for_add`].
  pub async fn resolve_npm_for_add(
    &self,
    specifier: &crate::utils::NpmSpecifier,
    path_was_explicit: bool,
    base_dir: Option<&crate::environment::CanonicalizedPathBuf>,
  ) -> Result<crate::plugins::NpmAddResolution> {
    self.plugin_cache.resolve_npm_for_add(specifier, path_was_explicit, base_dir).await
  }

  /// Downloads a remote plugin for `dprint add`, computes its checksum, and
  /// warms the cache. See [`PluginCache::resolve_remote_for_add`].
  pub async fn resolve_remote_for_add(&self, plugin_reference: &PluginSourceReference) -> Result<String> {
    self.plugin_cache.resolve_remote_for_add(plugin_reference).await
  }

  pub async fn resolve_plugin(&self, plugin_reference: PluginSourceReference) -> Result<Rc<PluginWrapper>> {
    let cell = {
      let mut mem_cache = self.memory_cache.borrow_mut();
      mem_cache
        .entry(PluginCacheKey::Source(plugin_reference.clone()))
        .or_insert_with(|| Rc::new(tokio::sync::OnceCell::new()))
        .clone()
    };
    cell
      .get_or_try_init(|| async {
        if let Some(plugin) = create_builtin_exec_plugin(&self.environment, &plugin_reference) {
          return Ok(Rc::new(PluginWrapper::new(plugin)));
        }
        match create_plugin(&self.plugin_cache, self.environment.clone(), &plugin_reference, &self.wasm_module_creator).await {
          Ok(plugin) => Ok(Rc::new(PluginWrapper::new(plugin))),
          Err(err) => {
            match self.plugin_cache.forget(&plugin_reference).await {
              Ok(()) => {}
              Err(inner_err) => {
                bail!(
                  "Error resolving plugin {} and forgetting from cache: {:#}\n{:#}",
                  plugin_reference.display(),
                  err,
                  inner_err
                )
              }
            }
            Err(err).with_context(|| format!("Error resolving plugin {}", plugin_reference.display()))
          }
        }
      })
      .await
      .cloned()
  }
}

#[cfg(test)]
mod lifecycle_tests {
  use super::*;
  use crate::environment::TestEnvironment;
  use crate::plugins::TestPlugin;

  #[tokio::test]
  async fn direct_exec_is_cached_and_retired_only_after_its_owners_release_it() {
    let environment = TestEnvironment::new();
    let resolver = PluginResolver::new(environment.clone(), PluginCache::new(environment));
    let first = resolver.resolve_builtin_exec().await;
    assert!(Rc::ptr_eq(&first, &resolver.resolve_builtin_exec().await));
    resolver.shutdown_unused().await;
    assert!(Rc::ptr_eq(&first, &resolver.resolve_builtin_exec().await));
    let retired = Rc::downgrade(&first);
    drop(first);
    resolver.shutdown_unused().await;
    assert!(retired.upgrade().is_none());
    let next = resolver.resolve_builtin_exec().await;
    assert_eq!(next.info().config_key, "exec");
    resolver.clear_and_shutdown_initialized().await;
    assert!(!Rc::ptr_eq(&next, &resolver.resolve_builtin_exec().await));
  }

  #[tokio::test]
  async fn a_stopped_wrapper_initializes_a_fresh_instance() {
    let wrapper = PluginWrapper::new(Box::new(TestPlugin::new("test", "test", vec!["txt"], vec![])));
    let first = wrapper.initialize().await.unwrap();
    wrapper.shutdown().await;
    let second = wrapper.initialize().await.unwrap();
    assert!(!Rc::ptr_eq(&first, &second));
    wrapper.shutdown().await;
  }
}
