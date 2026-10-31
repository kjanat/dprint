//! Requests shared by CLI, editor and language-server frontends.
use crate::environment::EnvironmentVariables;
use crate::selection::ConfigDiscovery;
use crate::selection::FilePatternArgs;
/// The value of `--config`, which is either something to read the
/// configuration file from or the configuration file text itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigArg {
  /// A file path or url to the configuration file.
  PathOrUrl(String),
  /// The configuration file text, provided inline or read from stdin.
  Text(ConfigArgText),
}

impl ConfigArg {
  /// The file path or url when the configuration wasn't provided as text.
  pub fn maybe_path_or_url(&self) -> Option<&str> {
    match self {
      ConfigArg::PathOrUrl(value) => Some(value),
      ConfigArg::Text(_) => None,
    }
  }
}

/// Configuration file text that didn't come from a file dprint opened itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigArgText {
  pub text: String,
  /// Where the text came from, for display in error messages
  /// (ex. `<stdin>` or `/dev/fd/63`).
  pub origin: String,
}

/// Configuration and selection policy, independent of command-line parsing.
pub trait ConfigOptions {
  fn config(&self) -> Option<&ConfigArg>;
  fn plugins(&self) -> &[String];
  fn discovery_override(&self) -> Option<ConfigDiscovery>;
  fn stdin_file_path(&self) -> Option<&str> {
    None
  }
  fn requires_config_file(&self) -> bool {
    false
  }
  fn confirm_global_format(&self) -> bool {
    false
  }
  fn allow_no_files(&self) -> bool {
    false
  }
  fn allow_skipping_paths(&self) -> bool {
    false
  }
  fn file_patterns(&self) -> Option<&FilePatternArgs> {
    None
  }
  fn config_discovery(&self, environment: &dyn EnvironmentVariables) -> ConfigDiscovery {
    self.discovery_override().unwrap_or_else(|| {
      environment
        .env_var("DPRINT_CONFIG_DISCOVERY")
        .as_ref()
        .and_then(|value| value.to_str())
        .and_then(|value| value.parse().ok())
        .unwrap_or(ConfigDiscovery::Default)
    })
  }
}

/// Owned options for embedding a formatter without a CLI.
#[derive(Debug, Clone, Default)]
pub struct SessionOptions {
  pub config: Option<ConfigArg>,
  pub plugins: Vec<String>,
  pub discovery: Option<ConfigDiscovery>,
  pub file_path: Option<String>,
  pub patterns: FilePatternArgs,
  pub requires_config_file: bool,
  pub allow_no_files: bool,
  pub allow_skipping_paths: bool,
}
impl ConfigOptions for SessionOptions {
  fn config(&self) -> Option<&ConfigArg> {
    self.config.as_ref()
  }
  fn plugins(&self) -> &[String] {
    &self.plugins
  }
  fn discovery_override(&self) -> Option<ConfigDiscovery> {
    self.discovery
  }
  fn stdin_file_path(&self) -> Option<&str> {
    self.file_path.as_deref()
  }
  fn requires_config_file(&self) -> bool {
    self.requires_config_file
  }
  fn allow_no_files(&self) -> bool {
    self.allow_no_files
  }
  fn allow_skipping_paths(&self) -> bool {
    self.allow_skipping_paths
  }
  fn file_patterns(&self) -> Option<&FilePatternArgs> {
    Some(&self.patterns)
  }
}
