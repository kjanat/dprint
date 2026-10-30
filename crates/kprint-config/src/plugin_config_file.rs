//! A configuration file of a plugin's own (ex. `tombi.toml`), which dprint
//! parses and passes to the plugin, so the formatter's own executable and
//! dprint can be configured in one place.
//!
//! A plugin's `configFile` property names it, relative to the configuration
//! file's directory, optionally followed by `#` and the dotted path of the
//! part the plugin's configuration is in (ex. `pyproject.toml#tool.ruff`).
//! Without one, the first of the files the plugin declares that exists is
//! used (see `PluginInfo::config_files`).

use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use serde_json::Value as JsonValue;
use toml_edit::Item;
use toml_edit::Value as TomlValue;

/// The plugin property that names the file.
pub const CONFIG_FILE_PROPERTY: &str = "configFile";
/// The plugin property dprint passes the file in, as an object with its
/// `path` and its parsed `content` (a JSON string, so numbers and dates
/// survive what dprint's configuration values can hold).
pub const CONFIG_FILE_CONTENT_PROPERTY: &str = "$configFile";

/// What a `configFile` property names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigFileReference {
  /// The path relative to the configuration file's directory.
  pub path: String,
  /// The keys of the part the plugin's configuration is in, if not the whole
  /// file.
  pub keys: Vec<String>,
}

impl ConfigFileReference {
  pub fn parse(text: &str) -> Result<Self> {
    let (path, keys) = match text.split_once('#') {
      Some((path, keys)) => (path, keys.split('.').map(str::to_string).collect::<Vec<_>>()),
      None => (text, Vec::new()),
    };
    if path.is_empty() {
      bail!("Expected a file path, but found '{}'.", text);
    }
    if keys.iter().any(String::is_empty) {
      bail!("Expected the dotted keys after '#' (ex. 'pyproject.toml#tool.ruff'), but found '{}'.", text);
    }
    Ok(Self { path: path.to_string(), keys })
  }
}

/// Parses the file by its extension: TOML, JSON (with comments), or YAML. A
/// file without one (ex. `.prettierrc`) is parsed as JSON, or else YAML.
pub fn parse_config_file(path: &Path, text: &str) -> Result<JsonValue> {
  let extension = path.extension().and_then(|extension| extension.to_str()).map(str::to_ascii_lowercase);
  match extension.as_deref() {
    Some("toml") => parse_toml(text),
    Some("json" | "jsonc" | "json5") => parse_json(text),
    Some("yaml" | "yml") => parse_yaml(text),
    _ => parse_json(text).or_else(|json_err| parse_yaml(text).map_err(|_| json_err)),
  }
  .with_context(|| format!("Failed parsing {}.", path.display()))
}

/// The part of the file the keys lead to.
pub fn select<'a>(value: &'a JsonValue, reference: &ConfigFileReference) -> Result<&'a JsonValue> {
  let mut current = value;
  for (index, key) in reference.keys.iter().enumerate() {
    current = match current.get(key) {
      Some(value) => value,
      None => bail!("{} has no '{}'.", reference.path, reference.keys[..=index].join(".")),
    };
  }
  Ok(current)
}

fn parse_json(text: &str) -> Result<JsonValue> {
  Ok(jsonc_parser::parse_to_serde_value(text, &Default::default())?)
}

fn parse_yaml(text: &str) -> Result<JsonValue> {
  // an empty document is no configuration
  Ok(serde_norway::from_str::<Option<JsonValue>>(text)?.unwrap_or(JsonValue::Object(Default::default())))
}

fn parse_toml(text: &str) -> Result<JsonValue> {
  let document = text.parse::<toml_edit::DocumentMut>()?;
  Ok(item_to_json(document.as_item()))
}

fn item_to_json(item: &Item) -> JsonValue {
  match item {
    Item::None => JsonValue::Null,
    Item::Value(value) => value_to_json(value),
    Item::Table(table) => JsonValue::Object(table.iter().map(|(key, item)| (key.to_string(), item_to_json(item))).collect()),
    Item::ArrayOfTables(tables) => JsonValue::Array(
      tables
        .iter()
        .map(|table| JsonValue::Object(table.iter().map(|(key, item)| (key.to_string(), item_to_json(item))).collect()))
        .collect(),
    ),
  }
}

fn value_to_json(value: &TomlValue) -> JsonValue {
  match value {
    TomlValue::String(value) => JsonValue::String(value.value().clone()),
    TomlValue::Integer(value) => JsonValue::from(*value.value()),
    // NaN and infinity have no JSON
    TomlValue::Float(value) => serde_json::Number::from_f64(*value.value()).map_or(JsonValue::Null, JsonValue::Number),
    TomlValue::Boolean(value) => JsonValue::Bool(*value.value()),
    TomlValue::Datetime(value) => JsonValue::String(value.value().to_string()),
    TomlValue::Array(array) => JsonValue::Array(array.iter().map(value_to_json).collect()),
    TomlValue::InlineTable(table) => JsonValue::Object(table.iter().map(|(key, value)| (key.to_string(), value_to_json(value))).collect()),
  }
}

#[cfg(test)]
mod test {
  use serde_json::json;

  use super::*;

  #[test]
  fn parses_references() {
    assert_eq!(
      ConfigFileReference::parse("tombi.toml").unwrap(),
      ConfigFileReference {
        path: "tombi.toml".to_string(),
        keys: vec![],
      }
    );
    assert_eq!(
      ConfigFileReference::parse("pyproject.toml#tool.ruff").unwrap(),
      ConfigFileReference {
        path: "pyproject.toml".to_string(),
        keys: vec!["tool".to_string(), "ruff".to_string()],
      }
    );
    assert!(ConfigFileReference::parse("#tool").is_err());
    assert!(ConfigFileReference::parse("pyproject.toml#tool..ruff").is_err());
    assert!(ConfigFileReference::parse("pyproject.toml#").is_err());
  }

  #[test]
  fn parses_toml() {
    let text = r#"
toml-version = "v1.0.0"
[format.rules]
line-width = 140
ratio = 1.5
[[schemas]]
path = "cargo.json"
include = ["Cargo.toml"]
lint.rules = { deprecated = "off" }
"#;
    assert_eq!(
      parse_config_file(Path::new("tombi.toml"), text).unwrap(),
      json!({
        "toml-version": "v1.0.0",
        "format": { "rules": { "line-width": 140, "ratio": 1.5 } },
        "schemas": [{ "path": "cargo.json", "include": ["Cargo.toml"], "lint": { "rules": { "deprecated": "off" } } }],
      })
    );
  }

  #[test]
  fn parses_json_with_comments_and_yaml() {
    let expected = json!({ "printWidth": 100, "semi": false });
    assert_eq!(
      parse_config_file(Path::new(".prettierrc.json"), "{ // width\n \"printWidth\": 100, \"semi\": false }").unwrap(),
      expected
    );
    assert_eq!(
      parse_config_file(Path::new(".prettierrc.yaml"), "printWidth: 100\nsemi: false\n").unwrap(),
      expected
    );
    assert_eq!(parse_config_file(Path::new(".prettierrc.yml"), "").unwrap(), json!({}));
    // without an extension, JSON or else YAML
    assert_eq!(
      parse_config_file(Path::new(".prettierrc"), "{ \"printWidth\": 100, \"semi\": false }").unwrap(),
      expected
    );
    assert_eq!(parse_config_file(Path::new(".prettierrc"), "printWidth: 100\nsemi: false\n").unwrap(), expected);
  }

  #[test]
  fn names_the_file_it_fails_parsing() {
    let err = parse_config_file(Path::new("tombi.toml"), "a = ").unwrap_err();
    assert!(err.to_string().starts_with("Failed parsing tombi.toml."), "{:#}", err);
  }

  #[test]
  fn selects_the_part_the_keys_lead_to() {
    let value = json!({ "tool": { "ruff": { "line-length": 100 } } });
    let reference = ConfigFileReference::parse("pyproject.toml#tool.ruff").unwrap();
    assert_eq!(select(&value, &reference).unwrap(), &json!({ "line-length": 100 }));
    let reference = ConfigFileReference::parse("pyproject.toml#tool.black").unwrap();
    assert_eq!(select(&value, &reference).unwrap_err().to_string(), "pyproject.toml has no 'tool.black'.");
  }
}
