//! What a dprint configuration file may hold, as types, which its schema is
//! generated from.
//!
//! These describe the file's syntax: what dprint itself reads from it (see
//! `config_layer.rs` and `get_global_config.rs` in the dprint crate, which
//! read these properties by name) and the shape every plugin's table has.
//! What a plugin's own properties are is the plugin's schema's to say (see
//! [`crate::compose`]).

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Map;
use serde_json::Value;

/// Where the website serves the schema of a configuration file without the
/// plugins' schemas.
pub const ROOT_SCHEMA_ID: &str = "https://dprint.dev/schemas/v0.json";

/// Schema for a dprint configuration file.
#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(title = "dprint configuration file", extend("$id" = ROOT_SCHEMA_ID, "allowTrailingCommas" = true))]
pub struct ConfigFile {
  /// The JSON schema reference. Normally you shouldn't bother to provide this as the dprint vscode editor extension will handle constructing the schema for you based on the plugins provided.
  #[serde(rename = "$schema", skip_serializing_if = "Option::is_none")]
  pub schema: Option<String>,
  /// Whether to format files only when they change.
  #[serde(skip_serializing_if = "Option::is_none")]
  #[schemars(extend("default" = true))]
  pub incremental: Option<bool>,
  /// For a nested (directory specific) configuration file, whether to inherit the plugins and configuration of the ancestor configuration file. Has no effect on a root configuration file.
  #[serde(skip_serializing_if = "Option::is_none")]
  #[schemars(extend("default" = false))]
  pub inherit: Option<bool>,
  /// Configurations to extend.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub extends: Option<Extends>,
  /// The width of a line the printer will try to stay under. Note that the printer may exceed this width in certain cases.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub line_width: Option<u32>,
  /// The number of characters for an indent.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub indent_width: Option<u8>,
  /// Whether to use tabs (true) or spaces (false) for indentation.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub use_tabs: Option<bool>,
  /// The kind of newline to use.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub new_line_kind: Option<NewLineKind>,
  /// Array of patterns (globs) to use to find files to format.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub includes: Option<Vec<String>>,
  /// Array of patterns (globs) to exclude files or directories to format.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub excludes: Option<Vec<String>>,
  /// Maps a shebang line (ex. "#!/usr/bin/env bash") to a file extension so extensionless scripts can be routed to a plugin.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub shebangs: Option<Shebangs>,
  /// Array of plugin URLs to format files.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub plugins: Option<Vec<String>>,
  /// Plugin configuration.
  #[serde(flatten)]
  pub plugin_tables: BTreeMap<String, PluginTable>,
}

impl ConfigFile {
  /// The properties dprint itself reads from a configuration file (the
  /// others are the plugins' tables), which the schema has to describe.
  pub const PROPERTIES: &[&str] = &[
    "$schema",
    "incremental",
    "inherit",
    "extends",
    "lineWidth",
    "indentWidth",
    "useTabs",
    "newLineKind",
    "includes",
    "excludes",
    "shebangs",
    "plugins",
  ];
}

/// Configurations to extend.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
#[schemars(inline)]
pub enum Extends {
  /// A file path or url to a configuration file to extend.
  One(String),
  /// A collection of file paths and/or urls to configuration files to extend.
  Many(Vec<String>),
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub enum NewLineKind {
  /// For each file, uses the newline kind found at the end of the last line.
  Auto,
  /// Uses carriage return, line feed.
  Crlf,
  /// Uses line feed.
  Lf,
  /// Uses the system standard (ex. crlf on Windows).
  System,
}

/// Shebang lines (ex. "#!/usr/bin/env bash") to file extensions.
#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
#[schemars(inline, extend("propertyNames" = { "pattern": "^#!" }))]
pub struct Shebangs(pub BTreeMap<String, ShebangExtension>);

/// The file extension to treat matching files as (ex. "sh" or ".sh").
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[schemars(inline)]
pub struct ShebangExtension(pub String);

/// Plugin configuration.
#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
#[schemars(rename = "pluginTable")]
pub struct PluginTable {
  /// Whether this plugin configuration is locked against overrides from extending configurations.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub locked: Option<bool>,
  /// File patterns to associate with this plugin, in addition to the file extensions and file names it matches by default. Use a negated glob (ex. "!**/*.js") to stop matching a default extension or file name.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub associations: Option<Associations>,
  /// Plugin configuration overrides for specific file patterns.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub overrides: Option<Overrides>,
  /// The plugin's own configuration, which its schema describes.
  #[serde(flatten)]
  pub plugin: Map<String, Value>,
}

impl PluginTable {
  /// dprint's properties of every plugin's table, which a plugin's schema
  /// doesn't describe.
  pub const PROPERTIES: &[&str] = &["locked", "associations", "overrides"];
}

/// File patterns, one or more.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
#[schemars(inline)]
pub enum Associations {
  One(String),
  Many(Vec<String>),
}

/// Overrides of a plugin's configuration, one or more.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
#[schemars(inline)]
pub enum Overrides {
  One(PluginOverride),
  Many(Vec<PluginOverride>),
}

/// An override of a plugin's configuration for some of its files.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[schemars(rename = "pluginOverride", extend("minProperties" = 2))]
pub struct PluginOverride {
  /// File patterns this override applies to.
  pub files: OverrideFiles,
  /// The plugin's own configuration for the files, which its schema describes.
  #[serde(flatten)]
  pub plugin: Map<String, Value>,
}

/// File patterns, one or more.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
#[schemars(inline)]
pub enum OverrideFiles {
  One(String),
  #[schemars(extend("minItems" = 1))]
  Many(Vec<String>),
}

/// The schema of a configuration file without the plugins' schemas: what
/// the website serves at [`ROOT_SCHEMA_ID`], and what `dprint schema`
/// composes the plugins' schemas into.
pub fn root_schema() -> Value {
  crate::schema_for::<ConfigFile>()
}

#[cfg(test)]
mod test {
  use pretty_assertions::assert_eq;
  use serde_json::json;

  use super::*;

  /// The schema as published, which the website serves.
  const PUBLISHED: &str = include_str!("../schema/v0.json");
  const PUBLISHED_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/schema/v0.json");

  #[test]
  fn the_published_schema_is_the_generated_one() {
    let generated = crate::schema_json_for::<ConfigFile>();
    if std::env::var_os("UPDATE_CONFIG_SCHEMA").is_some() {
      std::fs::write(PUBLISHED_PATH, &generated).unwrap();
    }
    assert_eq!(
      PUBLISHED, generated,
      "schema/v0.json isn't the generated schema. Run `UPDATE_CONFIG_SCHEMA=1 cargo test -p dprint-config-schema` to regenerate it."
    );
  }

  #[test]
  fn describes_dprints_properties_and_the_plugins_tables() {
    let schema = root_schema();
    assert_eq!(schema["$schema"], json!("https://json-schema.org/draft/2020-12/schema"));
    assert_eq!(schema["$id"], json!(ROOT_SCHEMA_ID));
    assert_eq!(schema["allowTrailingCommas"], json!(true));
    assert_eq!(
      schema["properties"].as_object().unwrap().keys().collect::<Vec<_>>(),
      ConfigFile::PROPERTIES.to_vec()
    );
    assert_eq!(schema["properties"]["incremental"]["default"], json!(true));
    assert_eq!(
      schema["properties"]["lineWidth"],
      json!({ "description": schema["properties"]["lineWidth"]["description"], "type": "integer", "minimum": 0 })
    );
    assert_eq!(
      schema["properties"]["newLineKind"]["oneOf"][1],
      json!({ "description": "Uses carriage return, line feed.", "type": "string", "const": "crlf" })
    );
    assert_eq!(
      schema["properties"]["shebangs"],
      json!({
        "description": schema["properties"]["shebangs"]["description"],
        "type": "object",
        "additionalProperties": { "description": "The file extension to treat matching files as (ex. \"sh\" or \".sh\").", "type": "string" },
        "propertyNames": { "pattern": "^#!" }
      })
    );
    // every other property is a plugin's table
    assert_eq!(schema["additionalProperties"], json!({ "$ref": "#/$defs/pluginTable" }));
    let table = &schema["$defs"]["pluginTable"];
    assert_eq!(
      table["properties"].as_object().unwrap().keys().collect::<Vec<_>>(),
      PluginTable::PROPERTIES.to_vec()
    );
    assert_eq!(table["additionalProperties"], json!(true));
    assert_eq!(
      table["properties"]["overrides"]["anyOf"],
      json!([{ "$ref": "#/$defs/pluginOverride" }, { "type": "array", "items": { "$ref": "#/$defs/pluginOverride" } }])
    );
    let override_schema = &schema["$defs"]["pluginOverride"];
    assert_eq!(override_schema["required"], json!(["files"]));
    assert_eq!(override_schema["minProperties"], json!(2));
    assert_eq!(override_schema["additionalProperties"], json!(true));
    assert_eq!(
      override_schema["properties"]["files"]["anyOf"],
      json!([{ "type": "string" }, { "type": "array", "items": { "type": "string" }, "minItems": 1 }])
    );
    // nothing is `null`, and no integer has a Rust width
    fn check(value: &Value) {
      match value {
        Value::Object(object) => {
          assert_ne!(object.get("type"), Some(&json!("null")), "{}", value);
          assert!(object.get("format").is_none(), "{}", value);
          object.values().for_each(check);
        }
        Value::Array(values) => values.iter().for_each(check),
        _ => {}
      }
    }
    check(&schema);
  }

  #[test]
  fn reads_and_writes_a_configuration_file() {
    let text = json!({
      "$schema": ROOT_SCHEMA_ID,
      "lineWidth": 100,
      "newLineKind": "lf",
      "shebangs": { "#!/usr/bin/env bash": "sh" },
      "plugins": ["https://plugins.dprint.dev/typescript-0.96.1.wasm"],
      "typescript": { "locked": true, "semiColons": "asNeeded", "overrides": [{ "files": "*.test.ts", "semiColons": "always" }] }
    });
    let file: ConfigFile = serde_json::from_value(text.clone()).unwrap();
    assert_eq!(file.line_width, Some(100));
    assert!(matches!(file.new_line_kind, Some(NewLineKind::Lf)));
    let typescript = &file.plugin_tables["typescript"];
    assert_eq!(typescript.locked, Some(true));
    assert_eq!(typescript.plugin["semiColons"], json!("asNeeded"));
    assert!(matches!(&typescript.overrides, Some(Overrides::Many(overrides)) if overrides[0].plugin["semiColons"] == json!("always")));
    assert_eq!(serde_json::to_value(&file).unwrap(), text);
  }
}
