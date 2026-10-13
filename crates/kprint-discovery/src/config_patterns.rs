use anyhow::Result;

use crate::GlobMatcher;
use crate::GlobMatcherOptions;
use crate::GlobPattern;
use crate::GlobPatterns;
use crate::environment::CanonicalizedPathBuf;
use crate::is_negated_glob;

pub fn process_config_patterns(file_patterns: &[String]) -> impl Iterator<Item = String> + '_ {
  file_patterns.iter().map(|p| process_config_pattern(p))
}

pub fn process_config_pattern(file_pattern: &str) -> String {
  let file_pattern = file_pattern.replace('\\', "/");
  // make config patterns that start with `/` be relative
  if file_pattern.starts_with('/') {
    format!(".{}", file_pattern)
  } else if file_pattern.starts_with("!/") {
    format!("!.{}", &file_pattern[1..])
  } else {
    file_pattern
  }
}

pub fn get_patterns_as_glob_matcher(patterns: &[String], config_base_path: &CanonicalizedPathBuf) -> Result<GlobMatcher> {
  let patterns = process_config_patterns(patterns);
  let (includes, excludes) = patterns.into_iter().partition(|p| !is_negated_glob(p));
  GlobMatcher::new(
    GlobPatterns {
      shebangs: Vec::new(),
      arg_includes: None,
      config_includes: Some(GlobPattern::new_vec(includes, config_base_path.clone())),
      arg_excludes: None,
      config_excludes: excludes
        .into_iter()
        .map(|relative_pattern| GlobPattern::new(relative_pattern, config_base_path.clone()).invert())
        .collect(),
    },
    &GlobMatcherOptions {
      case_sensitive: true,
      base_dir: config_base_path.clone(),
    },
  )
}
