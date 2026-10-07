//! What the built-in exec plugin's configuration may hold, as types.
//!
//! These are what the configuration is read into (`Configuration::resolve`
//! in `configuration.rs` reads an [`ExecConfigInput`] and resolves it to the
//! runtime's `Configuration`) and what its schema is generated from, so the
//! names, types, casing and structure the plugin accepts and the schema
//! describes are one definition. The schema says what this version accepts
//! (ex. `playWithFire` and `setupTimeout`), which the schema published with
//! the exec plugin doesn't.
//!
//! The types hold what a configuration says and nothing more: a property it
//! doesn't specify is `None`, and what that resolves to (ex. a `timeout` of
//! [`default_timeout`] seconds) is the resolution's, which the schema states
//! as the property's `default` from the same function.

use std::sync::LazyLock;
use std::sync::OnceLock;

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

/// The exec plugin built into dprint, which formats files with external commands.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecConfigInput {
  /// The width of a line the formatter will try to stay under. Available to commands as {{line_width}}.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub line_width: Option<u32>,
  /// The number of spaces for an indent. Available to commands as {{indent_width}}.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub indent_width: Option<u8>,
  /// Whether to use tabs (true) or spaces (false). Available to commands as {{use_tabs}}.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub use_tabs: Option<bool>,
  /// Change this to invalidate the incremental cache, ex. when a command's version changes.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub cache_key: Option<String>,
  /// The working directory to run the commands in. Defaults to the current working directory.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub cwd: Option<String>,
  /// Seconds a command may take to format a file before it's killed and the format fails.
  #[serde(skip_serializing_if = "Option::is_none")]
  #[schemars(default = "schema_default_timeout")]
  pub timeout: Option<u32>,
  /// Seconds a setup command may run before it's killed. A setup command that times out isn't run again for the other files.
  #[serde(skip_serializing_if = "Option::is_none")]
  #[schemars(default = "schema_default_setup_timeout")]
  pub setup_timeout: Option<u32>,
  /// Allows the exec commands of remote configuration (ex. an `extends` url) to run. `true` allows any program, or list the programs their commands and setup commands may run. Only a local configuration file can allow it.
  ///
  /// dprint reads this when it combines the configuration files (see
  /// `remote_exec.rs`), before the configuration gets to the plugin.
  #[serde(skip_serializing_if = "Option::is_none")]
  #[schemars(default = "schema_default_play_with_fire")]
  pub play_with_fire: Option<PlayWithFire>,
  /// Commands to format with.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub commands: Option<Vec<ExecCommandInput>>,
}

/// Seconds a command may take to format a file when the configuration
/// doesn't say.
pub fn default_timeout() -> u32 {
  30
}

fn schema_default_timeout() -> Option<u32> {
  Some(default_timeout())
}

/// Seconds a setup command may run when the configuration doesn't say. Setup
/// commands often install a tool, which can take a while.
pub fn default_setup_timeout() -> u32 {
  300
}

fn schema_default_setup_timeout() -> Option<u32> {
  Some(default_setup_timeout())
}

fn schema_default_play_with_fire() -> Option<PlayWithFire> {
  Some(PlayWithFire::default())
}

/// Whether the exec commands of remote configuration may run: any program, or the listed ones.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged, expecting = "Expected true, false or an array of the programs remote commands may run.")]
#[schemars(inline)]
pub enum PlayWithFire {
  Any(bool),
  Programs(Vec<String>),
}

/// What remote commands may run when no local configuration file says:
/// nothing.
impl Default for PlayWithFire {
  fn default() -> Self {
    PlayWithFire::Any(false)
  }
}

/// A command to format files with.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields, expecting = "a command (an object)")]
#[schemars(
  inline,
  extend(
    "$comment" = "A command needs a non-empty exts, fileNames or associations. Each alternative allows other properties, as tombi's strict mode otherwise treats it as allowing none.",
    "anyOf" = [
      { "required": ["exts"], "properties": { "exts": { "minItems": 1 } }, "additionalProperties": true },
      { "required": ["fileNames"], "properties": { "fileNames": { "minItems": 1 } }, "additionalProperties": true },
      { "required": ["associations"], "properties": { "associations": { "minItems": 1 } }, "additionalProperties": true }
    ]
  )
)]
pub struct ExecCommandInput {
  /// The command to format with: the program and its arguments. The file text is passed to it on stdin and the formatted text is read from its stdout. Arguments may use {{file_path}}, {{line_width}}, {{use_tabs}}, {{indent_width}}, {{cwd}} and {{timeout}}.
  pub command: String,
  /// File extensions to format with this command.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub exts: Option<StringOrStrings>,
  /// File names to format with this command, ex. for files without an extension.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub file_names: Option<StringOrStrings>,
  /// A glob of the file paths to format with this command. Prefer "exts" when possible.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub associations: Option<Associations>,
  /// Whether to pass the file text to the command on stdin.
  #[serde(skip_serializing_if = "Option::is_none")]
  #[schemars(default = "schema_default_stdin")]
  pub stdin: Option<bool>,
  /// The working directory to run this command in.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub cwd: Option<String>,
  /// Files whose contents invalidate the incremental cache when they change, ex. the command's own configuration file.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub cache_key_files: Option<Vec<String>>,
  /// A command to run once before this command formats its first file, ex. to install it. It's killed after "setupTimeout" seconds.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub setup_command: Option<String>,
}

/// Whether a command gets the file text on stdin when the configuration
/// doesn't say.
pub fn default_stdin() -> bool {
  true
}

fn schema_default_stdin() -> Option<bool> {
  Some(default_stdin())
}

/// One or more.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged, expecting = "Expected a string or an array of strings.")]
#[schemars(inline)]
pub enum StringOrStrings {
  One(String),
  Many(Vec<String>),
}

impl From<StringOrStrings> for Vec<String> {
  fn from(values: StringOrStrings) -> Self {
    match values {
      StringOrStrings::One(value) => vec![value],
      StringOrStrings::Many(values) => values,
    }
  }
}

/// A glob, or one in an array (more than one isn't implemented, which
/// resolving the configuration says).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged, expecting = "Expected a glob or an array with one glob.")]
#[schemars(inline)]
pub enum Associations {
  One(String),
  #[schemars(extend("maxItems" = 1))]
  Many(Vec<String>),
}

/// The names of an exec configuration's properties as a configuration spells
/// them, looked up from [`ExecConfigInput`] (see
/// [`dprint_config_model::property_name`]): for what works on the
/// configuration's values before they're read as the type (dprint's policy
/// for remote configuration) and for diagnostics that name a property, so
/// that the type is the one place a name is spelled.
pub struct ExecPropertyNames {
  pub line_width: String,
  pub indent_width: String,
  pub use_tabs: String,
  pub cache_key: String,
  pub cwd: String,
  pub timeout: String,
  pub setup_timeout: String,
  pub play_with_fire: String,
  pub commands: String,
}

impl ExecPropertyNames {
  /// Whether it's the name of one of the exec configuration's properties.
  pub fn contains(&self, name: &str) -> bool {
    [
      &self.line_width,
      &self.indent_width,
      &self.use_tabs,
      &self.cache_key,
      &self.cwd,
      &self.timeout,
      &self.setup_timeout,
      &self.play_with_fire,
      &self.commands,
    ]
    .into_iter()
    .any(|property| property == name)
  }
}

impl ExecConfigInput {
  /// The names of the properties as a configuration spells them.
  pub fn property_names() -> &'static ExecPropertyNames {
    static NAMES: LazyLock<ExecPropertyNames> = LazyLock::new(|| {
      let name = |set: fn(&mut ExecConfigInput)| dprint_config_model::property_name(set);
      ExecPropertyNames {
        line_width: name(|input| input.line_width = Some(1)),
        indent_width: name(|input| input.indent_width = Some(1)),
        use_tabs: name(|input| input.use_tabs = Some(true)),
        cache_key: name(|input| input.cache_key = Some(String::new())),
        cwd: name(|input| input.cwd = Some(String::new())),
        timeout: name(|input| input.timeout = Some(1)),
        setup_timeout: name(|input| input.setup_timeout = Some(1)),
        play_with_fire: name(|input| input.play_with_fire = Some(PlayWithFire::Any(true))),
        commands: name(|input| input.commands = Some(Vec::new())),
      }
    });
    &NAMES
  }
}

/// The names of the command properties that are referred to by name (in
/// diagnostics and dprint's policy for remote configuration), as a
/// configuration spells them, looked up from [`ExecCommandInput`] (see
/// [`ExecPropertyNames`]).
pub struct ExecCommandPropertyNames {
  pub command: String,
  pub exts: String,
  pub file_names: String,
  pub associations: String,
  pub cwd: String,
  pub cache_key_files: String,
}

impl ExecCommandInput {
  /// The names of the properties as a configuration spells them.
  pub fn property_names() -> &'static ExecCommandPropertyNames {
    static NAMES: LazyLock<ExecCommandPropertyNames> = LazyLock::new(|| {
      let name = |set: fn(&mut ExecCommandInput)| dprint_config_model::property_name(set);
      ExecCommandPropertyNames {
        command: name(|command| command.command = "fmt".to_string()),
        exts: name(|command| command.exts = Some(StringOrStrings::Many(Vec::new()))),
        file_names: name(|command| command.file_names = Some(StringOrStrings::Many(Vec::new()))),
        associations: name(|command| command.associations = Some(Associations::Many(Vec::new()))),
        cwd: name(|command| command.cwd = Some(String::new())),
        cache_key_files: name(|command| command.cache_key_files = Some(Vec::new())),
      }
    });
    &NAMES
  }
}

/// The schema of the built-in exec's configuration, generated from
/// [`ExecConfigInput`].
pub fn exec_config_schema() -> &'static str {
  static SCHEMA: OnceLock<String> = OnceLock::new();
  SCHEMA.get_or_init(dprint_config_model::schema_json_for::<ExecConfigInput>)
}

#[cfg(test)]
mod test {
  use std::collections::BTreeSet;

  use dprint_core::configuration::ConfigKeyMap;
  use dprint_core::configuration::GlobalConfiguration;
  use pretty_assertions::assert_eq;
  use serde_json::json;

  use super::super::configuration::Configuration;
  use super::*;

  #[test]
  fn an_empty_configuration_says_nothing() {
    let input: ExecConfigInput = dprint_config_model::from_json(json!({})).unwrap();
    assert_eq!(input, ExecConfigInput::default());
    assert_eq!(input.timeout, None);
    let command: ExecCommandInput = dprint_config_model::from_json(json!({ "command": "fmt" })).unwrap();
    assert_eq!(command.stdin, None);
  }

  #[test]
  fn the_schemas_defaults_are_what_an_empty_configuration_resolves_to() {
    let schema: serde_json::Value = serde_json::from_str(exec_config_schema()).unwrap();
    let resolved = Configuration::resolve(
      dprint_config_model::to_values(&ExecConfigInput {
        commands: Some(vec![ExecCommandInput {
          command: "fmt".to_string(),
          exts: Some(StringOrStrings::One("txt".to_string())),
          ..Default::default()
        }]),
        ..Default::default()
      })
      .unwrap(),
      &GlobalConfiguration::default(),
    );
    assert_eq!(resolved.diagnostics, Vec::new());
    assert_eq!(schema["properties"]["timeout"]["default"], json!(resolved.config.timeout));
    assert_eq!(schema["properties"]["setupTimeout"]["default"], json!(resolved.config.setup_timeout));
    assert_eq!(
      schema["properties"]["commands"]["items"]["properties"]["stdin"]["default"],
      json!(resolved.config.commands[0].stdin)
    );
    // what remote commands may run when no local configuration file says
    assert_eq!(
      schema["properties"]["playWithFire"]["default"],
      serde_json::to_value(PlayWithFire::default()).unwrap()
    );
    // the types say nothing a configuration didn't
    assert_eq!(dprint_config_model::to_values(&ExecConfigInput::default()).unwrap(), ConfigKeyMap::new());
  }

  #[test]
  fn the_property_names_are_the_types() {
    let schema: serde_json::Value = serde_json::from_str(exec_config_schema()).unwrap();
    let keys = |properties: &serde_json::Value| properties.as_object().unwrap().keys().cloned().collect::<BTreeSet<_>>();
    let names = ExecConfigInput::property_names();
    assert_eq!(
      keys(&schema["properties"]),
      BTreeSet::from([
        names.line_width.clone(),
        names.indent_width.clone(),
        names.use_tabs.clone(),
        names.cache_key.clone(),
        names.cwd.clone(),
        names.timeout.clone(),
        names.setup_timeout.clone(),
        names.play_with_fire.clone(),
        names.commands.clone(),
      ])
    );
    assert_eq!(names.play_with_fire, "playWithFire");
    assert!(names.contains("setupTimeout"));
    assert!(!names.contains("shell"));
    let names = ExecCommandInput::property_names();
    let command_properties = keys(&schema["properties"]["commands"]["items"]["properties"]);
    let named = [
      names.command.clone(),
      names.exts.clone(),
      names.file_names.clone(),
      names.associations.clone(),
      names.cwd.clone(),
      names.cache_key_files.clone(),
    ];
    assert_eq!(named.iter().cloned().collect::<BTreeSet<_>>().len(), named.len(), "distinct");
    for name in &named {
      assert!(command_properties.contains(name), "{} in {:?}", name, command_properties);
    }
    assert_eq!(names.cache_key_files, "cacheKeyFiles");
  }
}
