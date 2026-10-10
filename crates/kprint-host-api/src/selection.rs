use anyhow::Result;
#[derive(Debug, Clone, Copy)]
pub enum ConfigDiscovery {
  Default,
  Global,
  IgnoreDescendants,
  Disabled,
}

impl std::str::FromStr for ConfigDiscovery {
  type Err = String;

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    match s.to_ascii_lowercase().as_str() {
      "default" | "true" | "1" => Ok(ConfigDiscovery::Default),
      "false" | "0" => Ok(ConfigDiscovery::Disabled),
      "global" => Ok(ConfigDiscovery::Global),
      "ignore-descendants" => Ok(ConfigDiscovery::IgnoreDescendants),
      _ => Err(format!("expected 'default', 'ignore-descendants' or 'false', got '{s}'")),
    }
  }
}

impl ConfigDiscovery {
  pub fn is_global(&self) -> bool {
    matches!(self, ConfigDiscovery::Global)
  }

  pub fn traverse_ancestors(&self) -> bool {
    match self {
      ConfigDiscovery::Default => true,
      ConfigDiscovery::IgnoreDescendants => true,
      ConfigDiscovery::Global => false,
      ConfigDiscovery::Disabled => false,
    }
  }

  pub fn traverse_descendants(&self) -> bool {
    match self {
      ConfigDiscovery::Default => true,
      ConfigDiscovery::IgnoreDescendants => false,
      ConfigDiscovery::Global => false,
      ConfigDiscovery::Disabled => false,
    }
  }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FilePatternArgs {
  /// File patterns specified on the command line or via `--stdin-files`.
  ///
  /// `None` means none were specified, which is different from specifying
  /// an empty list (ex. `--stdin-files` with no lines) as that means there
  /// are no files to format.
  pub include_patterns: Option<Vec<String>>,
  pub include_pattern_overrides: Option<Vec<String>>,
  pub exclude_patterns: Vec<String>,
  pub exclude_pattern_overrides: Option<Vec<String>>,
  pub allow_node_modules: bool,
  pub no_gitignore: bool,
  pub only_staged: bool,
  pub only_dirty: bool,
}

impl FilePatternArgs {
  /// Whether the arguments limit the run to some of the files the
  /// configuration would otherwise cover.
  pub fn is_partial_run(&self) -> bool {
    self.include_patterns.is_some()
      || self.include_pattern_overrides.is_some()
      || !self.exclude_patterns.is_empty()
      || self.exclude_pattern_overrides.is_some()
      || self.only_staged
      || self.only_dirty
  }
}
