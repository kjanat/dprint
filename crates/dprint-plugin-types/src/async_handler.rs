use serde::Serialize;

use crate::*;
use dprint_async_runtime::LocalBoxFuture;
use dprint_configuration::ConfigKeyMap;
use dprint_configuration::GlobalConfiguration;

#[derive(Debug)]
pub struct HostFormatRequest {
  pub file_path: std::path::PathBuf,
  pub file_bytes: Vec<u8>,
  /// Range to format.
  pub range: FormatRange,
  pub override_config: ConfigKeyMap,
  pub token: std::sync::Arc<dyn CancellationToken>,
}

pub struct FormatRequest<TConfiguration> {
  pub file_path: std::path::PathBuf,
  pub file_bytes: Vec<u8>,
  pub config_id: FormatConfigId,
  pub config: std::sync::Arc<TConfiguration>,
  /// Range to format.
  pub range: FormatRange,
  pub token: std::sync::Arc<dyn CancellationToken>,
}

/// Transport-independent asynchronous formatter handler.
#[dprint_async_runtime::async_trait(?Send)]
pub trait AsyncPluginHandler: 'static {
  type Configuration: Serialize + Clone + Send + Sync;

  /// Gets the plugin's plugin info.
  fn plugin_info(&self) -> PluginInfo;
  /// Gets the plugin's license text.
  fn license_text(&self) -> String;
  /// Resolves configuration based on the provided config map and global configuration.
  async fn resolve_config(&self, config: ConfigKeyMap, global_config: GlobalConfiguration) -> PluginResolveConfigurationResult<Self::Configuration>;
  /// Updates the config key map. This will be called after the CLI has upgraded the
  /// plugin in `dprint config update`.
  async fn check_config_updates(&self, _message: CheckConfigUpdatesMessage) -> Result<Vec<ConfigChange>, FormatError> {
    Ok(Vec::new())
  }
  /// Formats the provided file text based on the provided file path and configuration.
  async fn format(
    &self,
    request: FormatRequest<Self::Configuration>,
    format_with_host: impl FnMut(HostFormatRequest) -> LocalBoxFuture<'static, FormatResult> + 'static,
  ) -> FormatResult;
}

/// Calls the host to format a nested document, independently of transport.
pub type HostFormatCallback = std::rc::Rc<dyn Fn(HostFormatRequest) -> LocalBoxFuture<'static, FormatResult>>;
