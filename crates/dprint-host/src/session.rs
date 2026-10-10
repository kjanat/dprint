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
use crate::environment::HostEnvironment as Environment;
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
  /// frontend requests before replacing the previous configuration.
  pub async fn resolve_config(&self, config: ResolvedConfig, on_change: impl FnOnce()) -> Result<Rc<PluginsScope<TEnvironment>>> {
    let cell = {
      let mut scopes = self.plugins_scope_by_config.borrow_mut();
      scopes.entry(config.origin.source.display()).or_default().clone()
    };
    self.refresh_scope(config, cell, on_change).await
  }

  async fn refresh_scope(&self, config: ResolvedConfig, cell: Rc<ScopeCell<TEnvironment>>, on_change: impl FnOnce()) -> Result<Rc<PluginsScope<TEnvironment>>> {
    let mut cell = cell.lock().await;
    // Requests that finished since the last resolution dropped their scopes.
    self.plugin_resolver.release_queued_configs().await;
    if let Some(existing_scope) = cell.as_ref()
      && existing_scope.config.as_deref() == Some(&config)
    {
      return Ok(existing_scope.clone());
    }

    // Resolve before invalidating the last valid scope: plugin setup can fail.
    let new_scope = Rc::new(resolve_plugins_scope(Rc::new(config), &self.environment, &self.plugin_resolver).await?);
    if cell.is_some() {
      on_change();
    }
    *cell = Some(new_scope.clone());
    self.plugin_resolver.release_queued_configs().await;
    // Other scopes and outstanding requests retain their wrappers. They must
    // keep running even when a different configuration changes.
    self.plugin_resolver.shutdown_unused().await;
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
  use dprint_plugin_types::FormatConfigId;
  use std::cell::Cell;

  fn session(environment: &TestEnvironment) -> HostSession<TestEnvironment> {
    HostSession::new(
      environment.clone(),
      Rc::new(PluginResolver::new(environment.clone(), PluginCache::new(environment.clone()))),
      None,
    )
  }

  /// A scope with an initialized test plugin, and the configurations the
  /// plugin released.
  async fn test_plugin_scope(
    session: &HostSession<TestEnvironment>,
    environment: &TestEnvironment,
  ) -> (Rc<PluginsScope<TestEnvironment>>, Rc<RefCell<Vec<FormatConfigId>>>) {
    use crate::configuration::ConfigOrigin;
    use crate::configuration::FileRouting;
    use crate::environment::CanonicalizedPathBuf;
    use crate::resolution::PluginWithConfig;
    use crate::resolution::PluginWithConfigOptions;
    use dprint_plugin_types::FileMatchingInfo;

    let plugin = crate::plugins::TestPlugin::new("test-plugin", "test-plugin", vec!["txt"], vec![]);
    let released = plugin.released_configs();
    let wrapper = Rc::new(crate::plugins::PluginWrapper::new(Box::new(plugin)));
    wrapper.initialize().await.unwrap();
    let base_path = CanonicalizedPathBuf::new_for_testing("/");
    let config = Rc::new(ResolvedConfig {
      origin: ConfigOrigin {
        source: PathSource::new_local(base_path.join_panic_relative("dprint.json")),
        base_path,
        is_global: false,
      },
      files: Default::default(),
      routing: FileRouting { shebangs: None },
      execution: Default::default(),
      plugins: Default::default(),
    });
    let plugin = Rc::new(PluginWithConfig::new(
      wrapper,
      PluginWithConfigOptions {
        associations: None,
        format_config: std::sync::Arc::new(crate::plugins::FormatConfig {
          id: FormatConfigId::from_raw(7),
          plugin: Default::default(),
          global: Default::default(),
        }),
        file_matching: FileMatchingInfo {
          file_extensions: vec!["txt".to_string()],
          file_names: vec![],
          additive: false,
        },
        overrides: Vec::new(),
        serialized_resolved_config: "{}".to_string(),
        property_origins: Default::default(),
        release_queue: session.plugin_resolver.config_release_queue(),
      },
    ));
    let scope = Rc::new(PluginsScope::new(environment.clone(), vec![plugin], config, Vec::new()).unwrap());
    (scope, released)
  }

  #[test]
  fn a_dropped_scope_releases_its_configs_at_the_next_resolution() {
    let environment = TestEnvironment::new();
    environment.write_file("/dprint.json", "{}").unwrap();
    environment.clone().run_in_runtime(async move {
      let session = session(&environment);
      let options = SessionOptions {
        config: Some(ConfigArg::PathOrUrl("/dprint.json".into())),
        ..Default::default()
      };
      let (scope, released) = test_plugin_scope(&session, &environment).await;

      let plugin = scope.get_plugin("test-plugin");
      drop(scope);
      session.resolve_from_options(&options, || {}).await.unwrap();
      assert_eq!(*released.borrow(), Vec::<FormatConfigId>::new());

      drop(plugin);
      assert_eq!(*released.borrow(), Vec::<FormatConfigId>::new());
      session.resolve_from_options(&options, || {}).await.unwrap();
      assert_eq!(*released.borrow(), vec![FormatConfigId::from_raw(7)]);
      session.shutdown().await;
    });
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
  async fn format_process(scope: &Rc<PluginsScope<TestEnvironment>>, ending: &str) {
    let result = scope
      .format(dprint_plugin_types::HostFormatRequest {
        file_path: PathBuf::from("/file.txt_ps"),
        file_bytes: b"text".to_vec(),
        range: None,
        override_config: Default::default(),
        token: std::sync::Arc::new(dprint_plugin_types::NullCancellationToken),
      })
      .await
      .unwrap();
    assert_eq!(result, Some(format!("text_{ending}").into_bytes()));
  }

  #[test]
  fn changing_one_config_preserves_another_scope_and_in_flight_owners() {
    let environment = crate::environment::TestEnvironmentBuilder::with_initialized_remote_process_plugin().build();
    let config = environment.read_file("/dprint.json").unwrap();
    environment.write_file("/a.json", &config).unwrap();
    environment.write_file("/b.json", &config).unwrap();
    environment.clone().run_in_runtime(async move {
      let session = session(&environment);
      let options = |path: &str| SessionOptions {
        config: Some(ConfigArg::PathOrUrl(path.into())),
        ..Default::default()
      };
      let a_config = resolve_config_from_args(&options("/a.json"), &environment).await.unwrap();
      let b_config = resolve_config_from_args(&options("/b.json"), &environment).await.unwrap();
      let first_a = session.resolve_config(a_config, || {}).await.unwrap();
      let b = session.resolve_config(b_config, || {}).await.unwrap();
      let b_plugin = b.get_plugin("test-process-plugin").plugin.initialize().await.unwrap();
      format_process(&first_a, "formatted_process").await;
      format_process(&b, "formatted_process").await;
      let mut text: serde_json::Value = serde_json::from_str(&config).unwrap();
      text["testProcessPlugin"] = serde_json::json!({"ending": "changed"});
      environment.write_file("/a.json", &text.to_string()).unwrap();
      let changed = resolve_config_from_args(&options("/a.json"), &environment).await.unwrap();
      let second_a = session.resolve_config(changed, || {}).await.unwrap();
      format_process(&second_a, "changed").await;
      format_process(&b, "formatted_process").await;
      // A request that retained the old A scope can still finish as well.
      format_process(&first_a, "formatted_process").await;
      assert!(Rc::ptr_eq(&b_plugin, &b.get_plugin("test-process-plugin").plugin.initialize().await.unwrap()));
      session.shutdown().await;
    });
  }

  #[test]
  fn failed_plugin_setup_preserves_the_valid_scope_without_cancelling() {
    let environment = crate::environment::TestEnvironmentBuilder::with_initialized_remote_process_plugin().build();
    let config = environment.read_file("/dprint.json").unwrap();
    environment.clone().run_in_runtime(async move {
      let session = session(&environment);
      let options = SessionOptions::default();
      let first = session.resolve_from_options(&options, || {}).await.unwrap();
      environment.write_file("/dprint.json", r#"{"plugins":["/missing.wasm"]}"#).unwrap();
      assert!(
        session
          .resolve_from_options(&options, || panic!("failed setup cannot cancel a valid scope"))
          .await
          .is_err()
      );
      format_process(&first, "formatted_process").await;
      environment.write_file("/dprint.json", &config).unwrap();
      let restored = session
        .resolve_from_options(&options, || panic!("last valid scope remains cached"))
        .await
        .unwrap();
      assert!(Rc::ptr_eq(&first, &restored));
      session.shutdown().await;
    });
  }
}
