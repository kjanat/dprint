use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use kprint_git::PatternList;
use kprint_git::PatternMatch;

use crate::environment::DirEntry;
use crate::environment::DiscoveryEnvironment as Environment;
use crate::utils::DirPrefix;
use crate::utils::escape_glob_text;
use crate::utils::path_to_slash_bytes;

/// Resolved gitignore for a directory.
pub struct DirGitIgnores {
  current: Option<DirPatterns>,
  parent: Option<Arc<DirGitIgnores>>,
}

/// Gitignore patterns for the entries of `dir`.
struct DirPatterns {
  dir: DirPrefix,
  patterns: PatternList,
}

impl DirPatterns {
  fn matched(&self, path: &Path, is_dir: bool) -> PatternMatch {
    match self.dir.strip(path) {
      Some(relative) if !relative.as_os_str().is_empty() => self.patterns.matched(&path_to_slash_bytes(relative), is_dir),
      _ => PatternMatch::None,
    }
  }
}

impl DirGitIgnores {
  pub fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
    let mut is_ignored = false;
    if let Some(parent) = &self.parent {
      is_ignored = parent.is_ignored(path, is_dir);
    }
    if let Some(current) = &self.current {
      match current.matched(path, is_dir) {
        PatternMatch::None => {}
        PatternMatch::Positive => is_ignored = true,
        PatternMatch::Negative => is_ignored = false,
      }
    }
    is_ignored
  }

  /// Resolves the gitignores for a directory's entries from its parent's
  /// gitignores and its listing. dprint doesn't read a file absent from the
  /// listing.
  pub fn for_listed_dir(
    environment: &impl Environment,
    dir_path: &Path,
    hint: DirEntriesHint,
    parent: Option<&Arc<DirGitIgnores>>,
    options: &GitIgnoreTreeOptions,
  ) -> Option<Arc<DirGitIgnores>> {
    // a directory containing `.git` is a repository root, so gitignores above
    // it don't apply
    let parent = if hint.has_git { None } else { parent.cloned() };
    let current = resolve_current_gitignore(environment, dir_path, hint.has_git, Some(hint), options);
    Self::chain(current, parent)
  }

  /// Adds a directory's gitignore to its parent's. A directory without one
  /// shares its parent's, so checking a path only visits directories with a
  /// gitignore.
  fn chain(current: Option<DirPatterns>, parent: Option<Arc<DirGitIgnores>>) -> Option<Arc<DirGitIgnores>> {
    match current {
      Some(current) => Some(Arc::new(DirGitIgnores {
        current: Some(current),
        parent,
      })),
      None => parent,
    }
  }
}

/// Resolves Git's global excludes file unless explicitly disabled.
/// Returns an empty list when disabled or when no global excludes file exists.
pub fn resolve_global_gitignore_lines(environment: &impl Environment) -> Vec<String> {
  if !global_gitignore_enabled(environment) {
    return Vec::new();
  }
  let Some(path) = environment.global_gitignore_path() else {
    return Vec::new();
  };
  match environment.maybe_read_file(&path) {
    Ok(Some(text)) => text.lines().map(|line| line.to_string()).collect(),
    Ok(None) => Vec::new(), // no global excludes file present
    Err(err) => {
      log_debug!(environment, "Failed reading global gitignore '{}': {:#}", path.display(), err);
      Vec::new()
    }
  }
}

fn global_gitignore_enabled(environment: &impl Environment) -> bool {
  match environment.env_var("DPRINT_GLOBAL_GITIGNORE") {
    Some(value) => {
      let value = value.to_string_lossy();
      let value = value.trim();
      value == "1" || value.eq_ignore_ascii_case("true")
    }
    None => true,
  }
}

/// Whether `.gitignore` and `.git` are present in an already listed directory.
/// With this, resolution skips file system calls for absent files.
#[derive(Clone, Copy, Default)]
pub struct DirEntriesHint {
  pub has_gitignore: bool,
  pub has_git: bool,
}

impl DirEntriesHint {
  /// Derives a hint from an already-read directory listing.
  pub fn from_dir_entries(entries: &[DirEntry]) -> Self {
    let mut hint = DirEntriesHint {
      has_git: false,
      has_gitignore: false,
    };
    for entry in entries {
      let name = match entry {
        // `.gitignore` is a file and `.git` is usually a directory (a file in worktrees)
        DirEntry::Directory(path) => path.file_name().and_then(|f| f.to_str()),
        DirEntry::File { name, .. } => name.to_str(),
      };
      match name {
        Some(".gitignore") => hint.has_gitignore = true,
        Some(".git") => hint.has_git = true,
        _ => continue,
      }
      if hint.has_gitignore && hint.has_git {
        break; // nothing left to learn
      }
    }
    hint
  }
}

#[derive(Default, Clone)]
pub struct GitIgnoreTreeOptions {
  /// Paths exempt from the gitignore.
  pub include_paths: Vec<PathBuf>,
  /// Lines from git's global excludes file, applied at the repository root with
  /// the lowest precedence. Empty when global gitignore support is disabled.
  pub global_gitignore_lines: Vec<String>,
}

/// Resolves gitignores in a directory tree, including ancestor gitignores.
pub struct GitIgnoreTree<TEnvironment> {
  environment: TEnvironment,
  ignores: HashMap<PathBuf, Option<Arc<DirGitIgnores>>>,
  options: GitIgnoreTreeOptions,
}

impl<TEnvironment: Environment> GitIgnoreTree<TEnvironment> {
  pub fn new(environment: TEnvironment, options: GitIgnoreTreeOptions) -> Self {
    Self {
      environment,
      ignores: Default::default(),
      options,
    }
  }

  /// Resolves the gitignore for the children of an already listed directory.
  /// The hint lets resolution avoid redundant reads.
  pub fn get_resolved_git_ignore_for_dir_children(&mut self, dir_path: &Path, hint: DirEntriesHint) -> Option<Arc<DirGitIgnores>> {
    self.get_resolved_git_ignore_inner(dir_path, Some(hint))
  }

  pub fn get_resolved_git_ignore_for_file(&mut self, file_path: &Path) -> Option<Arc<DirGitIgnores>> {
    let dir_path = file_path.parent()?;
    self.get_resolved_git_ignore_inner(dir_path, None)
  }

  fn get_resolved_git_ignore_inner(&mut self, dir_path: &Path, hint: Option<DirEntriesHint>) -> Option<Arc<DirGitIgnores>> {
    let maybe_resolved = self.ignores.get(dir_path).cloned();
    if let Some(resolved) = maybe_resolved {
      resolved
    } else {
      let resolved = self.resolve_gitignore_in_dir(dir_path, hint);
      self.ignores.insert(dir_path.to_owned(), resolved.clone());
      resolved
    }
  }

  fn resolve_gitignore_in_dir(&mut self, dir_path: &Path, hint: Option<DirEntriesHint>) -> Option<Arc<DirGitIgnores>> {
    // a directory containing `.git` is the root of a repository, so don't
    // search for gitignores above it
    let is_repo_root = match hint {
      Some(hint) => hint.has_git,
      None => self.environment.path_exists(dir_path.join(".git")),
    };
    let parent = if is_repo_root {
      None
    } else {
      // ancestors aren't part of the caller's listing, so resolve them without a hint
      dir_path.parent().and_then(|parent| self.get_resolved_git_ignore_inner(parent, None))
    };
    let current = resolve_current_gitignore(&self.environment, dir_path, is_repo_root, hint, &self.options);
    DirGitIgnores::chain(current, parent)
  }
}

fn resolve_current_gitignore(
  environment: &impl Environment,
  dir_path: &Path,
  is_repo_root: bool,
  hint: Option<DirEntriesHint>,
  options: &GitIgnoreTreeOptions,
) -> Option<DirPatterns> {
  // skip the read when the caller's listing already shows there's no `.gitignore`
  let maybe_has_gitignore = hint.map(|h| h.has_gitignore).unwrap_or(true);
  let gitignore_bytes = if maybe_has_gitignore {
    environment.read_file_bytes(dir_path.join(".gitignore")).ok()
  } else {
    None
  };
  // git reads `.git/info/exclude` and the global excludes file only at the
  // repository root (https://git-scm.com/docs/gitignore)
  let exclude_bytes = if is_repo_root {
    environment.read_file_bytes(dir_path.join(".git").join("info").join("exclude")).ok()
  } else {
    None
  };
  let global_lines: &[String] = if is_repo_root { options.global_gitignore_lines.as_slice() } else { &[] };
  if gitignore_bytes.is_none() && exclude_bytes.is_none() && global_lines.is_empty() {
    return None;
  }

  // git's precedence is global excludes < `.git/info/exclude` < `.gitignore`,
  // and the last matching pattern wins, so add them in that order
  let mut patterns = PatternList::new(/* ignore case */ false);
  for line in global_lines {
    patterns.add_line(line.as_bytes());
  }
  if let Some(bytes) = &exclude_bytes {
    patterns.add_buffer(bytes);
  }
  if let Some(bytes) = &gitignore_bytes {
    patterns.add_buffer(bytes);
  }
  // override the gitignore contents to include these paths (escaping so a
  // path with glob characters in its name matches literally)
  for path in &options.include_paths {
    if let Ok(suffix) = path.strip_prefix(dir_path) {
      let suffix = escape_glob_text(&suffix.to_string_lossy().replace('\\', "/"));
      patterns.add_line(format!("!/{}", suffix).as_bytes());
      if !suffix.ends_with('/') {
        patterns.add_line(format!("!/{}/", suffix).as_bytes());
      }
    }
  }
  Some(DirPatterns {
    dir: DirPrefix::new(dir_path.to_path_buf()),
    patterns,
  })
}

#[cfg(test)]
mod test {
  use crate::environment::TestEnvironment;
  use kprint_platform::environment::*;

  use super::*;

  #[test]
  fn git_ignore_tree() {
    let env = TestEnvironment::new();
    env.write_file("/.gitignore", "file.txt").unwrap();
    env.mk_dir_all("/sub_dir/sub_dir").unwrap();
    env.write_file("/sub_dir/.gitignore", "data.txt").unwrap();
    env.write_file("/sub_dir/sub_dir/.gitignore", "!file.txt\nignore.txt").unwrap();
    let mut ignore_tree = GitIgnoreTree::new(env, GitIgnoreTreeOptions::default());
    let mut run_test = |path: &str, expected: bool| {
      let path = PathBuf::from(path);
      let gitignore = ignore_tree.get_resolved_git_ignore_for_file(&path).unwrap();
      assert_eq!(gitignore.is_ignored(&path, /* is_dir */ false), expected, "Path: {}", path.display());
    };
    run_test("/file.txt", true);
    run_test("/other.txt", false);
    run_test("/data.txt", false);
    run_test("/sub_dir/file.txt", true);
    run_test("/sub_dir/other.txt", false);
    run_test("/sub_dir/data.txt", true);
    run_test("/sub_dir/sub_dir/file.txt", false); // unignored up here
    run_test("/sub_dir/sub_dir/sub_dir/file.txt", false);
    run_test("/sub_dir/sub_dir/sub_dir/ignore.txt", true);
    run_test("/sub_dir/sub_dir/ignore.txt", true);
    run_test("/sub_dir/ignore.txt", false);
    run_test("/ignore.txt", false);
  }

  #[test]
  fn honours_git_info_exclude() {
    let env = TestEnvironment::new();
    env.write_file("/.gitignore", "from_gitignore.txt").unwrap();
    env.mk_dir_all("/.git/info").unwrap();
    env.write_file("/.git/info/exclude", "from_exclude.txt\n!unexclude.txt").unwrap();
    env.mk_dir_all("/sub_dir").unwrap();
    let mut ignore_tree = GitIgnoreTree::new(env, GitIgnoreTreeOptions::default());
    let mut run_test = |path: &str, expected: bool| {
      let path = PathBuf::from(path);
      let gitignore = ignore_tree.get_resolved_git_ignore_for_file(&path).unwrap();
      assert_eq!(gitignore.is_ignored(&path, /* is_dir */ false), expected, "Path: {}", path.display());
    };
    run_test("/from_gitignore.txt", true);
    run_test("/from_exclude.txt", true);
    // patterns in `.git/info/exclude` apply to descendant directories too
    run_test("/sub_dir/from_exclude.txt", true);
    run_test("/other.txt", false);
  }

  #[test]
  fn global_gitignore_is_lowest_precedence() {
    let env = TestEnvironment::new();
    // a `.git` dir makes `/` the repo root, where the global excludes apply
    env.mk_dir_all("/.git").unwrap();
    env.write_file("/.git/HEAD", "").unwrap();
    env.write_file("/.gitignore", "from_gitignore.txt\n!from_global.txt").unwrap();
    env.mk_dir_all("/sub").unwrap();
    let global_gitignore_lines = vec!["from_global.txt".to_string(), "*.log".to_string()];
    let mut ignore_tree = GitIgnoreTree::new(
      env,
      GitIgnoreTreeOptions {
        global_gitignore_lines,
        ..Default::default()
      },
    );
    let mut run_test = |path: &str, expected: bool| {
      let path = PathBuf::from(path);
      let gitignore = ignore_tree.get_resolved_git_ignore_for_file(&path).unwrap();
      assert_eq!(gitignore.is_ignored(&path, /* is_dir */ false), expected, "Path: {}", path.display());
    };
    // ignored by the global excludes file
    run_test("/from_global.txt", false); // re-included by the closer `.gitignore`
    run_test("/debug.log", true); // global pattern, applies to descendants too
    run_test("/sub/debug.log", true);
    // ignored by the repo `.gitignore`
    run_test("/from_gitignore.txt", true);
    run_test("/other.txt", false);
  }

  #[test]
  fn git_info_exclude_without_gitignore() {
    // a repo with only `.git/info/exclude` and no `.gitignore` should still be honoured
    let env = TestEnvironment::new();
    env.mk_dir_all("/.git/info").unwrap();
    env.write_file("/.git/info/exclude", "ignored.txt").unwrap();
    let mut ignore_tree = GitIgnoreTree::new(env, GitIgnoreTreeOptions::default());
    let path = PathBuf::from("/ignored.txt");
    let gitignore = ignore_tree.get_resolved_git_ignore_for_file(&path).unwrap();
    assert!(gitignore.is_ignored(&path, /* is_dir */ false));
  }
}
