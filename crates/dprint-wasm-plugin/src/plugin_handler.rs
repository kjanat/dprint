use dprint_configuration::ConfigKeyMap;
use dprint_configuration::GlobalConfiguration;
use dprint_plugin_types::*;

#[derive(Debug)]
pub struct SyncHostFormatRequest<'a> {
  pub file_path: &'a std::path::Path,
  pub file_bytes: &'a [u8],
  /// Range to format.
  pub range: FormatRange,
  pub override_config: &'a ConfigKeyMap,
}

pub struct SyncFormatRequest<'a, TConfiguration> {
  pub file_path: &'a std::path::Path,
  pub file_bytes: Vec<u8>,
  pub config_id: FormatConfigId,
  pub config: &'a TConfiguration,
  /// Range to format.
  pub range: FormatRange,
  pub token: &'a dyn CancellationToken,
}

/// Trait for implementing a Wasm plugin.
pub trait SyncPluginHandler<TConfiguration: Clone + serde::Serialize> {
  /// Resolves configuration based on the provided config map and global configuration.
  fn resolve_config(&mut self, config: ConfigKeyMap, global_config: &GlobalConfiguration) -> PluginResolveConfigurationResult<TConfiguration>;
  /// Gets the plugin's plugin info.
  fn plugin_info(&mut self) -> PluginInfo;
  /// Gets the plugin's license text.
  fn license_text(&mut self) -> String;
  /// Updates the config key map. This will be called after the CLI has upgraded the
  /// plugin in `dprint config update`.
  fn check_config_updates(&self, message: CheckConfigUpdatesMessage) -> Result<Vec<ConfigChange>, FormatError>;
  /// Formats the provided file text based on the provided file path and configuration.
  fn format(&mut self, request: SyncFormatRequest<TConfiguration>, format_with_host: impl FnMut(SyncHostFormatRequest) -> FormatResult) -> FormatResult;
}
