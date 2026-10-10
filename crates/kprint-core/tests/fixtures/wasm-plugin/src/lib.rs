#[cfg(feature = "facade")]
use legacy::generate_plugin_code;
#[cfg(feature = "facade")]
use legacy::plugins as api;
#[cfg(feature = "direct")]
use sdk as api;
#[cfg(feature = "direct")]
use sdk::generate_plugin_code;

use configuration::ConfigKeyMap;
use configuration::GlobalConfiguration;

type Configuration = bool;
struct Plugin;

impl api::SyncPluginHandler<Configuration> for Plugin {
  fn resolve_config(&mut self, _config: ConfigKeyMap, _global: &GlobalConfiguration) -> api::PluginResolveConfigurationResult<Configuration> {
    api::PluginResolveConfigurationResult {
      config: true,
      diagnostics: Vec::new(),
      file_matching: api::FileMatchingInfo {
        file_extensions: vec!["txt".into()],
        file_names: Vec::new(),
        additive: false,
      },
    }
  }

  fn plugin_info(&mut self) -> api::PluginInfo {
    api::PluginInfo {
      name: "fixture".into(),
      version: "0.0.0".into(),
      config_key: "fixture".into(),
      help_url: String::new(),
      config_schema_url: String::new(),
      update_url: None,
    }
  }

  fn license_text(&mut self) -> String {
    "MIT".into()
  }

  fn check_config_updates(&self, _message: api::CheckConfigUpdatesMessage) -> Result<Vec<api::ConfigChange>, api::FormatError> {
    Ok(Vec::new())
  }

  fn format(
    &mut self,
    _request: api::SyncFormatRequest<Configuration>,
    _host: impl FnMut(api::SyncHostFormatRequest) -> api::FormatResult,
  ) -> api::FormatResult {
    Ok(None)
  }
}

generate_plugin_code!(Plugin, Plugin);
