use dprint_config_model::PluginOverride;
use dprint_config_model::PluginTable;
use dprint_core::configuration::ConfigKeyMap;
use dprint_core::configuration::ConfigKeyValue;
use indexmap::IndexMap;

use crate::utils::PathSource;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RawPluginConfigOverride {
  pub files: Vec<String>,
  pub properties: ConfigKeyMap,
  /// The configuration file it's from, so a diagnostic about one of its
  /// properties can say which file to change.
  pub origin: ValueOrigin,
}

impl From<PluginOverride> for RawPluginConfigOverride {
  fn from(override_config: PluginOverride) -> Self {
    RawPluginConfigOverride {
      files: override_config.files.into(),
      properties: override_config.plugin.0,
      origin: Default::default(),
    }
  }
}

/// Where a value is from, for diagnostics. It isn't part of the value: the
/// same value from different files is equal.
#[derive(Clone, Debug, Default)]
pub struct ValueOrigin(pub Option<PathSource>);

impl PartialEq for ValueOrigin {
  fn eq(&self, _other: &Self) -> bool {
    true
  }
}

impl Eq for ValueOrigin {}

/// Unresolved plugin configuration.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct RawPluginConfig {
  pub associations: Option<Vec<String>>,
  pub locked: bool,
  pub overrides: Vec<RawPluginConfigOverride>,
  pub properties: ConfigKeyMap,
}

/// A plugin's table as a configuration file has it, as what's combined with
/// the tables of the other configuration files.
impl From<PluginTable> for RawPluginConfig {
  fn from(table: PluginTable) -> Self {
    RawPluginConfig {
      associations: table.associations.map(Into::into),
      locked: table.locked.unwrap_or(false),
      overrides: table
        .overrides
        .map(Vec::<PluginOverride>::from)
        .unwrap_or_default()
        .into_iter()
        .map(Into::into)
        .collect(),
      properties: table.plugin,
    }
  }
}

/// A root property of a configuration file as the configuration files are
/// combined: the global configuration's values (ex. `lineWidth`), and each
/// plugin's table by its key.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ConfigMapValue {
  KeyValue(ConfigKeyValue),
  PluginConfig(RawPluginConfig),
}

#[cfg(test)]
impl ConfigMapValue {
  pub fn from_i32(value: i32) -> ConfigMapValue {
    ConfigMapValue::KeyValue(ConfigKeyValue::from_i32(value))
  }

  pub fn from_str(value: &str) -> ConfigMapValue {
    ConfigMapValue::KeyValue(ConfigKeyValue::from_str(value))
  }

  pub fn from_bool(value: bool) -> ConfigMapValue {
    ConfigMapValue::KeyValue(ConfigKeyValue::from_bool(value))
  }
}

pub type ConfigMap = IndexMap<String, ConfigMapValue>;
