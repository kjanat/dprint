use serde::Deserialize;
use serde::Serialize;

/// Information about a plugin.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PluginInfo {
  /// The name of the plugin.
  pub name: String,
  /// The version of the plugin.
  pub version: String,
  /// Gets the key that can be used in the configuration JSON.
  pub config_key: String,
  /// A url the user can go to in order to get help information about the plugin.
  pub help_url: String,
  /// Schema url for the plugin configuration.
  ///
  /// Generally in the format: `https://plugins.dprint.dev/<org-or-user>/<repo>/<tag-name>/schema.json`
  /// For example: <https://plugins.dprint.dev/dprint/dprint-plugin-typescript/0.60.0/schema.json>
  pub config_schema_url: String,
  /// Plugin update url.
  ///
  /// Generally in the format: `https://plugins.dprint.dev/<org-or-user>/<repo>/latest.json`
  /// For example: <https://plugins.dprint.dev/dprint/dprint-plugin-typescript/latest.json>
  pub update_url: Option<String>,
  /// Set when the plugin reads a configuration file of its own (ex.
  /// `tombi.toml`), which dprint parses and passes to it: the paths
  /// relative to the configuration file's directory to use when its
  /// `configFile` property doesn't name one, the first that exists. Each
  /// must stay within that directory (no absolute path or `..`, and no
  /// symlink out of it), or the plugin's configuration is an error.
  ///
  /// A Wasm plugin declares them with a `get_config_files` export
  /// returning a JSON array, so a plugin built for dprint's own plugin
  /// crates can read one too.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub config_files: Option<Vec<String>>,
}

/// Message for when a plugin's [`FileMatchingInfo`] can't be deserialized. A
/// newer plugin may describe its file matching in a way this version of the CLI
/// doesn't understand, so point at upgrading rather than leaving the user with
/// only a deserialization error.
pub const FILE_MATCHING_INFO_ERROR_MESSAGE: &str =
  "Failed getting the plugin's file matching info. This plugin may require a newer version of the kprint CLI. Try running: kprint upgrade";

/// The plugin file matching information based on the configuration.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FileMatchingInfo {
  /// The file extensions this plugin should format.
  #[serde(default = "Vec::new")]
  pub file_extensions: Vec<String>,
  /// The file names this plugin should format.
  #[serde(default = "Vec::new")]
  pub file_names: Vec<String>,
  /// Whether the plugin formats a file it matches in addition to the plugin
  /// that claims the file, rather than claiming the file itself.
  ///
  /// This is for plugins that do something to a file other than format it
  /// from scratch (ex. sorting the keys of a `package.json`), which would
  /// otherwise stop the file's usual formatter from running.
  #[serde(default)]
  pub additive: bool,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub additive_associations: Vec<String>,
}
