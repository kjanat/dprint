use crate::GlobalSettings;
use crate::NewLineKind;
use dprint_configuration::ConfigKeyMap;
use dprint_configuration::ConfigurationDiagnostic;
use dprint_configuration::GlobalConfiguration;

use super::ConfigMap;
use super::ConfigMapValue;

pub enum GlobalConfigDiagnostic {
  UnknownProperty(ConfigurationDiagnostic),
  Other(ConfigurationDiagnostic),
}

impl std::fmt::Display for GlobalConfigDiagnostic {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      GlobalConfigDiagnostic::UnknownProperty(diagnostic) => diagnostic.fmt(f),
      GlobalConfigDiagnostic::Other(diagnostic) => diagnostic.fmt(f),
    }
  }
}

pub struct GlobalConfigurationResult {
  pub config: GlobalConfiguration,
  pub diagnostics: Vec<GlobalConfigDiagnostic>,
}

/// Resolves the global configuration from the root of the combined
/// configuration files, which by now should only hold values (the plugins'
/// tables having been taken out): the global configuration as the model of a
/// configuration file has it, and whatever else is there, which is unknown.
pub fn get_global_config(config_map: ConfigMap) -> GlobalConfigurationResult {
  let mut diagnostics = Vec::new();
  let values = root_values(&mut diagnostics, config_map);
  let known = global_property_names();
  let config = match crate::from_values::<GlobalSettings>(values.clone()) {
    Ok(settings) => settings.into(),
    Err(err) => {
      diagnostics.push(GlobalConfigDiagnostic::Other(ConfigurationDiagnostic {
        property_name: err.path,
        message: err.message,
      }));
      GlobalConfiguration::default()
    }
  };
  diagnostics.extend(values.into_keys().filter(|key| !known.contains_key(key)).map(|key| {
    GlobalConfigDiagnostic::UnknownProperty(ConfigurationDiagnostic {
      property_name: key,
      message: "Unknown property in configuration".to_string(),
    })
  }));
  GlobalConfigurationResult { config, diagnostics }
}

/// The names of the global configuration's properties as a configuration
/// spells them, from the model: every property set, so that one the model
/// gains has to be set here too.
fn global_property_names() -> ConfigKeyMap {
  crate::to_values(&GlobalSettings {
    line_width: Some(1),
    indent_width: Some(1),
    use_tabs: Some(true),
    new_line_kind: Some(NewLineKind::Auto),
  })
  .expect("the model serializes")
}

fn root_values(diagnostics: &mut Vec<GlobalConfigDiagnostic>, config_map: ConfigMap) -> ConfigKeyMap {
  let mut values = ConfigKeyMap::new();
  for (key, value) in config_map.into_iter() {
    if let ConfigMapValue::KeyValue(value) = value {
      values.insert(key, value);
    } else {
      diagnostics.push(GlobalConfigDiagnostic::UnknownProperty(ConfigurationDiagnostic {
        property_name: key,
        message: "Unexpected non-string, boolean, or int property".to_string(),
      }));
    }
  }
  values
}

#[cfg(test)]
mod tests {
  use dprint_configuration::NewLineKind;

  use super::*;
  use crate::configuration::ConfigMap;

  #[test]
  fn should_get_global_config() {
    let mut config_map = ConfigMap::new();
    config_map.insert(String::from("lineWidth"), ConfigMapValue::from_i32(80));
    config_map.insert(String::from("useTabs"), ConfigMapValue::from_bool(true));
    config_map.insert(String::from("indentWidth"), ConfigMapValue::from_i32(2));
    config_map.insert(String::from("newLineKind"), ConfigMapValue::from("crlf"));
    assert_result(
      config_map,
      GlobalConfiguration {
        line_width: Some(80),
        use_tabs: Some(true),
        indent_width: Some(2),
        new_line_kind: Some(NewLineKind::CarriageReturnLineFeed),
      },
      &[],
    );
  }

  #[test]
  fn should_get_global_for_system_new_line_kind() {
    let mut config_map = ConfigMap::new();
    config_map.insert(String::from("newLineKind"), ConfigMapValue::from("system"));
    assert_result(
      config_map,
      GlobalConfiguration {
        line_width: None,
        use_tabs: None,
        indent_width: None,
        new_line_kind: Some(if cfg!(windows) {
          NewLineKind::CarriageReturnLineFeed
        } else {
          NewLineKind::LineFeed
        }),
      },
      &[],
    );
  }

  #[test]
  fn should_diagnostic_on_unexpected_object_properties() {
    let mut config_map = ConfigMap::new();
    config_map.insert(String::from("test"), ConfigMapValue::PluginConfig(Default::default()));
    assert_result(
      config_map,
      GlobalConfiguration::default(),
      &["Unexpected non-string, boolean, or int property (test)"],
    );
  }

  #[test]
  fn should_diagnostic_on_unknown_props() {
    let mut config_map = ConfigMap::new();
    config_map.insert(String::from("lineWidth"), ConfigMapValue::from_i32(80));
    config_map.insert(String::from("unknownProperty"), ConfigMapValue::from_i32(80));
    assert_result(
      config_map,
      GlobalConfiguration {
        line_width: Some(80),
        ..Default::default()
      },
      &["Unknown property in configuration (unknownProperty)"],
    );
  }

  #[test]
  fn should_diagnostic_on_a_value_that_isnt_what_it_may_be() {
    let mut config_map = ConfigMap::new();
    config_map.insert(String::from("lineWidth"), ConfigMapValue::from("test"));
    assert_result(
      config_map,
      GlobalConfiguration::default(),
      &["invalid type: string \"test\", expected u32 (lineWidth)"],
    );
  }

  #[track_caller]
  fn assert_result(config_map: ConfigMap, global_config: GlobalConfiguration, diagnostics: &[&str]) {
    let result = get_global_config(config_map);
    assert_eq!(result.config, global_config);
    assert_eq!(
      result.diagnostics.into_iter().map(|d| d.to_string()).collect::<Vec<_>>(),
      diagnostics.iter().map(|d| d.to_string()).collect::<Vec<_>>()
    );
  }
}
