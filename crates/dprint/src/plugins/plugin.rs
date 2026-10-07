use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use anyhow::Result;
use dprint_core::async_runtime::async_trait;
use dprint_core::configuration::ConfigKeyMap;
use dprint_core::configuration::ConfigurationDiagnostic;
use dprint_core::configuration::GlobalConfiguration;
use dprint_core::plugins::CancellationToken;
use dprint_core::plugins::CheckConfigUpdatesMessage;
use dprint_core::plugins::ConfigChange;
use dprint_core::plugins::CriticalFormatError;
use dprint_core::plugins::FileMatchingInfo;
use dprint_core::plugins::FormatConfigId;
use dprint_core::plugins::FormatError;
use dprint_core::plugins::FormatRange;
use dprint_core::plugins::FormatResult;
use dprint_core::plugins::PluginInfo;
use dprint_core::plugins::process::HostFormatCallback;

use super::PluginResolutionCache;
use crate::plugins::PluginSourceReference;

/// A formatter that ships inside dprint instead of being loaded as a plugin.
///
/// It's released with dprint, so it has no version, update URL or schema URL
/// of its own.
pub struct BuiltInFormatter {
  /// Its name, ex. "exec".
  pub name: &'static str,
  /// Identifies how it formats in the incremental cache, in place of a
  /// version. Bump it whenever it can format a file differently for the same
  /// configuration. Upgrading dprint then reformats the files it formats, and
  /// only when this changes.
  pub cache_revision: u32,
  /// Name of the external plugin whose references it serves, ex.
  /// "dprint-plugin-exec". Configuration tooling uses this name for it, since
  /// that's the plugin a configuration file refers to.
  pub serves_plugin: &'static str,
  /// Whether a reference is to the external plugin it serves, in any version.
  pub refers_to_served_plugin: fn(&PluginSourceReference) -> bool,
}

/// Looks for a [`CriticalFormatError`] in an `anyhow::Error`, whether it was
/// stored directly or wrapped in a [`FormatError`].
pub fn maybe_critical_format_error(err: &anyhow::Error) -> Option<&CriticalFormatError> {
  if let Some(critical) = err.downcast_ref::<CriticalFormatError>() {
    Some(critical)
  } else {
    err.downcast_ref::<FormatError>().and_then(|err| err.downcast_ref::<CriticalFormatError>())
  }
}

/// Converts an `anyhow::Error` into a [`FormatError`] while preserving a
/// possible [`CriticalFormatError`] so it can still be detected later.
pub fn anyhow_to_format_error(err: anyhow::Error) -> FormatError {
  match err.downcast::<FormatError>() {
    Ok(err) => err,
    Err(err) => match err.downcast::<CriticalFormatError>() {
      Ok(critical) => FormatError::from(critical),
      Err(err) => FormatError::new(err),
    },
  }
}

#[async_trait(?Send)]
pub trait Plugin {
  fn info(&self) -> &PluginInfo;

  /// Initializes the plugin.
  async fn initialize(&self) -> Result<Rc<dyn InitializedPlugin>>;

  /// Gets if this is a process plugin.
  fn is_process_plugin(&self) -> bool;

  /// The schema of the plugin's configuration when it's built into dprint, so
  /// it isn't downloaded from the plugin info's `config_schema_url`.
  fn config_schema(&self) -> Option<&'static str> {
    None
  }

  /// Where what the plugin resolved configurations to is kept, for a plugin
  /// whose resolution only depends on the configuration.
  fn resolution_cache(&self) -> Option<&PluginResolutionCache> {
    None
  }

  /// Set when the plugin is a formatter built into dprint.
  fn built_in(&self) -> Option<&'static BuiltInFormatter> {
    None
  }

  /// Loads existing native code without compiling, so failed cache recovery
  /// can be counted before a format run starts.
  async fn prepare_format_engine(&self) -> Result<()> {
    Ok(())
  }

  /// Whether how the plugin formats depends on how much it formats, which
  /// is the case for a Wasm plugin without native code.
  fn chooses_format_engine(&self) -> bool {
    false
  }

  /// Chooses how the plugin formats in this run, from the bytes of the
  /// files it will format.
  fn choose_format_engine(&self, _bytes_to_format: u64) {}

  /// Whether the plugin is compiled to native code before it formats, as
  /// `choose_format_engine` chose. Compiling takes up to seconds of every
  /// core.
  fn compiles_to_format(&self) -> bool {
    false
  }
}

/// Name of the plugin a configuration file refers to for `plugin`. For a
/// built-in, that's the external plugin it serves references to.
pub fn referenced_plugin_name(plugin: &dyn Plugin) -> &str {
  match plugin.built_in() {
    Some(built_in) => built_in.serves_plugin,
    None => &plugin.info().name,
  }
}

pub struct FormatConfig {
  pub id: FormatConfigId,
  pub plugin: ConfigKeyMap,
  pub global: GlobalConfiguration,
}

pub struct InitializedPluginFormatRequest {
  pub file_path: PathBuf,
  pub file_text: Vec<u8>,
  pub range: FormatRange,
  pub config: Arc<FormatConfig>,
  pub override_config: ConfigKeyMap,
  pub on_host_format: HostFormatCallback,
  pub token: Arc<dyn CancellationToken>,
}

#[async_trait(?Send)]
pub trait InitializedPlugin {
  /// Gets the license text
  async fn license_text(&self) -> Result<String>;
  /// Gets the configuration as a collection of key value pairs.
  async fn resolved_config(&self, config: Arc<FormatConfig>) -> Result<String>;
  /// Gets the configuration's file matching info.
  async fn file_matching_info(&self, config: Arc<FormatConfig>) -> Result<FileMatchingInfo>;
  /// Gets the configuration diagnostics.
  async fn config_diagnostics(&self, config: Arc<FormatConfig>) -> Result<Vec<ConfigurationDiagnostic>>;
  /// Checks for any configuration changes based on the provided plugin config.
  async fn check_config_updates(&self, message: CheckConfigUpdatesMessage) -> Result<Vec<ConfigChange>>;
  /// Formats the text in memory based on the file path and file text.
  async fn format_text(&self, format_request: InitializedPluginFormatRequest) -> FormatResult;
  /// Shuts down the plugin. This is used for process plugins.
  async fn shutdown(&self) -> ();
}

#[cfg(test)]
pub struct TestPlugin {
  info: PluginInfo,
  built_in: Option<&'static BuiltInFormatter>,
  initialized_test_plugin: InitializedTestPlugin,
}

#[cfg(test)]
impl TestPlugin {
  pub fn new(name: &str, config_key: &str, file_extensions: Vec<&str>, file_names: Vec<&str>) -> TestPlugin {
    TestPlugin {
      info: PluginInfo {
        name: name.to_string(),
        version: "1.0.0".to_string(),
        config_key: config_key.to_string(),
        help_url: "https://dprint.dev/plugins/test".to_string(),
        config_schema_url: "https://plugins.dprint.dev/schemas/test.json".to_string(),
        update_url: None,
      },
      built_in: None,
      initialized_test_plugin: InitializedTestPlugin(FileMatchingInfo {
        file_extensions: file_extensions.into_iter().map(String::from).collect(),
        file_names: file_names.into_iter().map(String::from).collect(),
        additive: false,
      }),
    }
  }

  /// Makes it a built-in formatter.
  pub fn built_in(mut self, built_in: &'static BuiltInFormatter) -> Self {
    self.built_in = Some(built_in);
    self
  }

  pub fn with_version(mut self, version: &str) -> Self {
    self.info.version = version.to_string();
    self
  }
}

#[cfg(test)]
#[async_trait(?Send)]
impl Plugin for TestPlugin {
  fn info(&self) -> &PluginInfo {
    &self.info
  }

  fn built_in(&self) -> Option<&'static BuiltInFormatter> {
    self.built_in
  }

  fn is_process_plugin(&self) -> bool {
    false
  }

  async fn initialize(&self) -> Result<Rc<dyn InitializedPlugin>> {
    let test_plugin: Rc<dyn InitializedPlugin> = Rc::new(self.initialized_test_plugin.clone());
    Ok(test_plugin)
  }
}

#[cfg(test)]
#[derive(Clone)]
pub struct InitializedTestPlugin(FileMatchingInfo);

#[cfg(test)]
#[async_trait(?Send)]
impl InitializedPlugin for InitializedTestPlugin {
  async fn license_text(&self) -> Result<String> {
    Ok(String::from("License Text"))
  }

  async fn resolved_config(&self, _config: Arc<FormatConfig>) -> Result<String> {
    Ok(String::from("{}"))
  }

  async fn file_matching_info(&self, _config: Arc<FormatConfig>) -> Result<FileMatchingInfo> {
    Ok(self.0.clone())
  }

  async fn config_diagnostics(&self, _config: Arc<FormatConfig>) -> Result<Vec<ConfigurationDiagnostic>> {
    Ok(vec![])
  }

  async fn check_config_updates(&self, _message: CheckConfigUpdatesMessage) -> Result<Vec<ConfigChange>> {
    Ok(Vec::new())
  }

  async fn format_text(&self, format_request: InitializedPluginFormatRequest) -> FormatResult {
    Ok(Some(format!("{}_formatted", String::from_utf8(format_request.file_text)?).into_bytes()))
  }

  async fn shutdown(&self) -> () {
    // do nothing
  }
}
