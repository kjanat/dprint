//! What the built-in exec plugin's configuration may hold, as types, which
//! its schema is generated from.
//!
//! These describe the configuration's syntax, what `Configuration::resolve`
//! (see `configuration.rs`) reads by name; what it resolves it to is the
//! runtime's. The schema says what this version accepts (ex.
//! `playWithFire` and `setupTimeout`), which the schema published with the
//! exec plugin doesn't.

use std::sync::OnceLock;

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

/// The exec plugin built into dprint, which formats files with external commands.
#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
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
  #[schemars(extend("default" = 30))]
  pub timeout: Option<u32>,
  /// Seconds a setup command may run before it's killed. A setup command that times out isn't run again for the other files.
  #[serde(skip_serializing_if = "Option::is_none")]
  #[schemars(extend("default" = 300))]
  pub setup_timeout: Option<u32>,
  /// Allows the exec commands of remote configuration (ex. an `extends` url) to run. `true` allows any program, or list the programs their commands and setup commands may run. Only a local configuration file can allow it.
  #[serde(skip_serializing_if = "Option::is_none")]
  #[schemars(extend("default" = false))]
  pub play_with_fire: Option<PlayWithFire>,
  /// Commands to format with.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub commands: Option<Vec<ExecCommandInput>>,
}

/// Whether the exec commands of remote configuration may run: any program, or the listed ones.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
#[schemars(inline)]
pub enum PlayWithFire {
  Any(bool),
  Programs(Vec<String>),
}

/// A command to format files with.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
  #[schemars(extend("default" = true))]
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

/// One or more.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
#[schemars(inline)]
pub enum StringOrStrings {
  One(String),
  Many(Vec<String>),
}

/// A glob, or one in an array (more than one isn't implemented).
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
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
  SCHEMA.get_or_init(dprint_config_schema::schema_json_for::<ExecConfigInput>)
}
