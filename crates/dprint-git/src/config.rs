#[derive(Debug, Default, PartialEq, Eq)]
pub struct RepoConfig {
  pub ignore_case: bool,
  pub bare: bool,
  pub has_work_tree_setting: bool,
  pub has_includes: bool,
  pub has_worktree_config: bool,
  pub object_format: Option<String>,
}

/// Reads a repository's `config` file. Includes aren't followed.
pub fn parse_repo_config(text: &str) -> RepoConfig {
  let mut config = RepoConfig::default();
  let mut section = String::new();
  for line in text.lines() {
    let line = line.trim();
    let line = match line.find(['#', ';']) {
      Some(index) if !line[..index].contains('"') => line[..index].trim(),
      _ => line,
    };
    if let Some(header) = line.strip_prefix('[') {
      let header = header.trim_end_matches(']');
      let name = header
        .split(|c: char| c.is_whitespace() || c == '"')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
      if name == "include" || name == "includeif" {
        config.has_includes = true;
      }
      section = name;
      continue;
    }
    if line.is_empty() {
      continue;
    }
    let (key, value) = match line.split_once('=') {
      Some((key, value)) => (key.trim().to_ascii_lowercase(), Some(value.trim().trim_matches('"'))),
      None => (line.to_ascii_lowercase(), None),
    };
    let is_true = value.is_none_or(|value| matches!(value.to_ascii_lowercase().as_str(), "true" | "yes" | "on" | "1"));
    match (section.as_str(), key.as_str()) {
      ("core", "ignorecase") => config.ignore_case = is_true,
      ("core", "bare") => config.bare = is_true,
      ("core", "worktree") => config.has_work_tree_setting = true,
      ("extensions", "worktreeconfig") => config.has_worktree_config = is_true,
      ("extensions", "objectformat") => config.object_format = value.map(|value| value.to_ascii_lowercase()),
      _ => {}
    }
  }
  config
}

#[cfg(test)]
mod test {
  use super::*;

  #[test]
  fn reads_the_relevant_settings() {
    let config = parse_repo_config(
      r#"
[core]
	repositoryformatversion = 1
	bare = false
	ignoreCase ; a comment
	worktree = "../elsewhere"
[extensions]
	objectFormat = SHA256
	worktreeConfig = true
[includeIf "gitdir:~/work/"]
	path = work.inc
"#,
    );
    assert_eq!(
      config,
      RepoConfig {
        ignore_case: true,
        bare: false,
        has_work_tree_setting: true,
        has_includes: true,
        has_worktree_config: true,
        object_format: Some("sha256".to_string()),
      }
    );
    assert_eq!(parse_repo_config("[core]\n\tignorecase = false\n"), RepoConfig::default());
  }
}
