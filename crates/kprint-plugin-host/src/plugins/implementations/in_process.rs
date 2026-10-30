use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use anyhow::Result;
use kprint_async_runtime::async_trait;
use kprint_configuration::ConfigKeyMap;
use kprint_configuration::ConfigurationDiagnostic;
use kprint_configuration::GlobalConfiguration;
use kprint_plugin_types::CheckConfigUpdatesMessage;
use kprint_plugin_types::ConfigChange;
use kprint_plugin_types::FileMatchingInfo;
use kprint_plugin_types::FormatConfigId;
use kprint_plugin_types::FormatRequest;
use kprint_plugin_types::FormatResult;
use kprint_plugin_types::PluginInfo;
use kprint_plugin_types::PluginResolveConfigurationResult;
use serde::Serialize;

use crate::environment::CanonicalizedPathBuf;
use crate::plugins::BuiltInFormatter;
use crate::plugins::FormatConfig;
use crate::plugins::InitializedPlugin;
use crate::plugins::InitializedPluginFormatRequest;
use crate::plugins::Plugin;

/// A formatter that runs inside the dprint process.
#[async_trait(?Send)]
pub trait InProcessHandler: 'static {
  type Configuration: Serialize + Clone + Send + Sync;

  fn plugin_info(&self) -> PluginInfo;
  fn license_text(&self) -> String;
  /// Resolves a configuration whose file patterns are relative to `base_dir`.
  async fn resolve_config(
    &self,
    config: ConfigKeyMap,
    global_config: GlobalConfiguration,
    base_dir: &CanonicalizedPathBuf,
  ) -> PluginResolveConfigurationResult<Self::Configuration>;
  async fn format(&self, request: FormatRequest<Self::Configuration>) -> FormatResult;
}

/// Runs an [`InProcessHandler`] inside the dprint process.
pub struct InProcessPlugin<THandler: InProcessHandler> {
  info: PluginInfo,
  // a constructor rather than the handler itself because a plugin must be
  // `Send + Sync`, which handlers needn't be
  create_handler: fn() -> THandler,
  config_schema: &'static str,
  built_in: &'static BuiltInFormatter,
}

impl<THandler: InProcessHandler> InProcessPlugin<THandler> {
  pub fn new(create_handler: fn() -> THandler, config_schema: &'static str, built_in: &'static BuiltInFormatter) -> Self {
    Self {
      info: create_handler().plugin_info(),
      create_handler,
      config_schema,
      built_in,
    }
  }
}

#[async_trait(?Send)]
impl<THandler: InProcessHandler> Plugin for InProcessPlugin<THandler> {
  fn info(&self) -> &PluginInfo {
    &self.info
  }

  async fn initialize(&self) -> Result<Rc<dyn InitializedPlugin>> {
    Ok(Rc::new(InitializedInProcessPlugin {
      handler: (self.create_handler)(),
      configs: Default::default(),
    }))
  }

  fn is_process_plugin(&self) -> bool {
    false
  }

  fn is_sandboxed(&self) -> bool {
    false
  }

  fn config_schema(&self) -> Option<&'static str> {
    Some(self.config_schema)
  }

  fn built_in(&self) -> Option<&'static BuiltInFormatter> {
    Some(self.built_in)
  }
}

struct ResolvedConfig<TConfiguration> {
  config: Arc<TConfiguration>,
  file_matching: FileMatchingInfo,
  diagnostics: Vec<ConfigurationDiagnostic>,
}

struct InitializedInProcessPlugin<THandler: InProcessHandler> {
  handler: THandler,
  configs: RefCell<HashMap<FormatConfigId, Rc<ResolvedConfig<THandler::Configuration>>>>,
}

impl<THandler: InProcessHandler> InitializedInProcessPlugin<THandler> {
  async fn resolve(&self, config: &FormatConfig) -> Rc<ResolvedConfig<THandler::Configuration>> {
    if let Some(resolved) = self.configs.borrow().get(&config.id) {
      return resolved.clone();
    }
    let result = self
      .handler
      .resolve_config(config.plugin.clone(), config.global.clone(), &config.base_dir)
      .await;
    let resolved = Rc::new(ResolvedConfig {
      config: Arc::new(result.config),
      file_matching: result.file_matching,
      diagnostics: result.diagnostics,
    });
    self.configs.borrow_mut().insert(config.id, resolved.clone());
    resolved
  }
}

#[async_trait(?Send)]
impl<THandler: InProcessHandler> InitializedPlugin for InitializedInProcessPlugin<THandler> {
  async fn license_text(&self) -> Result<String> {
    Ok(self.handler.license_text())
  }

  async fn resolved_config(&self, config: Arc<FormatConfig>) -> Result<String> {
    Ok(serde_json::to_string(&*self.resolve(&config).await.config)?)
  }

  async fn file_matching_info(&self, config: Arc<FormatConfig>) -> Result<FileMatchingInfo> {
    Ok(self.resolve(&config).await.file_matching.clone())
  }

  async fn config_diagnostics(&self, config: Arc<FormatConfig>) -> Result<Vec<ConfigurationDiagnostic>> {
    Ok(self.resolve(&config).await.diagnostics.clone())
  }

  async fn check_config_updates(&self, _message: CheckConfigUpdatesMessage) -> Result<Vec<ConfigChange>> {
    Ok(Vec::new())
  }

  async fn format_text(&self, request: InitializedPluginFormatRequest) -> FormatResult {
    let config = if request.override_config.is_empty() {
      self.resolve(&request.config).await.config.clone()
    } else {
      let mut config_map: ConfigKeyMap = request.config.plugin.clone();
      for (key, value) in request.override_config {
        config_map.insert(key, value);
      }
      Arc::new(
        self
          .handler
          .resolve_config(config_map, request.config.global.clone(), &request.config.base_dir)
          .await
          .config,
      )
    };
    self
      .handler
      .format(FormatRequest {
        file_path: request.file_path,
        file_bytes: request.file_text,
        config_id: request.config.id,
        config,
        range: request.range,
        token: request.token,
      })
      .await
  }

  async fn release_config(&self, config_id: FormatConfigId) -> Result<()> {
    self.configs.borrow_mut().remove(&config_id);
    Ok(())
  }

  async fn shutdown(&self) {
    // nothing runs in the background
  }
}
