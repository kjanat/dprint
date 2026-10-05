//! The format-neutral reading of a configuration file. A JSON or TOML file is
//! read into dprint-core's [`ConfigKeyMap`] (see `config_file_format.rs`), and
//! everything after that works the same for both.

use anyhow::Result;
use anyhow::bail;
use dprint_core::configuration::ConfigKeyMap;
use dprint_core::configuration::ConfigKeyValue;
use jsonc_parser::JsonValue;

use super::ConfigMap;
use super::ConfigMapValue;
use super::RawPluginConfig;
use super::RawPluginConfigOverride;

/// Reads JSON (with comments) configuration text. Text without an object at
/// its root has no properties.
pub fn parse_json_config(config_file_text: &str) -> Result<ConfigKeyMap> {
  let value = jsonc_parser::parse_to_value(config_file_text, &Default::default())?;
  match value {
    Some(JsonValue::Object(obj)) => {
      let mut properties = ConfigKeyMap::new();
      for (key, value) in obj.into_iter() {
        let value = json_value_to_config_value(value, &key)?;
        properties.insert(key, value);
      }
      Ok(properties)
    }
    _ => Ok(Default::default()),
  }
}

fn json_value_to_config_value(value: JsonValue, path: &str) -> Result<ConfigKeyValue> {
  Ok(match value {
    JsonValue::Boolean(value) => ConfigKeyValue::Bool(value),
    JsonValue::String(value) => ConfigKeyValue::String(value.into_owned()),
    JsonValue::Number(value) => ConfigKeyValue::Number(parse_integer(value, path)?),
    JsonValue::Array(values) => ConfigKeyValue::Array(values.into_iter().map(|value| json_value_to_config_value(value, path)).collect::<Result<_>>()?),
    JsonValue::Object(obj) => {
      let mut properties = ConfigKeyMap::new();
      for (key, value) in obj.into_iter() {
        let value = json_value_to_config_value(value, &format!("{} -> {}", path, key))?;
        properties.insert(key, value);
      }
      ConfigKeyValue::Object(properties)
    }
    JsonValue::Null => ConfigKeyValue::Null,
  })
}

/// Configuration numbers are 32-bit integers, whatever the format allows.
/// `path` is the property's path (ex. `typescript -> lineWidth`).
pub fn parse_integer(text: &str, path: &str) -> Result<i32> {
  match text.parse::<i32>() {
    Ok(value) => Ok(value),
    Err(err) => bail!(
      "Expected property '{}' with value '{}' to be convertible to a signed integer. {}",
      path,
      text,
      err
    ),
  }
}

/// Reads the root properties of a configuration file: plugin configurations
/// (objects), arrays of strings and other values.
pub fn config_map_from_values(values: ConfigKeyMap) -> Result<ConfigMap> {
  let mut properties = ConfigMap::new();
  for (property_name, value) in values {
    let property_value = match value {
      ConfigKeyValue::Object(obj) => ConfigMapValue::PluginConfig(raw_plugin_config(obj)?),
      ConfigKeyValue::Array(values) => ConfigMapValue::Vec(string_vec(&property_name, values)?),
      ConfigKeyValue::Null => bail!("Unexpected null value in root object property '{}'", property_name),
      value => ConfigMapValue::KeyValue(value),
    };
    properties.insert(property_name, property_value);
  }
  Ok(properties)
}

/// Reads a plugin's configuration, separating the properties dprint handles
/// (`locked`, `associations` and `overrides`) from the plugin's own.
pub fn raw_plugin_config(obj: ConfigKeyMap) -> Result<RawPluginConfig> {
  let mut properties = ConfigKeyMap::new();
  let mut locked = false;
  let mut associations = None;
  let mut overrides = Vec::new();

  for (property_name, value) in obj {
    match property_name.as_str() {
      "locked" => match value {
        ConfigKeyValue::Bool(value) => locked = value,
        _ => bail!("The 'locked' property in a plugin configuration must be a boolean."),
      },
      "associations" => match value {
        ConfigKeyValue::Array(values) => {
          let mut items = Vec::with_capacity(values.len());
          for value in values {
            match value {
              ConfigKeyValue::String(value) => items.push(value),
              _ => bail!("The 'associations' array in a plugin configuration must contain only strings."),
            }
          }
          associations = Some(items);
        }
        ConfigKeyValue::String(value) => associations = Some(vec![value]),
        _ => bail!("The 'associations' property in a plugin configuration must be a string or an array of strings."),
      },
      "overrides" => {
        overrides = match value {
          ConfigKeyValue::Object(value) => vec![raw_plugin_config_override(value)?],
          ConfigKeyValue::Array(values) => {
            let mut items = Vec::with_capacity(values.len());
            for value in values {
              match value {
                ConfigKeyValue::Object(value) => items.push(raw_plugin_config_override(value)?),
                _ => bail!("The 'overrides' property in a plugin configuration must be an object or an array of objects."),
              }
            }
            items
          }
          _ => bail!("The 'overrides' property in a plugin configuration must be an object or an array of objects."),
        };
      }
      _ => {
        properties.insert(property_name, value);
      }
    }
  }

  Ok(RawPluginConfig {
    locked,
    associations,
    overrides,
    properties,
  })
}

fn raw_plugin_config_override(obj: ConfigKeyMap) -> Result<RawPluginConfigOverride> {
  let mut files = None;
  let mut properties = ConfigKeyMap::new();

  for (key, value) in obj {
    if key == "files" {
      files = Some(match value {
        ConfigKeyValue::Array(values) => {
          let mut items = Vec::with_capacity(values.len());
          for value in values {
            match value {
              ConfigKeyValue::String(value) => items.push(value),
              _ => bail!("The 'files' array in a plugin configuration override must contain only strings."),
            }
          }
          items
        }
        ConfigKeyValue::String(value) => vec![value],
        _ => bail!("The 'files' property in a plugin configuration override must be a string or an array of strings."),
      });
    } else {
      properties.insert(key, value);
    }
  }

  let files = match files {
    Some(files) => files,
    None => bail!("A plugin configuration override must specify a 'files' property."),
  };
  if files.is_empty() {
    bail!("A plugin configuration override must specify at least one file pattern.");
  }
  if properties.is_empty() {
    bail!("A plugin configuration override must specify at least one configuration property.");
  }

  Ok(RawPluginConfigOverride { files, properties })
}

/// Reads an array that may only contain strings.
pub fn string_vec(parent_prop_name: &str, values: Vec<ConfigKeyValue>) -> Result<Vec<String>> {
  let mut elements = Vec::with_capacity(values.len());
  for value in values {
    match value {
      ConfigKeyValue::String(value) => elements.push(value),
      _ => bail!("Expected a string in array '{}'", parent_prop_name),
    }
  }
  Ok(elements)
}

#[cfg(test)]
mod tests {
  use super::config_map_from_values;
  use super::parse_json_config;
  use crate::configuration::ConfigMap;
  use crate::configuration::ConfigMapValue;
  use crate::configuration::RawPluginConfig;
  use crate::configuration::RawPluginConfigOverride;

  use dprint_core::configuration::ConfigKeyMap;
  use dprint_core::configuration::ConfigKeyValue;
  use pretty_assertions::assert_eq;

  #[test]
  fn should_error_when_there_is_a_parser_error() {
    assert_error("{prop}", "Unexpected token on line 1 column 2");
  }

  #[test]
  fn should_not_error_when_no_object_in_root() {
    assert_deserializes("", ConfigMap::new());
    assert_deserializes("[]", ConfigMap::new());
  }

  #[test]
  fn should_error_when_the_root_property_has_an_unexpected_value_type() {
    assert_error("{'prop': null}", "Unexpected null value in root object property 'prop'");
  }

  #[test]
  fn should_deserialize_empty_object() {
    assert_deserializes("{}", ConfigMap::new());
  }

  #[test]
  fn should_deserialize_full_object() {
    let mut expected_props = ConfigMap::new();
    expected_props.insert(String::from("includes"), ConfigMapValue::Vec(Vec::new()));
    expected_props.insert(
      String::from("typescript"),
      ConfigMapValue::PluginConfig(RawPluginConfig {
        locked: false,
        associations: None,
        overrides: Vec::new(),
        properties: ConfigKeyMap::from([
          (String::from("lineWidth"), ConfigKeyValue::from_i32(40)),
          (String::from("preferSingleLine"), ConfigKeyValue::from_bool(true)),
          (String::from("other"), ConfigKeyValue::from_str("test")),
          (
            String::from("obj"),
            ConfigKeyValue::Object(ConfigKeyMap::from([(String::from("prop"), ConfigKeyValue::from_i32(5))])),
          ),
          (
            String::from("array"),
            ConfigKeyValue::Array(vec![ConfigKeyValue::from_i32(1), ConfigKeyValue::Null]),
          ),
        ]),
      }),
    );
    assert_deserializes(
      "{'includes': [], 'typescript': { 'lineWidth': 40, 'preferSingleLine': true, 'other': 'test', 'obj': { 'prop': 5 }, 'array': [1, null] }}",
      expected_props,
    );
  }

  #[test]
  fn should_deserialize_cli_specific_plugin_config() {
    let expected_props = ConfigMap::from([
      (
        "typescript".to_string(),
        ConfigMapValue::PluginConfig(RawPluginConfig {
          locked: true,
          associations: Some(vec!["test".to_string()]),
          overrides: Vec::new(),
          properties: ConfigKeyMap::from([("lineWidth".to_string(), ConfigKeyValue::from_i32(40))]),
        }),
      ),
      (
        "other".to_string(),
        ConfigMapValue::PluginConfig(RawPluginConfig {
          locked: false,
          associations: Some(vec!["other".to_string(), "test".to_string()]),
          overrides: Vec::new(),
          properties: ConfigKeyMap::new(),
        }),
      ),
    ]);
    assert_deserializes(
      "{'typescript': { 'lineWidth': 40, locked: true, associations: 'test' }, 'other': { 'locked': false, 'associations': ['other', 'test'] }}",
      expected_props,
    );
  }

  #[test]
  fn error_invalid_cli_specific_properties() {
    assert_error(
      "{'typescript': { 'associations': [1] }}",
      "The 'associations' array in a plugin configuration must contain only strings.",
    );
    assert_error(
      "{'typescript': { 'associations': 1 }}",
      "The 'associations' property in a plugin configuration must be a string or an array of strings.",
    );
    assert_error(
      "{'typescript': { locked: 1 }}",
      "The 'locked' property in a plugin configuration must be a boolean.",
    );
  }

  #[test]
  fn should_deserialize_plugin_config_overrides() {
    let expected_props = ConfigMap::from([(
      "typescript".to_string(),
      ConfigMapValue::PluginConfig(RawPluginConfig {
        locked: false,
        associations: None,
        overrides: vec![RawPluginConfigOverride {
          files: vec!["**/package.json".to_string(), "**/composer.json".to_string()],
          properties: ConfigKeyMap::from([
            ("indentWidth".to_string(), ConfigKeyValue::from_i32(4)),
            ("useTabs".to_string(), ConfigKeyValue::from_bool(false)),
          ]),
        }],
        properties: ConfigKeyMap::from([("lineWidth".to_string(), ConfigKeyValue::from_i32(80))]),
      }),
    )]);

    assert_deserializes(
      "{'typescript': { 'lineWidth': 80, 'overrides': { 'files': ['**/package.json', '**/composer.json'], 'indentWidth': 4, 'useTabs': false } }}",
      expected_props,
    );
  }

  #[test]
  fn should_deserialize_plugin_config_overrides_array() {
    let expected_props = ConfigMap::from([(
      "typescript".to_string(),
      ConfigMapValue::PluginConfig(RawPluginConfig {
        locked: false,
        associations: None,
        overrides: vec![
          RawPluginConfigOverride {
            files: vec!["**/package.json".to_string()],
            properties: ConfigKeyMap::from([("indentWidth".to_string(), ConfigKeyValue::from_i32(4))]),
          },
          RawPluginConfigOverride {
            files: vec!["**/special-package.json".to_string()],
            properties: ConfigKeyMap::from([("lineWidth".to_string(), ConfigKeyValue::from_i32(80))]),
          },
        ],
        properties: ConfigKeyMap::new(),
      }),
    )]);

    assert_deserializes(
      "{'typescript': { 'overrides': [{ 'files': '**/package.json', 'indentWidth': 4 }, { 'files': ['**/special-package.json'], 'lineWidth': 80 }] }}",
      expected_props,
    );
  }

  #[test]
  fn error_invalid_plugin_config_overrides() {
    assert_error(
      "{'typescript': { 'overrides': 5 }}",
      "The 'overrides' property in a plugin configuration must be an object or an array of objects.",
    );
    assert_error(
      "{'typescript': { 'overrides': [{ 'files': [1], 'indentWidth': 4 }] }}",
      "The 'files' array in a plugin configuration override must contain only strings.",
    );
    assert_error(
      "{'typescript': { 'overrides': [{ 'files': 5, 'indentWidth': 4 }] }}",
      "The 'files' property in a plugin configuration override must be a string or an array of strings.",
    );
    assert_error(
      "{'typescript': { 'overrides': [{ 'indentWidth': 4 }] }}",
      "A plugin configuration override must specify a 'files' property.",
    );
    assert_error(
      "{'typescript': { 'overrides': [{ 'files': [], 'indentWidth': 4 }] }}",
      "A plugin configuration override must specify at least one file pattern.",
    );
    assert_error(
      "{'typescript': { 'overrides': [{ 'files': '**/package.json' }] }}",
      "A plugin configuration override must specify at least one configuration property.",
    );
  }

  #[test]
  fn should_have_stable_deserialization_of_config_properties() {
    for _ in 0..10 {
      let config = deserialize_config(
        r#"{
        "exec": {
          "commands": [{
            "command": "rustfmt --edition 2024 --config imports_granularity=item",
            "exts": ["rs"]
          }]
        }
      }"#,
      )
      .unwrap();
      match config.get("exec").unwrap() {
        ConfigMapValue::PluginConfig(plugin) => {
          let commands = plugin.properties.get("commands").unwrap().as_array().unwrap();
          assert_eq!(commands.len(), 1);
          let obj = commands[0].as_object().unwrap();
          let mut values = obj.into_iter();
          assert_eq!(values.next().unwrap().0, "command");
          assert_eq!(values.next().unwrap().0, "exts");
          assert!(values.next().is_none());
        }
        _ => unreachable!(),
      }
    }
  }

  fn deserialize_config(text: &str) -> anyhow::Result<ConfigMap> {
    config_map_from_values(parse_json_config(text)?)
  }

  fn assert_deserializes(text: &str, expected_map: ConfigMap) {
    match deserialize_config(text) {
      Ok(result) => assert_eq!(result, expected_map),
      Err(err) => panic!("Errored, but that was not expected. {:#}", err),
    }
  }

  fn assert_error(text: &str, expected_err: &str) {
    match deserialize_config(text) {
      Ok(_) => panic!("Did not error, but that was expected."),
      Err(err) => assert_eq!(err.to_string(), expected_err),
    }
  }
}
