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

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ConfigMapValue {
  KeyValue(ConfigKeyValue),
  PluginConfig(RawPluginConfig),
  Vec(Vec<String>),
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
