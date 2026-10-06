//! What the built-in exec plugin's configuration may hold, as types.
//!
//! These are what the configuration is read into (`Configuration::resolve`
//! in `configuration.rs` reads an [`ExecConfigInput`] and resolves it to the
//! runtime's `Configuration`) and what its schema is generated from, so the
//! names, types, casing and structure the plugin accepts and the schema
//! describes are one definition. The schema says what this version accepts
//! (ex. `playWithFire` and `setupTimeout`), which the schema published with
//! the exec plugin doesn't.

use std::sync::OnceLock;

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

/// The exec plugin built into dprint, which formats files with external commands.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
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
  #[serde(default = "default_timeout")]
  pub timeout: u32,
  /// Seconds a setup command may run before it's killed. A setup command that times out isn't run again for the other files.
  #[serde(default = "default_setup_timeout")]
  pub setup_timeout: u32,
  /// Allows the exec commands of remote configuration (ex. an `extends` url) to run. `true` allows any program, or list the programs their commands and setup commands may run. Only a local configuration file can allow it.
  ///
  /// dprint reads this when it combines the configuration files (see
  /// `remote_exec.rs`), before the configuration gets to the plugin.
  #[serde(default)]
  pub play_with_fire: PlayWithFire,
  /// Commands to format with.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub commands: Option<Vec<ExecCommandInput>>,
}

impl Default for ExecConfigInput {
  /// What an empty configuration reads as.
  fn default() -> Self {
    ExecConfigInput {
      line_width: None,
      indent_width: None,
      use_tabs: None,
      cache_key: None,
      cwd: None,
      timeout: default_timeout(),
      setup_timeout: default_setup_timeout(),
      play_with_fire: PlayWithFire::default(),
      commands: None,
    }
  }
}

fn default_timeout() -> u32 {
  30
}

/// Setup commands often install a tool, which can take a while.
fn default_setup_timeout() -> u32 {
  300
}

/// Whether the exec commands of remote configuration may run: any program, or the listed ones.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged, expecting = "Expected true, false or an array of the programs remote commands may run.")]
#[schemars(inline)]
pub enum PlayWithFire {
  Any(bool),
  Programs(Vec<String>),
}

impl Default for PlayWithFire {
  fn default() -> Self {
    PlayWithFire::Any(false)
  }
}

/// A command to format files with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
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
  #[serde(default = "default_stdin")]
  pub stdin: bool,
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

fn default_stdin() -> bool {
  true
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

/// The schema of the built-in exec's configuration, generated from
/// [`ExecConfigInput`].
pub fn exec_config_schema() -> &'static str {
  static SCHEMA: OnceLock<String> = OnceLock::new();
  SCHEMA.get_or_init(dprint_config_model::schema_json_for::<ExecConfigInput>)
}

#[cfg(test)]
mod test {
  use pretty_assertions::assert_eq;
  use serde_json::json;

  use super::*;

  #[test]
  fn an_empty_configuration_reads_as_the_default() {
    let input: ExecConfigInput = dprint_config_model::from_json(json!({})).unwrap();
    assert_eq!(input, ExecConfigInput::default());
    assert_eq!(input.timeout, 30);
    assert_eq!(input.setup_timeout, 300);
    assert_eq!(input.play_with_fire, PlayWithFire::Any(false));
    let command: ExecCommandInput = dprint_config_model::from_json(json!({ "command": "fmt" })).unwrap();
    assert!(command.stdin);
  }

  #[test]
  fn the_schemas_defaults_are_what_an_empty_configuration_reads_as() {
    let schema: serde_json::Value = serde_json::from_str(exec_config_schema()).unwrap();
    let input = ExecConfigInput::default();
    assert_eq!(schema["properties"]["timeout"]["default"], json!(input.timeout));
    assert_eq!(schema["properties"]["setupTimeout"]["default"], json!(input.setup_timeout));
    assert_eq!(
      schema["properties"]["playWithFire"]["default"],
      serde_json::to_value(&input.play_with_fire).unwrap()
    );
    assert_eq!(schema["properties"]["commands"]["items"]["properties"]["stdin"]["default"], json!(true));
  }
}
