use anyhow::Result;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use crate::ConfigDiscovery;
use crate::POSSIBLE_CONFIG_FILE_NAMES;
use crate::environment::CanonicalizedPathBuf;
use crate::environment::DiscoveryEnvironment as Environment;
use crate::environment::PathKind;
use crate::utils::gitignore::GitIgnoreTree;
use crate::utils::gitignore::GitIgnoreTreeOptions;
use crate::utils::gitignore::resolve_global_gitignore_lines;

use super::ExcludeMatchDetail;
use super::GlobMatcher;
use super::GlobMatcherOptions;
use super::GlobPattern;
use super::GlobPatterns;
use super::escape_glob_text;
use super::is_pattern;
use super::non_negated_glob;
use super::scan::DirScanGitIgnore;
use super::scan::DirScanOptions;
use super::scan::scan_dir;
use super::unescape_glob_text;

#[derive(Debug, Default, Clone)]
pub struct GlobOutput {
  /// Files matching the patterns. Extensionless files that would match with a
  /// shebang line are included without reading them. Plugin resolution reads
  /// the shebang later.
  pub file_paths: Vec<PathBuf>,
  pub config_files: Vec<PathBuf>,
  /// CLI paths and patterns that are outside the pattern base directory.
  /// The caller resolves the config file to use for these separately.
  pub outside_base_paths: Vec<OutsideBasePath>,
}

/// A CLI path or glob pattern that is outside the pattern base directory.
#[derive(Debug, Clone)]
pub struct OutsideBasePath {
  /// The directory to start searching for the governing config file from.
  pub config_search_dir: PathBuf,
  /// The absolute path or pattern to resolve in the new scope.
  pub include_pattern: String,
}

pub struct GlobOptions {
  /// The directory to start searching from.
  pub start_dir: PathBuf,
  /// Whether to enable configuration discovery.
  pub config_discovery: ConfigDiscovery,
  /// The config file already in use, so it isn't rediscovered as a
  /// descendant config file (ex. the global config file when its directory
  /// is within the directory being formatted).
  pub current_config_path: Option<PathBuf>,
  /// The file patterns to use for globbing.
  pub file_patterns: GlobPatterns,
  /// The directory to use as the base for the patterns.
  /// Generally you want this to be the directory of the config file.
  pub pattern_base: CanonicalizedPathBuf,
  /// Whether to disable respecting .gitignore files.
  pub no_gitignore: bool,
}

pub fn glob(environment: &impl Environment, mut opts: GlobOptions) -> Result<GlobOutput> {
  if opts
    .file_patterns
    .arg_includes
    .as_ref()
    .map(|p| p.iter().all(|p| p.is_negated()))
    .unwrap_or(false)
  {
    // performance improvement (see issue #379)
    log_debug!(environment, "Skipping negated globs: {:?}", opts.file_patterns.arg_includes);
    return Ok(Default::default());
  }

  let start_instant = std::time::Instant::now();
  log_debug!(environment, "Globbing: {:?}", opts.file_patterns);

  // resolve literal (non-glob) CLI paths directly against the file system so
  // they can be matched without directory traversal (this also allows matching
  // paths outside the directory the traversal starts in, like ../file.txt)
  let mut literal_arg_paths = extract_literal_arg_paths(environment, &mut opts.file_patterns, &opts.pattern_base);
  rewrite_literal_exclude_paths(environment, &mut opts.file_patterns, &opts.pattern_base);
  extract_outside_arg_patterns(&mut opts.file_patterns, &opts.pattern_base, &mut literal_arg_paths.outside_base_paths);
  let mut run_traversal = requires_traversal(&opts.file_patterns);
  if run_traversal {
    opts.start_dir = expand_start_dir_for_arg_patterns(opts.start_dir, &opts.file_patterns, &opts.pattern_base);
  } else {
    log_debug!(environment, "Skipping traversal because the CLI args were all file paths.");
  }

  let git_ignore_options = (!opts.no_gitignore).then(|| GitIgnoreTreeOptions {
    include_paths: opts.file_patterns.include_paths(),
    global_gitignore_lines: resolve_global_gitignore_lines(environment),
  });
  let mut git_ignore_tree = git_ignore_options.clone().map(|options| GitIgnoreTree::new(environment.clone(), options));
  let glob_matcher = Arc::new(GlobMatcher::new(
    opts.file_patterns,
    &GlobMatcherOptions {
      // make it work the same way on every operating system
      case_sensitive: true,
      base_dir: opts.pattern_base.clone(),
    },
  )?);

  let mut output = GlobOutput {
    outside_base_paths: literal_arg_paths.outside_base_paths,
    ..Default::default()
  };

  let discover_configs = opts.config_discovery.traverse_descendants();
  let mut config_file_finder = DirConfigFileFinder::new(environment, opts.current_config_path.clone());

  // check the directories between the pattern base and the start directory the
  // same way a traversal descending from the pattern base would so matching
  // works the same regardless of the directory dprint is run from
  //
  // Note this chain includes the start directory itself, so a config file
  // sitting in it hands the directory to the sub scope created for that config
  // file. That's deliberate: it's what happens when running from the pattern
  // base instead, where the traversal descends into the directory and finds it.
  // The traversal below intentionally does the opposite for the start directory
  // (see `DiscoveryPolicy::child_context`), which only matters when this check
  // doesn't run because the start directory is the pattern base.
  if run_traversal && opts.start_dir != opts.pattern_base.as_ref() && opts.start_dir.starts_with(opts.pattern_base.as_ref()) {
    match check_dir_chain(
      &glob_matcher,
      &mut git_ignore_tree,
      discover_configs.then_some(&mut config_file_finder),
      opts.pattern_base.as_ref(),
      &opts.start_dir,
    ) {
      DirChainResult::Matched => {}
      DirChainResult::Excluded => {
        log_debug!(environment, "Skipping traversal because the start directory is excluded.");
        run_traversal = false;
      }
      DirChainResult::HasConfigFile(config_file) => {
        // the sub scope created for the config file handles this directory
        log_debug!(environment, "Skipping traversal because the start directory has its own config file.");
        push_dedup_config_file(&mut output.config_files, config_file);
        run_traversal = false;
      }
    }
  }

  // match the literal file paths (a gitignored file is still matched when
  // explicitly specified, but not when one of its ancestor directories is
  // gitignored because a traversal wouldn't descend into the directory)
  for file_path in literal_arg_paths.file_paths {
    if run_traversal && file_path.starts_with(&opts.start_dir) {
      continue; // the traversal will find this file
    }
    // examine the file's ancestor directories top down the same way a
    // traversal would: an excluded directory excludes everything within it
    // and a directory with its own config file is handled by the sub scope
    // created for that config file instead of the current scope
    if let Some(parent) = file_path.parent()
      && parent.starts_with(opts.pattern_base.as_ref())
    {
      match check_dir_chain(
        &glob_matcher,
        &mut git_ignore_tree,
        discover_configs.then_some(&mut config_file_finder),
        opts.pattern_base.as_ref(),
        parent,
      ) {
        DirChainResult::Matched => {}
        DirChainResult::Excluded => continue,
        DirChainResult::HasConfigFile(config_file) => {
          push_dedup_config_file(&mut output.config_files, config_file);
          continue;
        }
      }
    }
    if glob_matcher.matches(&file_path) {
      output.file_paths.push(file_path);
    }
  }

  if run_traversal {
    let gitignore = git_ignore_options.map(|options| DirScanGitIgnore {
      above_start_dir: if environment.path_exists(opts.start_dir.join(".git")) {
        None
      } else {
        git_ignore_tree.as_mut().and_then(|tree| tree.get_resolved_git_ignore_for_file(&opts.start_dir))
      },
      options,
    });
    let results = scan_dir(
      environment,
      DirScanOptions {
        start_dir: opts.start_dir,
        matcher: glob_matcher.clone(),
        gitignore,
        discover_configs,
        current_config_path: opts.current_config_path,
      },
    )?;
    output.file_paths.extend(results.file_paths);
    for config_file in results.config_files {
      // the scan skips directories the checks above already handled, so this
      // shouldn't overlap. Dedup anyway: a duplicate would resolve the same
      // scope, and format its files, twice
      push_dedup_config_file(&mut output.config_files, config_file);
    }
  }

  log_debug!(
    environment,
    "Matched {} candidate files, {} nested configs, {} outside paths:",
    output.file_paths.len(),
    output.config_files.len(),
    output.outside_base_paths.len()
  );
  for path in &output.file_paths {
    log_debug!(environment, "  File: {}", path.display());
  }
  for path in &output.config_files {
    log_debug!(environment, "  Nested config: {}", path.display());
  }
  for path in &output.outside_base_paths {
    log_debug!(
      environment,
      "  Outside path: {} (config search: {})",
      path.include_pattern,
      path.config_search_dir.display()
    );
  }
  log_debug!(environment, "Finished globbing in {}ms", start_instant.elapsed().as_millis());

  Ok(output)
}

struct LiteralArgPaths {
  /// Files to match in the current scope.
  file_paths: Vec<PathBuf>,
  /// Existing paths outside the pattern base directory.
  outside_base_paths: Vec<OutsideBasePath>,
}

/// Resolves the positive literal CLI patterns against the file system.
///
/// Existing files are returned so they can be matched directly without any
/// directory traversal, and patterns for existing directories are expanded to
/// match everything within them (ex. `dprint fmt some_dir`). A glob-like
/// pattern whose text names an existing path is treated as that path instead
/// of a glob (ex. `dprint fmt routes/[id].svelte`). Paths outside the pattern
/// base directory are returned separately because the config file to use for
/// them needs to be resolved by the caller.
fn extract_literal_arg_paths(environment: &impl Environment, file_patterns: &mut GlobPatterns, pattern_base: &CanonicalizedPathBuf) -> LiteralArgPaths {
  let mut seen = HashSet::new();
  let mut result = LiteralArgPaths {
    file_paths: Vec::new(),
    outside_base_paths: Vec::new(),
  };
  for pattern in file_patterns.arg_includes.iter_mut().flatten() {
    if pattern.is_negated() {
      // a negated arg with an existing literal name should skip that path
      // instead of matching as a glob (ex. `!routes/[id].svelte`)
      rewrite_literal_arg_pattern(environment, pattern, pattern_base);
      continue;
    }
    if !could_be_literal_path(&pattern.relative_pattern) {
      continue;
    }
    let relative_path = pattern.relative_pattern.strip_prefix("./").unwrap_or(&pattern.relative_pattern);
    let relative_path = relative_path.trim_end_matches('/');
    // unescape so escaped glob characters resolve to the actual path
    // (ex. `\[a\]/file.js`)
    let relative_path = unescape_glob_text(relative_path);
    let (path, is_file) = if relative_path.is_empty() {
      // the pattern resolved to its base directory itself (ex. `dprint fmt ..`)
      (pattern.base_dir.as_ref().to_path_buf(), false)
    } else {
      // use platform-style separators so the resolved paths are
      // consistent with the paths a traversal produces
      let path = if cfg!(windows) {
        pattern.base_dir.join(relative_path.replace('/', "\\"))
      } else {
        pattern.base_dir.join(relative_path.as_ref())
      };
      let Some(kind) = environment.path_kind(&path) else {
        // a glob-like pattern stays a glob when nothing has its literal name
        continue;
      };
      // resolve symlinks and casing differences so the path gets classified
      // and matched based on where it actually is on the file system
      let path = match environment.canonicalize(&path) {
        Ok(canonical) => canonical.into_path_buf(),
        Err(_) => path,
      };
      let is_file = match kind {
        PathKind::File => true,
        PathKind::Dir => false,
        // stat the canonicalized path to see what the symlink points at
        PathKind::Symlink => match environment.path_kind(&path) {
          Some(PathKind::File) => true,
          Some(PathKind::Dir) => false,
          // a broken symlink stays a glob like a nonexistent path
          _ => continue,
        },
      };
      (path, is_file)
    };
    if !is_file && pattern_base.as_ref().starts_with(&path) {
      // a directory at or above the pattern base directory, so match
      // everything in the current scope
      if path != *pattern_base.as_ref() && seen.insert(path.clone()) {
        // also resolve the parts of the ancestor directory outside the
        // current scope separately (ex. sibling directories with their own
        // config files)
        result.outside_base_paths.push(OutsideBasePath {
          config_search_dir: path.clone(),
          include_pattern: path.to_string_lossy().into_owned(),
        });
      }
      pattern.relative_pattern = "**".to_string();
      pattern.base_dir = pattern_base.clone();
    } else if !path.starts_with(pattern_base.as_ref()) {
      if seen.insert(path.clone()) {
        result.outside_base_paths.push(OutsideBasePath {
          config_search_dir: if is_file {
            path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| path.clone())
          } else {
            path.clone()
          },
          include_pattern: path.to_string_lossy().into_owned(),
        });
      }
      // escape so the retained pattern is treated as a literal path instead
      // of a glob (the outside scope resolves the path itself)
      pattern.relative_pattern = format!("./{}", escape_glob_text(&relative_path));
    } else {
      // rewrite the pattern to the canonicalized path so the matcher's
      // literal path set agrees with the resolved path (escaping so glob
      // characters in the path match literally)
      let relative = path.strip_prefix(pattern_base.as_ref()).unwrap().to_string_lossy().replace('\\', "/");
      pattern.base_dir = pattern_base.clone();
      if is_file {
        pattern.relative_pattern = format!("./{}", escape_glob_text(&relative));
        if seen.insert(path.clone()) {
          result.file_paths.push(path);
        }
      } else {
        // an existing directory, so match everything within it
        pattern.relative_pattern = format!("./{}/**", escape_glob_text(&relative));
      }
    }
  }
  result
}

/// Rewrites the CLI arg patterns whose text names an existing path to match
/// that path literally, for matchers built without a full `glob()` (ex.
/// `dprint fmt --stdin`). Within `glob()` the positive include args are
/// instead handled by `extract_literal_arg_paths` because they additionally
/// resolve directory contents and outside paths.
pub fn rewrite_literal_arg_patterns(environment: &impl Environment, file_patterns: &mut GlobPatterns, pattern_base: &CanonicalizedPathBuf) {
  for pattern in file_patterns.arg_includes.iter_mut().flatten() {
    rewrite_literal_arg_pattern(environment, pattern, pattern_base);
  }
  rewrite_literal_exclude_paths(environment, file_patterns, pattern_base);
}

/// Rewrites the exclude args whose text names an existing path to match that
/// path literally, the same way include args are resolved (ex. `--excludes
/// "[a]"` excludes a directory named `[a]` instead of matching as a character
/// class).
fn rewrite_literal_exclude_paths(environment: &impl Environment, file_patterns: &mut GlobPatterns, pattern_base: &CanonicalizedPathBuf) {
  for pattern in file_patterns.arg_excludes.iter_mut().flatten() {
    rewrite_literal_arg_pattern(environment, pattern, pattern_base);
  }
}

/// Rewrites a single arg pattern to match literally when its text names an
/// existing path, preserving any negation (ex. `!routes/[id].svelte` skips
/// the file with that name instead of matching as a character class).
pub fn rewrite_literal_arg_pattern(environment: &impl Environment, pattern: &mut GlobPattern, pattern_base: &CanonicalizedPathBuf) {
  let is_negated = pattern.is_negated();
  let text = non_negated_glob(&pattern.relative_pattern);
  if !is_pattern(text) || !could_be_literal_path(text) {
    return;
  }
  let relative_path = text.strip_prefix("./").unwrap_or(text);
  let relative_path = relative_path.trim_end_matches('/');
  if relative_path.is_empty() {
    return;
  }
  let path = if cfg!(windows) {
    pattern.base_dir.join(relative_path.replace('/', "\\"))
  } else {
    pattern.base_dir.join(relative_path)
  };
  if !environment.path_exists(&path) {
    return;
  }
  let canonical_path = match environment.canonicalize(&path) {
    Ok(canonical) => canonical.into_path_buf(),
    Err(_) => path,
  };
  // rebase onto the pattern base when the canonical path is within it,
  // otherwise keep the uncanonicalized name (ex. a symlink pointing outside
  // the base still gets excluded by the name a traversal encounters)
  let (base_dir, relative) = match canonical_path.strip_prefix(pattern_base.as_ref()) {
    Ok(relative) if !relative.as_os_str().is_empty() => (pattern_base.clone(), relative.to_string_lossy().replace('\\', "/")),
    _ => (pattern.base_dir.clone(), relative_path.to_string()),
  };
  pattern.base_dir = base_dir;
  pattern.relative_pattern = format!("{}./{}", if is_negated { "!" } else { "" }, escape_glob_text(&relative));
}

/// Gets whether the pattern's text could name a literal path on the file
/// system and so is worth checking for existence.
///
/// Patterns with `*` or `?` are always treated as globs (`*` isn't even
/// allowed in Windows file names), but `[` and `{` are valid in file names
/// (ex. `routes/[id].svelte`), so those are resolved against the file system
/// (see issues #552, #920, #947).
fn could_be_literal_path(pattern: &str) -> bool {
  let mut was_last_escape = false;
  for c in pattern.chars() {
    if !was_last_escape && matches!(c, '*' | '?') {
      return false;
    }
    // consume backslashes in pairs the same way `unescape_glob_text` does so
    // an escaped backslash doesn't escape the character after it
    // (ex. the `*` in `a\\*b` is a glob star)
    was_last_escape = c == '\\' && !was_last_escape;
  }
  true
}

/// Extracts the positive CLI glob patterns based outside the pattern base
/// directory (ex. `dprint fmt ../other/**` when `../other` is outside the
/// config's directory or on another drive). They can never match anything in
/// the current scope, so the caller resolves the config file to use for them
/// separately the same way it does for literal paths outside the base. A
/// pattern based at an ancestor of the base is kept for the current scope and
/// additionally resolved separately for the parts outside the scope.
fn extract_outside_arg_patterns(file_patterns: &mut GlobPatterns, pattern_base: &CanonicalizedPathBuf, outside_base_paths: &mut Vec<OutsideBasePath>) {
  let Some(includes) = &mut file_patterns.arg_includes else {
    return;
  };
  includes.retain(|pattern| {
    if !is_positive_glob_pattern(pattern) {
      return true;
    }
    let deepest_base = pattern.clone().into_deepest_base().base_dir;
    if deepest_base.starts_with(pattern_base) {
      return true; // only matches within the scope
    }
    outside_base_paths.push(OutsideBasePath {
      config_search_dir: deepest_base.as_ref().to_path_buf(),
      include_pattern: pattern.as_absolute_pattern_text(),
    });
    // a pattern based at an ancestor directory also matches within the scope
    pattern_base.starts_with(&deepest_base)
  });
}

/// Gets whether resolving the patterns requires traversing the file system.
///
/// Traversal isn't necessary when the CLI args are all literal file paths
/// because those are resolved directly against the file system.
fn requires_traversal(file_patterns: &GlobPatterns) -> bool {
  match &file_patterns.arg_includes {
    // no CLI paths were provided, so traverse for the config includes
    None => true,
    Some(includes) => includes.iter().any(is_positive_glob_pattern),
  }
}

/// Gets whether the pattern is a non-negated glob pattern
/// as opposed to a literal path.
fn is_positive_glob_pattern(pattern: &GlobPattern) -> bool {
  !pattern.is_negated() && is_pattern(&pattern.relative_pattern)
}

/// Moves the traversal start directory up to the nearest ancestor directory
/// containing the positive CLI glob patterns so files outside the start
/// directory can be found (ex. `dprint fmt ../sub_dir/**/*.ts`). This stays
/// fast because the traversal quickly prunes directories the patterns can't
/// match (see `GlobPattern::matches_dir_for_traversal`).
fn expand_start_dir_for_arg_patterns(mut start_dir: PathBuf, file_patterns: &GlobPatterns, pattern_base: &CanonicalizedPathBuf) -> PathBuf {
  for pattern in file_patterns.arg_includes.iter().flatten() {
    if !is_positive_glob_pattern(pattern) {
      continue;
    }
    let deepest_base = pattern.clone().into_deepest_base().base_dir;
    if !deepest_base.starts_with(pattern_base) {
      continue;
    }
    // move the start dir up (bounded by the pattern base) until it contains
    // the pattern's base directory
    while !deepest_base.as_ref().starts_with(&start_dir) {
      match start_dir.parent() {
        Some(parent) if start_dir != *pattern_base.as_ref() => start_dir = parent.to_path_buf(),
        _ => break,
      }
    }
  }
  start_dir
}

enum DirChainResult {
  /// Nothing along the chain prevents matching within the directory.
  Matched,
  /// A directory along the chain is excluded or gitignored.
  Excluded,
  /// A directory along the chain has its own config file, so the sub scope
  /// created for that config file handles everything within it.
  HasConfigFile(PathBuf),
}

/// Checks the directories between the base directory (exclusive) and the
/// provided directory (inclusive) top down the same way a traversal
/// descending into them would.
fn check_dir_chain<TEnvironment: Environment>(
  glob_matcher: &GlobMatcher,
  git_ignore_tree: &mut Option<GitIgnoreTree<TEnvironment>>,
  mut config_file_finder: Option<&mut DirConfigFileFinder<'_, TEnvironment>>,
  base_dir: &Path,
  dir: &Path,
) -> DirChainResult {
  for dir in dirs_from_base_to(base_dir, dir) {
    if dir.file_name().is_some_and(|f| f == ".git") {
      return DirChainResult::Excluded;
    }
    match glob_matcher.is_dir_ignored(dir) {
      ExcludeMatchDetail::Excluded => return DirChainResult::Excluded,
      ExcludeMatchDetail::OptedOutExclude => {}
      ExcludeMatchDetail::NotExcluded => {
        if let Some(tree) = git_ignore_tree.as_mut()
          && let Some(gitignore) = tree.get_resolved_git_ignore_for_file(dir)
          && gitignore.is_ignored(dir, /* is dir */ true)
        {
          return DirChainResult::Excluded;
        }
      }
    }
    if let Some(finder) = config_file_finder.as_deref_mut()
      && let Some(config_file) = finder.find(dir)
    {
      return DirChainResult::HasConfigFile(config_file);
    }
  }
  DirChainResult::Matched
}

/// Gets the directories between the base directory (exclusive) and the
/// provided directory (inclusive) ordered top down.
fn dirs_from_base_to<'a>(base_dir: &Path, dir: &'a Path) -> Vec<&'a Path> {
  let mut dirs = dir.ancestors().take_while(|ancestor| *ancestor != base_dir).collect::<Vec<_>>();
  dirs.reverse();
  dirs
}

/// Adds a config file found along a directory chain, deduplicating because
/// multiple file paths often resolve to the same config file.
fn push_dedup_config_file(config_files: &mut Vec<PathBuf>, config_file: PathBuf) {
  if !config_files.contains(&config_file) {
    config_files.push(config_file);
  }
}

/// Finds the dprint config file within a directory, caching the result per
/// directory because multiple file paths often share ancestor directories.
struct DirConfigFileFinder<'a, TEnvironment: Environment> {
  environment: &'a TEnvironment,
  current_config_path: Option<PathBuf>,
  cache: HashMap<PathBuf, Option<PathBuf>>,
}

impl<'a, TEnvironment: Environment> DirConfigFileFinder<'a, TEnvironment> {
  pub fn new(environment: &'a TEnvironment, current_config_path: Option<PathBuf>) -> Self {
    Self {
      environment,
      current_config_path,
      cache: Default::default(),
    }
  }

  pub fn find(&mut self, dir: &Path) -> Option<PathBuf> {
    if let Some(result) = self.cache.get(dir) {
      return result.clone();
    }
    let result = POSSIBLE_CONFIG_FILE_NAMES
      .iter()
      .map(|file_name| dir.join(file_name))
      // the config file already in use doesn't create a new scope
      .filter(|path| Some(path.as_path()) != self.current_config_path.as_deref())
      .find(|path| self.environment.path_is_file(path));
    self.cache.insert(dir.to_path_buf(), result.clone());
    result
  }
}

#[cfg(test)]
mod test {
  use dprint_platform::environment::*;
  use pretty_assertions::assert_eq;

  use super::*;
  use crate::environment::TestEnvironmentBuilder;
  use crate::utils::GlobPattern;

  // `glob` internally uses tokio::spawn_blocking so that's why these
  // are using tokio::test, then that requires async
  #[tokio::test]
  async fn should_glob() {
    let mut environment_builder = TestEnvironmentBuilder::new();
    let mut expected_matches = Vec::new();
    // ignores .git folder
    environment_builder.write_file("/.git/data.txt", "");
    for i in 1..100 {
      environment_builder.write_file(format!("/{}.txt", i), "");
      expected_matches.push(format!("/{}.txt", i));

      environment_builder.write_file(format!("/sub/{}.txt", i), "");
      expected_matches.push(format!("/sub/{}.txt", i));

      environment_builder.write_file(format!("/sub/ignore/{}.txt", i), "");

      environment_builder.write_file(format!("/sub{0}/sub/{0}.txt", i), "");
      expected_matches.push(format!("/sub{0}/sub/{0}.txt", i));

      if i % 2 == 0 {
        environment_builder.write_file(format!("/{}.ps", i), "");
        environment_builder.write_file(format!("/sub/{}.ps", i), "");
        environment_builder.write_file(format!("/sub/ignore/{}.ps", i), "");
        environment_builder.write_file(format!("/sub{0}/sub/{0}.ps", i), "");
      }
    }

    let environment = environment_builder.build();
    let root_dir = environment.canonicalize("/").unwrap();
    let result = glob(
      &environment,
      GlobOptions {
        current_config_path: None,
        start_dir: PathBuf::from("/"),
        config_discovery: ConfigDiscovery::Default,
        file_patterns: GlobPatterns {
          shebangs: Vec::new(),
          arg_includes: None,
          config_includes: Some(vec![GlobPattern::new("**/*.txt".to_string(), root_dir.clone())]),
          arg_excludes: None,
          config_excludes: vec![GlobPattern::new("**/ignore".to_string(), root_dir)],
        },
        pattern_base: CanonicalizedPathBuf::new_for_testing("/"),
        no_gitignore: false,
      },
    )
    .unwrap();
    let mut result = result.file_paths.into_iter().map(|r| r.to_string_lossy().to_string()).collect::<Vec<_>>();
    result.sort();
    expected_matches.sort();
    assert_eq!(result, expected_matches);
  }

  #[tokio::test]
  async fn should_find_extensionless_files_without_reading_them() {
    let mut builder = TestEnvironmentBuilder::new();
    builder.write_file("/a.txt", "");
    builder.write_file("/scripts/build", "#!/bin/sh\ntext");
    builder.write_file("/scripts/notes", "text");
    builder.write_file("/scripts/other.md", "#!/bin/sh\ntext");
    let environment = builder.build();
    let root_dir = environment.canonicalize("/").unwrap();
    let result = glob(
      &environment,
      GlobOptions {
        current_config_path: None,
        start_dir: PathBuf::from("/"),
        config_discovery: ConfigDiscovery::Default,
        file_patterns: GlobPatterns {
          shebangs: vec!["#!/bin/sh".to_string()],
          arg_includes: None,
          config_includes: Some(vec![GlobPattern::new("**/*.txt".to_string(), root_dir)]),
          arg_excludes: None,
          config_excludes: Vec::new(),
        },
        pattern_base: CanonicalizedPathBuf::new_for_testing("/"),
        no_gitignore: false,
      },
    )
    .unwrap();
    let mut file_paths = result.file_paths.iter().map(|p| p.to_string_lossy().to_string()).collect::<Vec<_>>();
    file_paths.sort();
    // every extensionless file is found, whatever its first line is: plugin
    // resolution reads the shebang line and drops `notes`. a file with an
    // extension is only found by the patterns
    assert_eq!(file_paths, vec!["/a.txt", "/scripts/build", "/scripts/notes"]);
  }

  #[tokio::test]
  async fn should_match_a_wide_tree() {
    // a wide and deep tree with a gitignore at the root, excludes, an opted
    // out exclude and extensionless files
    let mut builder = TestEnvironmentBuilder::new();
    builder.write_file("/.git/HEAD", "");
    // `keep.txt` is gitignored, so opting it back in has to beat the
    // gitignore as well as the exclude
    builder.write_file("/.gitignore", "ignored\nkeep.txt\n");
    for i in 0..200 {
      builder.write_file(format!("/dir{}/a.txt", i), "");
      builder.write_file(format!("/dir{}/nested/deep/b.txt", i), "");
      // excluded by the root .gitignore
      builder.write_file(format!("/dir{}/ignored/c.txt", i), "");
      // excluded by the config excludes
      builder.write_file(format!("/dir{}/skip/d.txt", i), "");
      // opted back in, beating both the exclude and the gitignore
      builder.write_file(format!("/dir{}/skip/keep.txt", i), "");
      // extensionless, so found for plugin resolution to check the shebang
      builder.write_file(format!("/dir{}/script", i), "#!/bin/sh\n");
      builder.write_file(format!("/dir{}/notes", i), "not a script\n");
    }
    let environment = builder.build();
    let root_dir = environment.canonicalize("/").unwrap();
    let result = glob(
      &environment,
      GlobOptions {
        current_config_path: None,
        start_dir: PathBuf::from("/"),
        config_discovery: ConfigDiscovery::Default,
        file_patterns: GlobPatterns {
          shebangs: vec!["#!/bin/sh".to_string()],
          arg_includes: None,
          config_includes: Some(vec![GlobPattern::new("**/*.txt".to_string(), root_dir.clone())]),
          arg_excludes: None,
          config_excludes: vec![
            GlobPattern::new("**/skip/**".to_string(), root_dir.clone()),
            GlobPattern::new("!**/skip/keep.txt".to_string(), root_dir.clone()),
          ],
        },
        pattern_base: CanonicalizedPathBuf::new_for_testing("/"),
        no_gitignore: false,
      },
    )
    .unwrap();
    let mut result = result.file_paths.into_iter().map(|r| r.to_string_lossy().to_string()).collect::<Vec<_>>();
    result.sort();
    // a.txt, nested/deep/b.txt, skip/keep.txt, script and notes per dir, and
    // the root `.gitignore`, which has no extension either
    assert_eq!(result.len(), 1001);
    assert!(result.contains(&"/.gitignore".to_string()));
    assert!(result.iter().all(|p| !p.contains("ignored")));
    assert_eq!(result.iter().filter(|p| p.ends_with("/skip/keep.txt")).count(), 200);
    assert_eq!(result.iter().filter(|p| p.ends_with("/skip/d.txt")).count(), 0);
    assert_eq!(result.iter().filter(|p| p.ends_with("/script")).count(), 200);
    assert_eq!(result.iter().filter(|p| p.ends_with("/notes")).count(), 200);
  }

  #[tokio::test]
  async fn should_skip_a_directory_where_nothing_matched() {
    // nothing in `/sub` matches: the gitignore and the binary don't match the
    // includes, and `skip` is excluded
    let environment = TestEnvironmentBuilder::new()
      .write_file("/a.txt", "")
      .write_file("/sub/.gitignore", "whatever\n")
      .write_file("/sub/only.bin", "")
      .write_file("/sub/skip/b.txt", "")
      .build();
    let root_dir = environment.canonicalize("/").unwrap();
    let result = glob(
      &environment,
      GlobOptions {
        current_config_path: None,
        start_dir: PathBuf::from("/"),
        config_discovery: ConfigDiscovery::Default,
        file_patterns: GlobPatterns {
          shebangs: Vec::new(),
          arg_includes: None,
          config_includes: Some(vec![GlobPattern::new("**/*.txt".to_string(), root_dir.clone())]),
          arg_excludes: None,
          config_excludes: vec![GlobPattern::new("**/skip".to_string(), root_dir)],
        },
        pattern_base: CanonicalizedPathBuf::new_for_testing("/"),
        no_gitignore: false,
      },
    )
    .unwrap();

    let result = result.file_paths.into_iter().map(|r| r.to_string_lossy().to_string()).collect::<Vec<_>>();
    assert_eq!(result, vec!["/a.txt"]);
  }

  #[tokio::test]
  async fn should_respect_gitignore_that_doesnt_match_the_patterns() {
    // the `.gitignore` doesn't match the includes, but it must still be read
    let environment = TestEnvironmentBuilder::new()
      .write_file("/sub/.gitignore", "ignored\n")
      .write_file("/sub/ignored/a.txt", "")
      .write_file("/sub/keep/b.txt", "")
      .build();
    let root_dir = environment.canonicalize("/").unwrap();
    let result = glob(
      &environment,
      GlobOptions {
        current_config_path: None,
        start_dir: PathBuf::from("/"),
        config_discovery: ConfigDiscovery::Default,
        file_patterns: GlobPatterns {
          shebangs: Vec::new(),
          arg_includes: None,
          config_includes: Some(vec![GlobPattern::new("**/*.txt".to_string(), root_dir)]),
          arg_excludes: None,
          config_excludes: Vec::new(),
        },
        pattern_base: CanonicalizedPathBuf::new_for_testing("/"),
        no_gitignore: false,
      },
    )
    .unwrap();

    let result = result.file_paths.into_iter().map(|r| r.to_string_lossy().to_string()).collect::<Vec<_>>();
    assert_eq!(result, vec!["/sub/keep/b.txt"]);
  }

  #[tokio::test]
  async fn should_respect_git_info_exclude() {
    let environment = TestEnvironmentBuilder::new()
      .write_file("/.git/info/exclude", "excluded.txt")
      .write_file("/included.txt", "")
      .write_file("/excluded.txt", "")
      .write_file("/sub/included.txt", "")
      .write_file("/sub/excluded.txt", "")
      .build();
    let root_dir = environment.canonicalize("/").unwrap();
    let result = glob(
      &environment,
      GlobOptions {
        current_config_path: None,
        start_dir: PathBuf::from("/"),
        config_discovery: ConfigDiscovery::Default,
        file_patterns: GlobPatterns {
          shebangs: Vec::new(),
          arg_includes: None,
          config_includes: Some(vec![GlobPattern::new("**/*.txt".to_string(), root_dir)]),
          arg_excludes: None,
          config_excludes: Vec::new(),
        },
        pattern_base: CanonicalizedPathBuf::new_for_testing("/"),
        no_gitignore: false,
      },
    )
    .unwrap();

    let mut result = result.file_paths.into_iter().map(|r| r.to_string_lossy().to_string()).collect::<Vec<_>>();
    result.sort();
    assert_eq!(result, vec!["/included.txt", "/sub/included.txt"]);
  }

  #[tokio::test]
  async fn should_respect_global_gitignore_by_default() {
    let environment = TestEnvironmentBuilder::new()
      // a `.git` dir makes `/` the repository root, where the global excludes apply
      .write_file("/.git/HEAD", "")
      .write_file("/global_ignore", "globally_excluded.txt")
      .write_file("/included.txt", "")
      .write_file("/globally_excluded.txt", "")
      .write_file("/sub/included.txt", "")
      .write_file("/sub/globally_excluded.txt", "")
      .build();
    environment.set_global_gitignore_path("/global_ignore");
    let root_dir = environment.canonicalize("/").unwrap();
    let result = glob(
      &environment,
      GlobOptions {
        current_config_path: None,
        start_dir: PathBuf::from("/"),
        config_discovery: ConfigDiscovery::Default,
        file_patterns: GlobPatterns {
          shebangs: Vec::new(),
          arg_includes: None,
          config_includes: Some(vec![GlobPattern::new("**/*.txt".to_string(), root_dir)]),
          arg_excludes: None,
          config_excludes: Vec::new(),
        },
        pattern_base: CanonicalizedPathBuf::new_for_testing("/"),
        no_gitignore: false,
      },
    )
    .unwrap();

    let mut result = result.file_paths.into_iter().map(|r| r.to_string_lossy().to_string()).collect::<Vec<_>>();
    result.sort();
    // the global excludes apply at the repo root and to its descendants
    assert_eq!(result, vec!["/included.txt", "/sub/included.txt"]);
  }

  #[tokio::test]
  async fn should_ignore_global_gitignore_when_explicitly_disabled() {
    let environment = TestEnvironmentBuilder::new()
      .write_file("/.git/HEAD", "")
      .write_file("/global_ignore", "globally_excluded.txt")
      .write_file("/included.txt", "")
      .write_file("/globally_excluded.txt", "")
      .build();
    environment.set_env_var("DPRINT_GLOBAL_GITIGNORE", Some("0"));
    environment.set_global_gitignore_path("/global_ignore");
    let root_dir = environment.canonicalize("/").unwrap();
    let result = glob(
      &environment,
      GlobOptions {
        current_config_path: None,
        start_dir: PathBuf::from("/"),
        config_discovery: ConfigDiscovery::Default,
        file_patterns: GlobPatterns {
          shebangs: Vec::new(),
          arg_includes: None,
          config_includes: Some(vec![GlobPattern::new("**/*.txt".to_string(), root_dir)]),
          arg_excludes: None,
          config_excludes: Vec::new(),
        },
        pattern_base: CanonicalizedPathBuf::new_for_testing("/"),
        no_gitignore: false,
      },
    )
    .unwrap();

    let mut result = result.file_paths.into_iter().map(|r| r.to_string_lossy().to_string()).collect::<Vec<_>>();
    result.sort();
    assert_eq!(result, vec!["/globally_excluded.txt", "/included.txt"]);
  }

  #[tokio::test]
  async fn no_gitignore_should_override_global_gitignore() {
    let environment = TestEnvironmentBuilder::new()
      .write_file("/.git/HEAD", "")
      .write_file("/global_ignore", "globally_excluded.txt")
      .write_file("/included.txt", "")
      .write_file("/globally_excluded.txt", "")
      .build();
    environment.set_env_var("DPRINT_GLOBAL_GITIGNORE", Some("1"));
    environment.set_global_gitignore_path("/global_ignore");
    let root_dir = environment.canonicalize("/").unwrap();
    let result = glob(
      &environment,
      GlobOptions {
        current_config_path: None,
        start_dir: PathBuf::from("/"),
        config_discovery: ConfigDiscovery::Default,
        file_patterns: GlobPatterns {
          shebangs: Vec::new(),
          arg_includes: None,
          config_includes: Some(vec![GlobPattern::new("**/*.txt".to_string(), root_dir)]),
          arg_excludes: None,
          config_excludes: Vec::new(),
        },
        pattern_base: CanonicalizedPathBuf::new_for_testing("/"),
        // `--no-gitignore` disables all gitignore handling, including the global file
        no_gitignore: true,
      },
    )
    .unwrap();

    let mut result = result.file_paths.into_iter().map(|r| r.to_string_lossy().to_string()).collect::<Vec<_>>();
    result.sort();
    assert_eq!(result, vec!["/globally_excluded.txt", "/included.txt"]);
  }

  #[tokio::test]
  async fn should_match_literal_file_paths_without_traversal() {
    let environment = TestEnvironmentBuilder::new()
      .write_file("/sub/file.txt", "")
      .write_file("/sub/other.txt", "")
      .build();
    // error any attempt at reading a directory in order to
    // prove that literal file paths don't cause a traversal
    environment.set_dir_info_error(std::io::Error::other("FAILURE"));
    let root_dir = environment.canonicalize("/").unwrap();
    let result = glob(
      &environment,
      GlobOptions {
        current_config_path: None,
        start_dir: PathBuf::from("/"),
        config_discovery: ConfigDiscovery::Default,
        file_patterns: GlobPatterns {
          shebangs: Vec::new(),
          arg_includes: Some(vec![
            GlobPattern::new("./sub/file.txt".to_string(), root_dir.clone()),
            GlobPattern::new("./not_exists.txt".to_string(), root_dir.clone()),
          ]),
          config_includes: Some(vec![GlobPattern::new("**/*.txt".to_string(), root_dir)]),
          arg_excludes: None,
          config_excludes: Vec::new(),
        },
        pattern_base: CanonicalizedPathBuf::new_for_testing("/"),
        no_gitignore: false,
      },
    )
    .unwrap();
    assert_eq!(result.file_paths, vec![PathBuf::from("/sub/file.txt")]);
    assert!(result.config_files.is_empty());
  }

  #[tokio::test]
  async fn literal_file_path_in_dir_with_config_file_resolves_config() {
    let environment = TestEnvironmentBuilder::new()
      .write_file("/sub/dprint.json", "{}")
      .write_file("/sub/file.txt", "")
      .write_file("/sub/nested/dprint.json", "{}")
      .write_file("/sub/nested/file.txt", "")
      .write_file("/file.txt", "")
      .build();
    let root_dir = environment.canonicalize("/").unwrap();
    let result = glob(
      &environment,
      GlobOptions {
        current_config_path: None,
        start_dir: PathBuf::from("/"),
        config_discovery: ConfigDiscovery::Default,
        file_patterns: GlobPatterns {
          shebangs: Vec::new(),
          arg_includes: Some(vec![
            GlobPattern::new("./file.txt".to_string(), root_dir.clone()),
            GlobPattern::new("./sub/file.txt".to_string(), root_dir.clone()),
            GlobPattern::new("./sub/nested/file.txt".to_string(), root_dir.clone()),
          ]),
          config_includes: Some(vec![GlobPattern::new("**/*.txt".to_string(), root_dir)]),
          arg_excludes: None,
          config_excludes: Vec::new(),
        },
        pattern_base: CanonicalizedPathBuf::new_for_testing("/"),
        no_gitignore: false,
      },
    )
    .unwrap();
    // only the file directly in the current scope is matched and both files
    // below the sub config resolve to the shallowest config file, which will
    // discover the nested one recursively when its scope resolves
    assert_eq!(result.file_paths, vec![PathBuf::from("/file.txt")]);
    assert_eq!(result.config_files, vec![PathBuf::from("/sub/dprint.json")]);
  }

  #[tokio::test]
  async fn literal_file_path_ignores_config_files_when_config_discovery_disabled() {
    let environment = TestEnvironmentBuilder::new()
      .write_file("/sub/dprint.json", "{}")
      .write_file("/sub/file.txt", "")
      .build();
    let root_dir = environment.canonicalize("/").unwrap();
    let result = glob(
      &environment,
      GlobOptions {
        current_config_path: None,
        start_dir: PathBuf::from("/"),
        config_discovery: ConfigDiscovery::Disabled,
        file_patterns: GlobPatterns {
          shebangs: Vec::new(),
          arg_includes: Some(vec![GlobPattern::new("./sub/file.txt".to_string(), root_dir.clone())]),
          config_includes: Some(vec![GlobPattern::new("**/*.txt".to_string(), root_dir)]),
          arg_excludes: None,
          config_excludes: Vec::new(),
        },
        pattern_base: CanonicalizedPathBuf::new_for_testing("/"),
        no_gitignore: false,
      },
    )
    .unwrap();
    assert_eq!(result.file_paths, vec![PathBuf::from("/sub/file.txt")]);
    assert!(result.config_files.is_empty());
  }

  #[tokio::test]
  async fn literal_file_path_in_excluded_dir_not_matched() {
    let environment = TestEnvironmentBuilder::new()
      .write_file("/ignored/file.txt", "")
      // the config file is not discovered either because the
      // directory it's in is excluded
      .write_file("/ignored/dprint.json", "{}")
      .build();
    let root_dir = environment.canonicalize("/").unwrap();
    let result = glob(
      &environment,
      GlobOptions {
        current_config_path: None,
        start_dir: PathBuf::from("/"),
        config_discovery: ConfigDiscovery::Default,
        file_patterns: GlobPatterns {
          shebangs: Vec::new(),
          arg_includes: Some(vec![GlobPattern::new("./ignored/file.txt".to_string(), root_dir.clone())]),
          config_includes: Some(vec![GlobPattern::new("**/*.txt".to_string(), root_dir.clone())]),
          arg_excludes: None,
          config_excludes: vec![GlobPattern::new("./ignored".to_string(), root_dir)],
        },
        pattern_base: CanonicalizedPathBuf::new_for_testing("/"),
        no_gitignore: false,
      },
    )
    .unwrap();
    assert!(result.file_paths.is_empty());
    assert!(result.config_files.is_empty());
  }

  #[tokio::test]
  async fn should_traverse_from_glob_pattern_base_dir_outside_start_dir() {
    let environment = TestEnvironmentBuilder::new()
      .write_file("/other/file.txt", "")
      .write_file("/sub/file.txt", "")
      .build();
    let root_dir = environment.canonicalize("/").unwrap();
    let result = glob(
      &environment,
      GlobOptions {
        current_config_path: None,
        // this happens when running `dprint fmt "../other/**"` from /sub
        start_dir: PathBuf::from("/sub"),
        config_discovery: ConfigDiscovery::Default,
        file_patterns: GlobPatterns {
          shebangs: Vec::new(),
          arg_includes: Some(vec![GlobPattern::new("./other/**".to_string(), root_dir.clone())]),
          config_includes: Some(vec![GlobPattern::new("**/*.txt".to_string(), root_dir)]),
          arg_excludes: None,
          config_excludes: Vec::new(),
        },
        pattern_base: CanonicalizedPathBuf::new_for_testing("/"),
        no_gitignore: false,
      },
    )
    .unwrap();
    assert_eq!(result.file_paths, vec![PathBuf::from("/other/file.txt")]);
  }

  #[tokio::test]
  async fn should_expand_literal_dir_path_to_match_contents() {
    let environment = TestEnvironmentBuilder::new()
      .write_file("/sub/file1.txt", "")
      .write_file("/sub/nested/file2.txt", "")
      .write_file("/other/file3.txt", "")
      .build();
    let root_dir = environment.canonicalize("/").unwrap();
    let result = glob(
      &environment,
      GlobOptions {
        current_config_path: None,
        start_dir: PathBuf::from("/"),
        config_discovery: ConfigDiscovery::Default,
        file_patterns: GlobPatterns {
          shebangs: Vec::new(),
          arg_includes: Some(vec![GlobPattern::new("./sub".to_string(), root_dir.clone())]),
          config_includes: Some(vec![GlobPattern::new("**/*.txt".to_string(), root_dir)]),
          arg_excludes: None,
          config_excludes: Vec::new(),
        },
        pattern_base: CanonicalizedPathBuf::new_for_testing("/"),
        no_gitignore: false,
      },
    )
    .unwrap();
    let mut result = result.file_paths.into_iter().map(|r| r.to_string_lossy().to_string()).collect::<Vec<_>>();
    result.sort();
    assert_eq!(result, vec!["/sub/file1.txt", "/sub/nested/file2.txt"]);
  }

  #[tokio::test]
  async fn should_handle_dir_info_erroring() {
    let environment = TestEnvironmentBuilder::new().build();
    environment.set_dir_info_error(std::io::Error::other("FAILURE"));
    let root_dir = environment.canonicalize("/").unwrap();
    let err_message = glob(
      &environment,
      GlobOptions {
        current_config_path: None,
        start_dir: PathBuf::from("/"),
        config_discovery: ConfigDiscovery::Default,
        file_patterns: GlobPatterns {
          shebangs: Vec::new(),
          arg_includes: None,
          config_includes: Some(vec![GlobPattern::new("**/*.txt".to_string(), root_dir)]),
          arg_excludes: None,
          config_excludes: Vec::new(),
        },
        pattern_base: CanonicalizedPathBuf::new_for_testing("/"),
        no_gitignore: false,
      },
    )
    .err()
    .unwrap();
    assert_eq!(err_message.to_string(), "Error reading dir '/': FAILURE");
  }

  #[tokio::test]
  async fn should_ignore_permission_denied_error() {
    let environment = TestEnvironmentBuilder::new().build();
    environment.set_dir_info_error(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "Permission denied"));
    let root_dir = environment.canonicalize("/").unwrap();
    let result = glob(
      &environment,
      GlobOptions {
        current_config_path: None,
        start_dir: PathBuf::from("/"),
        config_discovery: ConfigDiscovery::Default,
        file_patterns: GlobPatterns {
          shebangs: Vec::new(),
          arg_includes: None,
          config_includes: Some(vec![GlobPattern::new("**/*.txt".to_string(), root_dir)]),
          arg_excludes: None,
          config_excludes: Vec::new(),
        },
        pattern_base: CanonicalizedPathBuf::new_for_testing("/"),
        no_gitignore: false,
      },
    );
    assert!(result.is_ok());
    assert_eq!(
      environment.take_stderr_messages(),
      vec!["WARNING: Ignoring directory. Permission denied: /".to_string()]
    );
  }

  #[tokio::test]
  async fn should_support_excluding_then_including_in_includes() {
    // this allows people to out out of everything then slowly opt back in
    let environment = TestEnvironmentBuilder::new().write_file("/dir/a.txt", "").write_file("/dir/b.txt", "").build();
    let root_dir = environment.canonicalize("/").unwrap();
    let result = glob(
      &environment,
      GlobOptions {
        current_config_path: None,
        start_dir: PathBuf::from("/"),
        config_discovery: ConfigDiscovery::Default,
        file_patterns: GlobPatterns {
          shebangs: Vec::new(),
          arg_includes: None,
          config_includes: Some(vec![
            GlobPattern::new("!**/*.*".to_string(), root_dir.clone()),
            GlobPattern::new("**/a.txt".to_string(), root_dir),
          ]),
          arg_excludes: None,
          config_excludes: Vec::new(),
        },
        pattern_base: CanonicalizedPathBuf::new_for_testing("/"),
        no_gitignore: false,
      },
    )
    .unwrap();

    let mut result = result.file_paths.into_iter().map(|r| r.to_string_lossy().to_string()).collect::<Vec<_>>();
    result.sort();
    assert_eq!(result, vec!["/dir/a.txt"]);
  }

  #[tokio::test]
  async fn should_support_including_then_excluding_then_including() {
    let environment = TestEnvironmentBuilder::new()
      .write_file("/dir/a.json", "")
      .write_file("/dir/b.json", "")
      .build();
    let root_dir = environment.canonicalize("/").unwrap();
    let result = glob(
      &environment,
      GlobOptions {
        current_config_path: None,
        start_dir: PathBuf::from("/"),
        config_discovery: ConfigDiscovery::Default,
        file_patterns: GlobPatterns {
          shebangs: Vec::new(),
          arg_includes: None,
          config_includes: Some(vec![
            GlobPattern::new("**/*.json".to_string(), root_dir.clone()),
            GlobPattern::new("!**/*.json".to_string(), root_dir.clone()),
            GlobPattern::new("**/a.json".to_string(), root_dir),
          ]),
          arg_excludes: None,
          config_excludes: Vec::new(),
        },
        pattern_base: CanonicalizedPathBuf::new_for_testing("/"),
        no_gitignore: false,
      },
    )
    .unwrap();

    let mut result = result.file_paths.into_iter().map(|r| r.to_string_lossy().to_string()).collect::<Vec<_>>();
    result.sort();
    assert_eq!(result, vec!["/dir/a.json"]);
  }

  #[tokio::test]
  async fn excluding_dir_but_including_sub_dir() {
    let environment = TestEnvironmentBuilder::new()
      .write_file("/test/a/a.json", "")
      .write_file("/test/a/b/b.json", "")
      .write_file("/test/test.json", "")
      .build();
    let test_dir = environment.canonicalize("/test/").unwrap();
    let result = glob(
      &environment,
      GlobOptions {
        current_config_path: None,
        start_dir: PathBuf::from("/test/"),
        config_discovery: ConfigDiscovery::Default,
        file_patterns: GlobPatterns {
          shebangs: Vec::new(),
          arg_includes: None,
          config_includes: Some(vec![
            GlobPattern::new("**/*.json".to_string(), test_dir.clone()),
            GlobPattern::new("!a/**/*.json".to_string(), test_dir.clone()),
            GlobPattern::new("a/b/**/*.json".to_string(), test_dir),
          ]),
          arg_excludes: None,
          config_excludes: Vec::new(),
        },
        pattern_base: CanonicalizedPathBuf::new_for_testing("/test/"),
        no_gitignore: false,
      },
    )
    .unwrap();

    let mut result = result.file_paths.into_iter().map(|r| r.to_string_lossy().to_string()).collect::<Vec<_>>();
    result.sort();
    assert_eq!(result, vec!["/test/a/b/b.json", "/test/test.json"]);
  }

  #[tokio::test]
  async fn should_be_case_sensitive() {
    // https://github.com/dprint/dprint/issues/1082
    let environment = TestEnvironmentBuilder::new()
      .write_file("/src/FooSamlService.java", "")
      .write_file("/src/FooMlService.java", "")
      .write_file("/src/Other.java", "")
      .build();
    let root_dir = environment.canonicalize("/").unwrap();
    let result = glob(
      &environment,
      GlobOptions {
        current_config_path: None,
        start_dir: PathBuf::from("/"),
        config_discovery: ConfigDiscovery::Default,
        file_patterns: GlobPatterns {
          shebangs: Vec::new(),
          arg_includes: None,
          config_includes: Some(vec![
            GlobPattern::new("**/*.java".to_string(), root_dir.clone()),
            GlobPattern::new("!**/*MlService.java".to_string(), root_dir),
          ]),
          arg_excludes: None,
          config_excludes: Vec::new(),
        },
        pattern_base: CanonicalizedPathBuf::new_for_testing("/"),
        no_gitignore: false,
      },
    )
    .unwrap();

    let mut result = result.file_paths.into_iter().map(|r| r.to_string_lossy().to_string()).collect::<Vec<_>>();
    result.sort();
    // FooSamlService.java should NOT be excluded — the pattern is *MlService, not *mlservice
    assert_eq!(result, vec!["/src/FooSamlService.java", "/src/Other.java"]);
  }

  #[tokio::test]
  async fn excluding_dir_but_including_sub_dir_case_2() {
    let environment = TestEnvironmentBuilder::new()
      .write_file("/dir/a/a.txt", "")
      .write_file("/dir/b/b.txt", "")
      .build();
    let root_dir = environment.canonicalize("/").unwrap();
    let result = glob(
      &environment,
      GlobOptions {
        current_config_path: None,
        start_dir: PathBuf::from("/"),
        config_discovery: ConfigDiscovery::Default,
        file_patterns: GlobPatterns {
          shebangs: Vec::new(),
          arg_includes: None,
          config_includes: Some(vec![
            GlobPattern::new("**/*.*".to_string(), root_dir.clone()),
            GlobPattern::new("!dir/a/**/*".to_string(), root_dir.clone()),
            GlobPattern::new("dir/b/b/**/*".to_string(), root_dir),
          ]),
          arg_excludes: None,
          config_excludes: Vec::new(),
        },
        pattern_base: CanonicalizedPathBuf::new_for_testing("/"),
        no_gitignore: false,
      },
    )
    .unwrap();

    let mut result = result.file_paths.into_iter().map(|r| r.to_string_lossy().to_string()).collect::<Vec<_>>();
    result.sort();
    assert_eq!(result, vec!["/dir/b/b.txt"]);
  }

  #[test]
  fn should_get_if_could_be_literal_path() {
    assert!(could_be_literal_path("routes/[id].svelte"));
    assert!(could_be_literal_path("{{myfile}}.yaml"));
    assert!(!could_be_literal_path("**/*.txt"));
    assert!(!could_be_literal_path("file?.txt"));
    // an escaped glob character names a literal path
    assert!(could_be_literal_path("a\\*b"));
    // ...but an escaped backslash doesn't escape the star after it
    assert!(!could_be_literal_path("a\\\\*b"));
  }
}
