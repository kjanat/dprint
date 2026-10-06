//! What a dprint configuration file may hold, as types.
//!
//! These are what a configuration file is read into (see
//! [`crate::from_values`]) and, with the `schema` feature, what its schema is
//! generated from: the names, types and shapes of dprint's own properties and
//! of every plugin's table, so what dprint accepts and what the schema says
//! are one definition. What a plugin's own properties are is the plugin's
//! schema's to say (see [`crate::compose`]), and what a configuration means
//! once its files are combined is dprint's (see `config_layer.rs` and
//! `resolve_config.rs` in the dprint crate, which convert these into that).

use std::fmt;

use dprint_core::configuration::ConfigKeyMap;
use dprint_core::configuration::ConfigKeyValue;
use indexmap::IndexMap;
#[cfg(feature = "schema")]
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::de::Error as _;
use serde::de::MapAccess;
use serde::de::SeqAccess;
use serde::de::Visitor;
use serde::de::value::MapAccessDeserializer;
use serde::de::value::SeqAccessDeserializer;

use crate::values::tracked;

/// Where the website serves the schema of a configuration file without the
/// plugins' schemas.
pub const ROOT_SCHEMA_ID: &str = "https://dprint.dev/schemas/v0.json";

/// Schema for a dprint configuration file.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(
  feature = "schema",
  derive(JsonSchema),
  schemars(title = "dprint configuration file", extend("$id" = ROOT_SCHEMA_ID, "allowTrailingCommas" = true))
)]
#[serde(rename_all = "camelCase")]
pub struct ConfigFile {
  /// The JSON schema reference. Normally you shouldn't bother to provide this as the dprint vscode editor extension will handle constructing the schema for you based on the plugins provided.
  #[serde(rename = "$schema", skip_serializing_if = "Option::is_none")]
  pub schema: Option<String>,
  /// Whether to format files only when they change.
  #[serde(skip_serializing_if = "Option::is_none")]
  #[cfg_attr(feature = "schema", schemars(extend("default" = true)))]
  pub incremental: Option<bool>,
  /// For a nested (directory specific) configuration file, whether to inherit the plugins and configuration of the ancestor configuration file. Has no effect on a root configuration file.
  #[serde(skip_serializing_if = "Option::is_none")]
  #[cfg_attr(feature = "schema", schemars(extend("default" = false)))]
  pub inherit: Option<bool>,
  /// Configurations to extend.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub extends: Option<Extends>,
  /// The global configuration, which every plugin gets.
  #[serde(flatten, deserialize_with = "tracked")]
  pub global: GlobalSettings,
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
  /// An old property that's no longer used, which is still accepted so old
  /// configuration files keep working. Not in the schema, so editors don't
  /// offer it.
  #[serde(skip_serializing_if = "Option::is_none")]
  #[cfg_attr(feature = "schema", schemars(skip))]
  pub project_type: Option<ConfigKeyValue>,
  /// The plugins' tables, by the plugin's configuration key: every property
  /// that isn't one of dprint's.
  #[serde(flatten, deserialize_with = "tracked")]
  pub plugin_tables: IndexMap<String, PluginTable>,
}

/// The configuration every plugin gets, which a plugin's table may set
/// differently for the plugin.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct GlobalSettings {
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
}

/// Configurations to extend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema), schemars(inline))]
#[serde(untagged, expecting = "Extends in configuration must be a string or an array of strings.")]
pub enum Extends {
  /// A file path or url to a configuration file to extend.
  One(String),
  /// A collection of file paths and/or urls to configuration files to extend.
  Many(Vec<String>),
}

impl From<Extends> for Vec<String> {
  fn from(extends: Extends) -> Self {
    match extends {
      Extends::One(specifier) => vec![specifier],
      Extends::Many(specifiers) => specifiers,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema), schemars(inline))]
#[serde(rename_all = "camelCase")]
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
///
/// Read, a shebang line is trimmed at its end and an extension is lowercased
/// without its leading dot, so they compare the way file extensions do.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema), schemars(inline, extend("propertyNames" = { "pattern": "^#!" })))]
#[serde(try_from = "IndexMap<String, ShebangExtension>")]
pub struct Shebangs(pub IndexMap<String, ShebangExtension>);

impl TryFrom<IndexMap<String, ShebangExtension>> for Shebangs {
  type Error = String;

  fn try_from(shebangs: IndexMap<String, ShebangExtension>) -> Result<Self, Self::Error> {
    let mut normalized = IndexMap::with_capacity(shebangs.len());
    for (mut shebang, ShebangExtension(extension)) in shebangs {
      if !shebang.starts_with("#!") || shebang.contains(['\r', '\n']) {
        return Err(format!(
          "Expected the key '{}' in the 'shebangs' property to be a shebang line starting with '#!'.",
          shebang
        ));
      }
      let extension_without_dot = extension.strip_prefix('.').unwrap_or(&extension);
      if extension_without_dot.is_empty()
        || extension_without_dot.contains(|c: char| c.is_whitespace() || matches!(c, '.' | '/' | '\\' | '*' | '?' | '[' | ']' | '{' | '}'))
      {
        return Err(format!(
          "Expected a file extension (ex. \"sh\") for shebang '{}' in the 'shebangs' property, but found '{}'.",
          shebang, extension
        ));
      }
      shebang.truncate(shebang.trim_end().len());
      normalized.insert(shebang, ShebangExtension(extension_without_dot.to_lowercase()));
    }
    Ok(Shebangs(normalized))
  }
}

/// The file extension to treat matching files as (ex. "sh" or ".sh").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema), schemars(inline))]
pub struct ShebangExtension(pub String);

/// Plugin configuration.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema), schemars(rename = "pluginTable"))]
#[serde(expecting = "a plugin's configuration (an object), as a property that isn't one of dprint's")]
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
  #[cfg_attr(feature = "schema", schemars(with = "PluginProperties"))]
  pub plugin: ConfigKeyMap,
}

/// What a plugin's own properties are is the plugin's schema's to say.
#[cfg(feature = "schema")]
struct PluginProperties;

#[cfg(feature = "schema")]
impl JsonSchema for PluginProperties {
  fn schema_name() -> std::borrow::Cow<'static, str> {
    "pluginProperties".into()
  }

  fn inline_schema() -> bool {
    true
  }

  fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({ "type": "object", "additionalProperties": true })
  }
}

/// File patterns, one or more.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema), schemars(inline))]
#[serde(
  untagged,
  expecting = "The 'associations' property in a plugin configuration must be a string or an array of strings."
)]
pub enum Associations {
  One(String),
  Many(Vec<String>),
}

impl From<Associations> for Vec<String> {
  fn from(associations: Associations) -> Self {
    match associations {
      Associations::One(pattern) => vec![pattern],
      Associations::Many(patterns) => patterns,
    }
  }
}

/// Overrides of a plugin's configuration, one or more.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema), schemars(inline))]
#[serde(untagged)]
pub enum Overrides {
  One(PluginOverride),
  Many(Vec<PluginOverride>),
}

impl From<Overrides> for Vec<PluginOverride> {
  fn from(overrides: Overrides) -> Self {
    match overrides {
      Overrides::One(override_config) => vec![override_config],
      Overrides::Many(overrides) => overrides,
    }
  }
}

// read by hand rather than as an untagged enum so an error in an override
// (ex. a missing `files`) is reported as what it is, which an untagged enum
// can't say
impl<'de> Deserialize<'de> for Overrides {
  fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
    struct OverridesVisitor;

    impl<'de> Visitor<'de> for OverridesVisitor {
      type Value = Overrides;

      fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("an override (an object) or an array of them")
      }

      fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
        PluginOverride::deserialize(MapAccessDeserializer::new(map)).map(Overrides::One)
      }

      fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
        Vec::deserialize(SeqAccessDeserializer::new(seq)).map(Overrides::Many)
      }
    }

    deserializer.deserialize_any(OverridesVisitor)
  }
}

/// An override of a plugin's configuration for some of its files.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema), schemars(rename = "pluginOverride", extend("minProperties" = 2)))]
#[serde(expecting = "an override (an object with 'files' and the plugin's properties for them)")]
pub struct PluginOverride {
  /// File patterns this override applies to.
  pub files: OverrideFiles,
  /// The plugin's own configuration for the files, which its schema describes.
  #[serde(flatten)]
  #[cfg_attr(feature = "schema", schemars(with = "PluginProperties"))]
  pub plugin: OverrideProperties,
}

/// File patterns, one or more.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema), schemars(inline))]
#[serde(untagged)]
pub enum OverrideFiles {
  One(String),
  #[cfg_attr(feature = "schema", schemars(extend("minItems" = 1)))]
  Many(Vec<String>),
}

impl From<OverrideFiles> for Vec<String> {
  fn from(files: OverrideFiles) -> Self {
    match files {
      OverrideFiles::One(pattern) => vec![pattern],
      OverrideFiles::Many(patterns) => patterns,
    }
  }
}

// read by hand so an empty array is reported as what it is (see `minItems`)
impl<'de> Deserialize<'de> for OverrideFiles {
  fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
    struct FilesVisitor;

    impl<'de> Visitor<'de> for FilesVisitor {
      type Value = OverrideFiles;

      fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a file pattern or an array of them")
      }

      fn visit_str<E: serde::de::Error>(self, pattern: &str) -> Result<Self::Value, E> {
        Ok(OverrideFiles::One(pattern.to_string()))
      }

      fn visit_string<E: serde::de::Error>(self, pattern: String) -> Result<Self::Value, E> {
        Ok(OverrideFiles::One(pattern))
      }

      fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
        let patterns: Vec<String> = Vec::deserialize(SeqAccessDeserializer::new(seq))?;
        if patterns.is_empty() {
          return Err(A::Error::custom("A plugin configuration override must specify at least one file pattern."));
        }
        Ok(OverrideFiles::Many(patterns))
      }
    }

    deserializer.deserialize_any(FilesVisitor)
  }
}

/// A plugin's properties in an override of its configuration: at least one
/// (see `minProperties` of [`PluginOverride`], which counts `files` too).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "ConfigKeyMap")]
pub struct OverrideProperties(pub ConfigKeyMap);

impl TryFrom<ConfigKeyMap> for OverrideProperties {
  type Error = &'static str;

  fn try_from(properties: ConfigKeyMap) -> Result<Self, Self::Error> {
    if properties.is_empty() {
      return Err("A plugin configuration override must specify at least one configuration property.");
    }
    Ok(OverrideProperties(properties))
  }
}

/// The schema of a configuration file without the plugins' schemas: what
/// the website serves at [`ROOT_SCHEMA_ID`], and what `dprint schema`
/// composes the plugins' schemas into.
#[cfg(feature = "schema")]
pub fn root_schema() -> serde_json::Value {
  crate::schema_for::<ConfigFile>()
}

#[cfg(test)]
mod test {
  use pretty_assertions::assert_eq;
  use serde_json::json;

  use super::*;
  use crate::from_json;

  /// A configuration file with every property of dprint's, and a plugin's
  /// table with every property of dprint's.
  pub(super) fn full_config() -> serde_json::Value {
    json!({
      "$schema": ROOT_SCHEMA_ID,
      "incremental": false,
      "inherit": true,
      "extends": ["./base.json", "https://example.com/base.json"],
      "lineWidth": 100,
      "indentWidth": 4,
      "useTabs": true,
      "newLineKind": "lf",
      "includes": ["**/*.ts"],
      "excludes": ["**/node_modules"],
      "shebangs": { "#!/usr/bin/env bash": "sh" },
      "plugins": ["https://plugins.dprint.dev/typescript-0.96.1.wasm"],
      "typescript": {
        "locked": true,
        "associations": ["**/*.cts"],
        "semiColons": "asNeeded",
        "quoteStyle": { "prefer": "single" },
        "overrides": [{ "files": "*.test.ts", "semiColons": "always" }]
      },
      "json": { "overrides": { "files": ["*.lock.json"], "indentWidth": 2 } }
    })
  }

  #[test]
  fn reads_a_configuration_file() {
    let file: ConfigFile = from_json(full_config()).unwrap();
    assert_eq!(file.schema.as_deref(), Some(ROOT_SCHEMA_ID));
    assert_eq!(file.incremental, Some(false));
    assert_eq!(file.inherit, Some(true));
    assert_eq!(
      file.extends,
      Some(Extends::Many(vec!["./base.json".to_string(), "https://example.com/base.json".to_string()]))
    );
    assert_eq!(
      file.global,
      GlobalSettings {
        line_width: Some(100),
        indent_width: Some(4),
        use_tabs: Some(true),
        new_line_kind: Some(NewLineKind::Lf),
      }
    );
    assert_eq!(file.includes, Some(vec!["**/*.ts".to_string()]));
    assert_eq!(file.excludes, Some(vec!["**/node_modules".to_string()]));
    assert_eq!(
      file.shebangs,
      Some(Shebangs(IndexMap::from([(
        "#!/usr/bin/env bash".to_string(),
        ShebangExtension("sh".to_string())
      )])))
    );
    assert_eq!(file.plugins, Some(vec!["https://plugins.dprint.dev/typescript-0.96.1.wasm".to_string()]));
    assert_eq!(file.project_type, None);
    assert_eq!(file.plugin_tables.keys().collect::<Vec<_>>(), ["typescript", "json"]);
    let typescript = &file.plugin_tables["typescript"];
    assert_eq!(typescript.locked, Some(true));
    assert_eq!(typescript.associations, Some(Associations::Many(vec!["**/*.cts".to_string()])));
    // the plugin's own properties, in order, however deep
    assert_eq!(
      typescript.plugin,
      ConfigKeyMap::from([
        ("semiColons".to_string(), ConfigKeyValue::from_str("asNeeded")),
        (
          "quoteStyle".to_string(),
          ConfigKeyValue::Object(ConfigKeyMap::from([("prefer".to_string(), ConfigKeyValue::from_str("single"))]))
        ),
      ])
    );
    assert_eq!(
      typescript.overrides,
      Some(Overrides::Many(vec![PluginOverride {
        files: OverrideFiles::One("*.test.ts".to_string()),
        plugin: OverrideProperties(ConfigKeyMap::from([("semiColons".to_string(), ConfigKeyValue::from_str("always"))])),
      }]))
    );
    assert_eq!(
      file.plugin_tables["json"].overrides,
      Some(Overrides::One(PluginOverride {
        files: OverrideFiles::Many(vec!["*.lock.json".to_string()]),
        plugin: OverrideProperties(ConfigKeyMap::from([("indentWidth".to_string(), ConfigKeyValue::from_i32(2))])),
      }))
    );
  }

  #[test]
  fn writes_a_configuration_file_as_it_was_read() {
    let file: ConfigFile = from_json(full_config()).unwrap();
    assert_eq!(serde_json::to_value(&file).unwrap(), full_config());
    // an empty file has nothing
    assert_eq!(serde_json::to_value(ConfigFile::default()).unwrap(), json!({}));
  }

  #[test]
  fn normalizes_shebangs_as_file_extensions() {
    let file: ConfigFile = from_json(json!({ "shebangs": { "#!/bin/sh  ": ".SH", "#!/usr/bin/env node": "mjs" } })).unwrap();
    assert_eq!(
      file
        .shebangs
        .unwrap()
        .0
        .into_iter()
        .map(|(shebang, extension)| (shebang, extension.0))
        .collect::<Vec<_>>(),
      [
        ("#!/bin/sh".to_string(), "sh".to_string()),
        ("#!/usr/bin/env node".to_string(), "mjs".to_string())
      ]
    );
  }

  #[test]
  fn keeps_the_old_project_type_property_out_of_the_plugins_tables() {
    let file: ConfigFile = from_json(json!({ "projectType": "openSource", "test": {} })).unwrap();
    assert_eq!(file.project_type, Some(ConfigKeyValue::from_str("openSource")));
    assert_eq!(file.plugin_tables.keys().collect::<Vec<_>>(), ["test"]);
  }

  #[test]
  fn says_what_is_wrong_and_where() {
    let cases = [
      // dprint's properties
      (json!({ "$schema": 1 }), "$schema: invalid type: integer `1`, expected a string"),
      (json!({ "incremental": "yes" }), "incremental: invalid type: string \"yes\", expected a boolean"),
      (json!({ "inherit": 1 }), "inherit: invalid type: integer `1`, expected a boolean"),
      (
        json!({ "extends": 5 }),
        "extends: Extends in configuration must be a string or an array of strings.",
      ),
      (
        json!({ "extends": [5] }),
        "extends: Extends in configuration must be a string or an array of strings.",
      ),
      (json!({ "lineWidth": "80" }), "lineWidth: invalid type: string \"80\", expected u32"),
      (json!({ "lineWidth": -1 }), "lineWidth: invalid value: integer `-1`, expected u32"),
      (json!({ "indentWidth": 300 }), "indentWidth: invalid value: integer `300`, expected u8"),
      (json!({ "useTabs": "no" }), "useTabs: invalid type: string \"no\", expected a boolean"),
      (
        json!({ "newLineKind": "cr" }),
        "newLineKind: unknown variant `cr`, expected one of `auto`, `crlf`, `lf`, `system`",
      ),
      (
        json!({ "includes": "**/*.ts" }),
        "includes: invalid type: string \"**/*.ts\", expected a sequence",
      ),
      (json!({ "excludes": [1] }), "excludes[0]: invalid type: integer `1`, expected a string"),
      (json!({ "plugins": {} }), "plugins: invalid type: map, expected a sequence"),
      (
        json!({ "shebangs": { "/bin/sh": "sh" } }),
        "shebangs: Expected the key '/bin/sh' in the 'shebangs' property to be a shebang line starting with '#!'.",
      ),
      (
        json!({ "shebangs": { " #!/bin/sh": "sh" } }),
        "shebangs: Expected the key ' #!/bin/sh' in the 'shebangs' property to be a shebang line starting with '#!'.",
      ),
      (
        json!({ "shebangs": { "#!/bin/sh\ntext": "sh" } }),
        "shebangs: Expected the key '#!/bin/sh\ntext' in the 'shebangs' property to be a shebang line starting with '#!'.",
      ),
      (
        json!({ "shebangs": { "#!/bin/sh": "" } }),
        "shebangs: Expected a file extension (ex. \"sh\") for shebang '#!/bin/sh' in the 'shebangs' property, but found ''.",
      ),
      (
        json!({ "shebangs": { "#!/bin/sh": "tar.gz" } }),
        "shebangs: Expected a file extension (ex. \"sh\") for shebang '#!/bin/sh' in the 'shebangs' property, but found 'tar.gz'.",
      ),
      (
        json!({ "shebangs": { "#!/bin/sh": 5 } }),
        "shebangs.#!/bin/sh: invalid type: integer `5`, expected a string",
      ),
      (json!({ "shebangs": [] }), "shebangs: invalid type: sequence, expected a map"),
      // what isn't one of dprint's properties is a plugin's table
      (
        json!({ "lineWidht": 80 }),
        "lineWidht: invalid type: integer `80`, expected a plugin's configuration (an object), as a property that isn't one of dprint's",
      ),
      (
        json!({ "test": null }),
        "test: invalid type: null, expected a plugin's configuration (an object), as a property that isn't one of dprint's",
      ),
      (
        json!({ "test": ["a"] }),
        "test: invalid type: sequence, expected a plugin's configuration (an object), as a property that isn't one of dprint's",
      ),
      // dprint's properties of a plugin's table
      (json!({ "test": { "locked": 1 } }), "test.locked: invalid type: integer `1`, expected a boolean"),
      (
        json!({ "test": { "associations": 1 } }),
        "test.associations: The 'associations' property in a plugin configuration must be a string or an array of strings.",
      ),
      (
        json!({ "test": { "associations": [1] } }),
        "test.associations: The 'associations' property in a plugin configuration must be a string or an array of strings.",
      ),
      (
        json!({ "test": { "overrides": 5 } }),
        "test.overrides: invalid type: integer `5`, expected an override (an object) or an array of them",
      ),
      (
        json!({ "test": { "overrides": [5] } }),
        "test.overrides[0]: invalid type: integer `5`, expected an override (an object with 'files' and the plugin's properties for them)",
      ),
      (json!({ "test": { "overrides": [{ "a": 1 }] } }), "test.overrides[0]: missing field `files`"),
      (
        json!({ "test": { "overrides": [{ "files": 5, "a": 1 }] } }),
        "test.overrides[0].files: invalid type: integer `5`, expected a file pattern or an array of them",
      ),
      (
        json!({ "test": { "overrides": [{ "files": [1], "a": 1 }] } }),
        "test.overrides[0].files[0]: invalid type: integer `1`, expected a string",
      ),
      (
        json!({ "test": { "overrides": [{ "files": [], "a": 1 }] } }),
        "test.overrides[0].files: A plugin configuration override must specify at least one file pattern.",
      ),
      (
        json!({ "test": { "overrides": { "files": "*.x" } } }),
        "test.overrides: A plugin configuration override must specify at least one configuration property.",
      ),
      (
        json!({ "test": { "overrides": [{ "files": "*.x" }] } }),
        "test.overrides[0]: A plugin configuration override must specify at least one configuration property.",
      ),
    ];
    for (text, expected) in cases {
      let err = from_json::<ConfigFile>(text.clone()).unwrap_err();
      assert_eq!(err.to_string(), expected, "{}", text);
    }
  }

  #[test]
  fn a_plugins_own_properties_may_be_anything() {
    let file: ConfigFile = from_json(json!({
      "test": { "a": null, "b": [1, "two", { "three": 3 }], "c": -1, "locked": false }
    }))
    .unwrap();
    let table = &file.plugin_tables["test"];
    assert_eq!(table.locked, Some(false));
    assert_eq!(table.plugin.keys().collect::<Vec<_>>(), ["a", "b", "c"]);
    assert_eq!(table.plugin["a"], ConfigKeyValue::Null);
    assert_eq!(table.plugin["c"], ConfigKeyValue::from_i32(-1));
  }
}

#[cfg(all(test, feature = "schema"))]
mod schema_test {
  use pretty_assertions::assert_eq;
  use serde_json::Value;
  use serde_json::json;

  use super::*;
  use crate::from_json;

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
      "schema/v0.json isn't the generated schema. Run `UPDATE_CONFIG_SCHEMA=1 cargo test -p dprint-config-model --features schema` to regenerate it."
    );
  }

  fn validate(schema: &Value, instance: &Value) -> Result<(), String> {
    let mut schemas = boon::Schemas::new();
    let mut compiler = boon::Compiler::new();
    compiler.add_resource(ROOT_SCHEMA_ID, schema.clone()).unwrap();
    let index = compiler.compile(ROOT_SCHEMA_ID, &mut schemas).map_err(|err| format!("{:#}", err))?;
    schemas.validate(instance, index).map_err(|err| format!("{:#}", err))
  }

  #[test]
  fn the_schema_accepts_what_the_model_reads_and_rejects_what_it_doesnt() {
    let schema = root_schema();
    let full = super::test::full_config();
    assert_eq!(from_json::<ConfigFile>(full.clone()).is_ok(), true);
    assert_eq!(validate(&schema, &full), Ok(()));
    // a written file reads the same, so it validates the same
    let written = serde_json::to_value(from_json::<ConfigFile>(full.clone()).unwrap()).unwrap();
    assert_eq!(validate(&schema, &written), Ok(()));

    // what the model rejects, the schema does too (the shape; the schema
    // can't know a shebang's extension is normalized)
    for invalid in [
      json!({ "incremental": "yes" }),
      json!({ "extends": 5 }),
      json!({ "lineWidth": "80" }),
      json!({ "lineWidth": -1 }),
      json!({ "indentWidth": 300 }),
      json!({ "newLineKind": "cr" }),
      json!({ "includes": "**/*.ts" }),
      json!({ "shebangs": { "/bin/sh": "sh" } }),
      json!({ "shebangs": { "#!/bin/sh": 5 } }),
      json!({ "lineWidht": 80 }),
      json!({ "test": null }),
      json!({ "test": { "locked": 1 } }),
      json!({ "test": { "associations": [1] } }),
      json!({ "test": { "overrides": 5 } }),
      json!({ "test": { "overrides": [{ "a": 1 }] } }),
      json!({ "test": { "overrides": [{ "files": [], "a": 1 }] } }),
      json!({ "test": { "overrides": [{ "files": "*.x" }] } }),
    ] {
      assert!(from_json::<ConfigFile>(invalid.clone()).is_err(), "{}", invalid);
      assert!(validate(&schema, &invalid).is_err(), "{}", invalid);
    }
    // and the old `projectType` is accepted but not offered
    assert!(from_json::<ConfigFile>(json!({ "projectType": "openSource" })).is_ok());
    assert!(schema["properties"].get("projectType").is_none());
  }

  #[test]
  fn describes_dprints_properties_and_the_plugins_tables() {
    let schema = root_schema();
    assert_eq!(schema["$schema"], json!("https://json-schema.org/draft/2020-12/schema"));
    assert_eq!(schema["$id"], json!(ROOT_SCHEMA_ID));
    assert_eq!(schema["allowTrailingCommas"], json!(true));
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
    assert_eq!(table["type"], json!("object"));
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
}
