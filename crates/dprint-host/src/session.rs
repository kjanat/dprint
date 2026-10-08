use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;

use anyhow::Context;
use anyhow::Result;

use crate::configuration::ResolvedConfig;
use crate::configuration::ResolvedConfigPathWithText;
use crate::configuration::get_default_config_file_in_ancestor_directories;
use crate::configuration::resolve_config_from_args;
use crate::configuration::resolve_config_from_path_with_bytes;
use crate::configuration::resolve_global_config_path_and_text;
use crate::environment::Environment;
use crate::plugins;
use crate::resolution::PluginsScope;
use crate::resolution::resolve_plugins_scope;
use crate::utils::AsyncMutex;
use crate::utils::PathSource;
use dprint_config::options::ConfigOptions;

type ScopeCell<TEnvironment> = AsyncMutex<Option<Rc<PluginsScope<TEnvironment>>>>;

pub struct HostSession<TEnvironment: Environment> {
  environment: TEnvironment,
  plugin_resolver: Rc<plugins::PluginResolver<TEnvironment>>,
  plugins_scope_by_config: RefCell<HashMap<String, Rc<ScopeCell<TEnvironment>>>>,
  config_override: Option<PathBuf>,
  options_scope: Rc<ScopeCell<TEnvironment>>,
}

impl<TEnvironment: Environment> HostSession<TEnvironment> {
  pub fn new(environment: TEnvironment, plugin_resolver: Rc<plugins::PluginResolver<TEnvironment>>, config_override: Option<PathBuf>) -> Self {
    Self {
      environment,
      plugin_resolver,
      plugins_scope_by_config: Default::default(),
      config_override,
      options_scope: Default::default(),
    }
  }

  pub async fn shutdown(&self) {
    self.plugins_scope_by_config.borrow_mut().clear();
    self.options_scope.lock().await.take();
    self.plugin_resolver.clear_and_shutdown_initialized().await;
  }

  pub async fn resolve_by_path(&self, dir_path: &Path) -> Result<Option<Rc<PluginsScope<TEnvironment>>>> {
    let config_file_bytes = if let Some(path) = &self.config_override {
      let path = self.environment.canonicalize(path).context("failed resolving --config path")?;
      let content = self.environment.read_file(&path).context("failed resolving --config path")?;
      Some(ResolvedConfigPathWithText {
        base_path: path.parent().unwrap_or_else(|| path.clone()),
        source: PathSource::new_local(path),
        is_first_download: false,
        content,
        is_global_config: false,
      })
    } else {
      match get_default_config_file_in_ancestor_directories(&self.environment, dir_path)? {
        Some(config) => Some(config),
        None => resolve_global_config_path_and_text(&self.environment)?,
      }
    };
    let Some(config_file_bytes) = config_file_bytes else {
      return Ok(None);
    };
    let config = resolve_config_from_path_with_bytes(&config_file_bytes, &self.environment).await?;
    self.resolve_config(config, || {}).await.map(Some)
  }

  /// Resolves an explicit frontend request, refreshing changed configuration.
  pub async fn resolve_from_options(&self, options: &dyn ConfigOptions, on_change: impl FnOnce()) -> Result<Rc<PluginsScope<TEnvironment>>> {
    let config = resolve_config_from_args(options, &self.environment).await?;
    self.refresh_scope(config, self.options_scope.clone(), on_change).await
  }

  /// Reuses a scope with the same effective configuration. The callback cancels
  /// frontend requests before plugins from the previous configuration stop.
  pub async fn resolve_config(&self, config: ResolvedConfig, on_change: impl FnOnce()) -> Result<Rc<PluginsScope<TEnvironment>>> {
    let cell = {
      let mut scopes = self.plugins_scope_by_config.borrow_mut();
      scopes.entry(config.origin.source.display()).or_default().clone()
    };
    self.refresh_scope(config, cell, on_change).await
  }

  async fn refresh_scope(&self, config: ResolvedConfig, cell: Rc<ScopeCell<TEnvironment>>, on_change: impl FnOnce()) -> Result<Rc<PluginsScope<TEnvironment>>> {
    let mut cell = cell.lock().await;
    if let Some(existing_scope) = cell.as_ref() {
      if existing_scope.config.as_deref() == Some(&config) {
        return Ok(existing_scope.clone());
      }
      on_change();
      // for simplicity, shut down all plugins when any config
      // changes in order to do some cleanup
      self.plugin_resolver.clear_and_shutdown_initialized().await;
    }

    let new_scope = Rc::new(resolve_plugins_scope(Rc::new(config), &self.environment, &self.plugin_resolver).await?);
    let _ = cell.insert(new_scope.clone());
    Ok(new_scope)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::environment::FileSystemEnvironment;
  use crate::environment::TestEnvironment;
  use crate::plugins::PluginCache;
  use crate::plugins::PluginResolver;
  use dprint_config::options::ConfigArg;
  use dprint_config::options::SessionOptions;
  use std::cell::Cell;

  fn session(environment: &TestEnvironment) -> HostSession<TestEnvironment> {
    HostSession::new(
      environment.clone(),
      Rc::new(PluginResolver::new(environment.clone(), PluginCache::new(environment.clone()))),
      None,
    )
  }

  #[test]
  fn unchanged_config_reuses_a_scope_across_frontend_requests() {
    let environment = TestEnvironment::new();
    environment.write_file("/dprint.json", "{}").unwrap();
    environment.clone().run_in_runtime(async move {
      let session = session(&environment);
      let options = SessionOptions {
        config: Some(ConfigArg::PathOrUrl("/dprint.json".into())),
        ..Default::default()
      };
      let first = session
        .resolve_from_options(&options, || panic!("first resolution has nothing to cancel"))
        .await
        .unwrap();
      let second = session
        .resolve_from_options(&options, || panic!("unchanged configuration must not invalidate requests"))
        .await
        .unwrap();
      assert!(Rc::ptr_eq(&first, &second));
      let by_path = session.resolve_by_path(Path::new("/")).await.unwrap().unwrap();
      assert_eq!(first.config, by_path.config);
      session.shutdown().await;
    });
  }

  #[test]
  fn changing_config_cancels_requests_once_and_replaces_the_scope() {
    let environment = TestEnvironment::new();
    environment.write_file("/dprint.json", "{}").unwrap();
    environment.clone().run_in_runtime(async move {
      let session = session(&environment);
      let mut options = SessionOptions {
        config: Some(ConfigArg::PathOrUrl("/dprint.json".into())),
        ..Default::default()
      };
      let first = session.resolve_from_options(&options, || {}).await.unwrap();
      environment.write_file("/other.json", r#"{"lineWidth": 120}"#).unwrap();
      options.config = Some(ConfigArg::PathOrUrl("/other.json".into()));
      let cancelled = Cell::new(0);
      let second = session.resolve_from_options(&options, || cancelled.set(cancelled.get() + 1)).await.unwrap();
      assert_eq!(cancelled.get(), 1);
      assert!(!Rc::ptr_eq(&first, &second));
      assert_ne!(first.config, second.config);
      let third = session.resolve_from_options(&options, || cancelled.set(cancelled.get() + 1)).await.unwrap();
      assert!(Rc::ptr_eq(&second, &third));
      assert_eq!(cancelled.get(), 1);
      session.shutdown().await;
    });
  }

  #[test]
  fn an_invalid_refresh_does_not_cancel_or_discard_the_last_valid_scope() {
    let environment = TestEnvironment::new();
    environment.write_file("/dprint.json", "{}").unwrap();
    environment.clone().run_in_runtime(async move {
      let session = session(&environment);
      let options = SessionOptions {
        config: Some(ConfigArg::PathOrUrl("/dprint.json".into())),
        ..Default::default()
      };
      let first = session.resolve_from_options(&options, || {}).await.unwrap();
      environment.write_file("/dprint.json", "{").unwrap();
      assert!(
        session
          .resolve_from_options(&options, || panic!("invalid configuration must not cancel requests"))
          .await
          .is_err()
      );
      environment.write_file("/dprint.json", "{}").unwrap();
      let restored = session
        .resolve_from_options(&options, || panic!("last valid scope should remain cached"))
        .await
        .unwrap();
      assert!(Rc::ptr_eq(&first, &restored));
      session.shutdown().await;
    });
  }
}
