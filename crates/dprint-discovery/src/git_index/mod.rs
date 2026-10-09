//! Finds the files to format with git's index instead of a walk.
//!
//! The index lists the tracked files. Its untracked cache lists the untracked
//! files of each directory. Git wrote both together with a fsmonitor token.
//! The fsmonitor daemon reports every changed path since that token, and
//! dprint lists those directories again, like `git status`.
//!
//! dprint applies its matcher, gitignore and config file checks to the result,
//! like a walk. On anything unexpected, dprint walks instead.

use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;

use crate::POSSIBLE_CONFIG_FILE_NAMES;
use crate::environment::DirEntry;
use crate::environment::DiscoveryEnvironment as Environment;
use crate::environment::PathKind;
use crate::gitignore::DirEntriesHint;
use crate::gitignore::DirGitIgnores;
use crate::glob::DirScanGitIgnore;
use crate::glob::DirScanOptions;
use crate::glob::DirScanOutput;
use crate::glob::ExcludeMatchDetail;
use crate::glob::GlobMatchesDetail;
use crate::glob::walk_dir;
use crate::repo_index::RepoIndex;

use dprint_git::DIR_HIDE_EMPTY_DIRECTORIES;
use dprint_git::DIR_SHOW_OTHER_DIRECTORIES;
use dprint_git::EntryKind;
use dprint_git::IndexEntry;
use dprint_git::UntrackedCache;
use dprint_git::exclude_file_oid;

/// Git's `fsmonitor_ipc__get_default_path`.
const FSMONITOR_SOCKET_FILE_NAME: &str = "fsmonitor--daemon.ipc";

/// `None` means dprint has to walk the directory.
pub(crate) fn scan_with_git_index<TEnvironment: Environment>(environment: &TEnvironment, options: &DirScanOptions) -> Result<Option<DirScanOutput>> {
  let Some(gitignore) = &options.gitignore else {
    return Ok(None);
  };
  let snapshot = match Snapshot::load(environment, options, gitignore) {
    Ok(snapshot) => snapshot,
    Err(err) => {
      log_debug!(environment, "Not using the git index for {}: {:#}", options.start_dir.display(), err);
      return Ok(None);
    }
  };
  let mut discovery = Discovery::new(environment, options, gitignore, &snapshot);
  match discovery.collect() {
    Ok(()) => {}
    Err(err) => {
      log_debug!(environment, "Not using the git index for {}: {:#}", options.start_dir.display(), err);
      return Ok(None);
    }
  }
  discovery.into_output().map(Some)
}

/// The index, with the daemon's changes applied.
struct Snapshot {
  index: Arc<RepoIndex>,
  /// Sorted positions in the index.
  dirty_entries: Vec<usize>,
  untracked_cache: UntrackedCache,
  /// Sorted paths the daemon reported, without a trailing `/`.
  changed_paths: Vec<Vec<u8>>,
}

impl Snapshot {
  fn load(environment: &impl Environment, options: &DirScanOptions, gitignore: &DirScanGitIgnore) -> Result<Self> {
    if !gitignore.options.include_paths.is_empty() {
      bail!("paths override the gitignore");
    }
    if options.matcher.has_opted_out_excludes() {
      bail!("excludes opt paths out of the gitignore");
    }
    if gitignore.start_dir_gitignored {
      bail!("the start directory is gitignored");
    }
    for name in ["GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR", "GIT_INDEX_FILE", "GIT_DISABLE_UNTRACKED_CACHE"] {
      if environment.env_var(name).is_some() {
        bail!("{} is set", name);
      }
    }
    let index = match &gitignore.index {
      Some(index) => index.clone(),
      None => Arc::new(RepoIndex::read(environment, &options.start_dir)?),
    };
    let repo = &index.repo;
    let fsmonitor = index.fsmonitor.as_ref().ok_or_else(|| anyhow!("the index has no fsmonitor token"))?;
    let mut untracked_cache = index.untracked_cache.clone().ok_or_else(|| anyhow!("the index has no untracked cache"))?;
    if untracked_cache.dirs.is_empty() {
      bail!("the untracked cache is empty");
    }
    let ident = format!("Location {}, system {}", repo.work_tree.display(), system_name()?);
    if untracked_cache.ident != ident.as_bytes() {
      bail!("the untracked cache is for {:?}", String::from_utf8_lossy(&untracked_cache.ident));
    }
    if untracked_cache.dir_flags != 0 && untracked_cache.dir_flags != DIR_SHOW_OTHER_DIRECTORIES | DIR_HIDE_EMPTY_DIRECTORIES {
      bail!("the untracked cache has flags {:#x}", untracked_cache.dir_flags);
    }
    if untracked_cache.exclude_per_dir != b".gitignore" {
      bail!("the untracked cache uses {:?} files", String::from_utf8_lossy(&untracked_cache.exclude_per_dir));
    }
    let info_exclude = maybe_read_file_bytes(environment, repo.common_dir.join("info").join("exclude"))?;
    if exclude_file_oid(info_exclude.as_deref(), index.hash_len) != untracked_cache.info_exclude_oid {
      bail!(".git/info/exclude differs from the untracked cache");
    }
    let excludes_file = match environment.global_gitignore_path() {
      Some(path) => maybe_read_file_bytes(environment, path)?,
      None => None,
    };
    if exclude_file_oid(excludes_file.as_deref(), index.hash_len) != untracked_cache.excludes_file_oid {
      bail!("the global excludes file differs from the untracked cache");
    }
    if excludes_file.is_some_and(|bytes| !bytes.is_empty()) && gitignore.options.global_gitignore_lines.is_empty() {
      bail!("git uses a global excludes file and dprint doesn't");
    }

    let response = environment
      .git_ipc_request(&repo.git_dir.join(FSMONITOR_SOCKET_FILE_NAME), &fsmonitor.token)
      .context("asking the fsmonitor daemon for changes")?;
    let Some((_token, paths)) = response.split_first_nul() else {
      bail!("the fsmonitor daemon sent no token");
    };
    if paths.starts_with(b"/") {
      bail!("the fsmonitor daemon has no history for the index's token");
    }
    let mut changed_paths = Vec::new();
    for path in paths.split(|byte| *byte == 0).filter(|path| !path.is_empty()) {
      untracked_cache.invalidate_path(path);
      let path = path.strip_suffix(b"/").unwrap_or(path);
      if path.rsplit(|byte| *byte == b'/').next() == Some(b".gitignore")
        && let Some(dir) = untracked_cache.find(parent_of(path))
      {
        untracked_cache.invalidate_tree(dir);
      }
      changed_paths.push(path.to_vec());
    }
    changed_paths.sort_unstable();
    changed_paths.dedup();
    let mut dirty_entries = fsmonitor.dirty_entries.clone();
    dirty_entries.sort_unstable();

    Ok(Snapshot {
      index,
      dirty_entries,
      untracked_cache,
      changed_paths,
    })
  }

  fn work_tree(&self) -> &Path {
    &self.index.repo.work_tree
  }

  /// Whether the daemon reported the path or one of its directories.
  fn is_changed(&self, path: &[u8]) -> bool {
    let is_reported = |path: &[u8]| self.changed_paths.binary_search_by(|changed| changed.as_slice().cmp(path)).is_ok();
    !self.changed_paths.is_empty() && (is_reported(path) || path.iter().enumerate().any(|(index, byte)| *byte == b'/' && is_reported(&path[..index])))
  }

  fn is_dirty(&self, position: usize) -> bool {
    self.dirty_entries.binary_search(&position).is_ok()
  }
}

trait SplitFirstNul {
  fn split_first_nul(&self) -> Option<(&[u8], &[u8])>;
}

impl SplitFirstNul for Vec<u8> {
  fn split_first_nul(&self) -> Option<(&[u8], &[u8])> {
    let index = self.iter().position(|byte| *byte == 0)?;
    Some((&self[..index], &self[index + 1..]))
  }
}

/// Git puts `uname`'s `sysname` into the untracked cache's ident.
fn system_name() -> Result<&'static str> {
  Ok(match std::env::consts::OS {
    "linux" | "android" => "Linux",
    "macos" => "Darwin",
    "freebsd" => "FreeBSD",
    "netbsd" => "NetBSD",
    "openbsd" => "OpenBSD",
    "dragonfly" => "DragonFly",
    "solaris" | "illumos" => "SunOS",
    os => bail!("unknown system name for {}", os),
  })
}

fn maybe_read_file_bytes(environment: &impl Environment, path: impl AsRef<Path>) -> Result<Option<Vec<u8>>> {
  match environment.read_file_bytes(path) {
    Ok(bytes) => Ok(Some(bytes)),
    Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
    Err(err) => Err(err.into()),
  }
}

fn parent_of(path: &[u8]) -> &[u8] {
  match path.iter().rposition(|byte| *byte == b'/') {
    Some(index) => &path[..index],
    None => b"",
  }
}

fn file_name_of(path: &[u8]) -> &[u8] {
  match path.iter().rposition(|byte| *byte == b'/') {
    Some(index) => &path[index + 1..],
    None => path,
  }
}

fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
  if dir.is_empty() {
    return name.to_vec();
  }
  let mut path = Vec::with_capacity(dir.len() + 1 + name.len());
  path.extend_from_slice(dir);
  path.push(b'/');
  path.extend_from_slice(name);
  path
}

/// Whether a file decides a directory's config scope or gitignore.
fn is_special_file(path: &[u8]) -> bool {
  let name = file_name_of(path);
  name == b".gitignore" || POSSIBLE_CONFIG_FILE_NAMES.iter().any(|file_name| file_name.as_bytes() == name)
}

#[derive(Clone)]
struct DirState {
  excluded: bool,
  /// The directory is gitignored. Only its tracked entries count.
  gitignored: bool,
  gitignore: Option<Arc<DirGitIgnores>>,
}

impl DirState {
  const EXCLUDED: DirState = DirState {
    excluded: true,
    gitignored: false,
    gitignore: None,
  };
}

struct Discovery<'a, TEnvironment: Environment> {
  environment: &'a TEnvironment,
  options: &'a DirScanOptions,
  gitignore: &'a DirScanGitIgnore,
  snapshot: &'a Snapshot,
  start: Vec<u8>,
  /// Tracked files under the start directory, relative to the work tree, in
  /// index order.
  tracked_files: Vec<&'a [u8]>,
  /// Untracked files under the start directory, relative to the work tree.
  untracked_files: Vec<Vec<u8>>,
  /// Directories for a walk, relative to the work tree. These are untracked
  /// directories, unlisted directories and nested repositories.
  walk_roots: Vec<Vec<u8>>,
}

impl<'a, TEnvironment: Environment> Discovery<'a, TEnvironment> {
  fn new(environment: &'a TEnvironment, options: &'a DirScanOptions, gitignore: &'a DirScanGitIgnore, snapshot: &'a Snapshot) -> Self {
    let start = options
      .start_dir
      .strip_prefix(snapshot.work_tree())
      .map(|path| path.as_os_str().as_bytes().to_vec())
      .unwrap_or_default();
    Self {
      environment,
      options,
      gitignore,
      snapshot,
      start,
      tracked_files: Vec::new(),
      untracked_files: Vec::new(),
      walk_roots: Vec::new(),
    }
  }

  fn path(&self, relative: &[u8]) -> PathBuf {
    work_tree_path(self.snapshot.work_tree(), relative)
  }

  fn is_below_start(&self, path: &[u8]) -> bool {
    self.start.is_empty() || (path.len() > self.start.len() && path.starts_with(&self.start) && path[self.start.len()] == b'/')
  }

  fn collect(&mut self) -> Result<()> {
    self.collect_tracked();
    self.collect_untracked()
  }

  fn collect_tracked(&mut self) {
    let snapshot = self.snapshot;
    let entries = &snapshot.index.entries;
    self.tracked_files.reserve(entries.len());
    for (position, entry) in entries.iter().enumerate() {
      let is_conflicted = |other: Option<&IndexEntry>| other.is_some_and(|other| other.path == entry.path);
      if is_conflicted(position.checked_sub(1).and_then(|previous| entries.get(previous))) {
        continue;
      }
      if entry.skip_worktree || !self.is_below_start(&entry.path) {
        continue;
      }
      if entry.kind == EntryKind::Gitlink {
        self.walk_roots.push(entry.path.clone());
        continue;
      }
      let needs_check = is_conflicted(entries.get(position + 1)) || snapshot.is_dirty(position) || snapshot.is_changed(&entry.path);
      let is_file = if needs_check {
        self.environment.path_kind(self.path(&entry.path)) == Some(PathKind::File)
      } else {
        entry.kind == EntryKind::File
      };
      if is_file {
        self.tracked_files.push(&entry.path);
      }
    }
  }

  fn collect_untracked(&mut self) -> Result<()> {
    let cache = &self.snapshot.untracked_cache;
    let start_dir = cache.find(&self.start).ok_or_else(|| anyhow!("git hasn't listed the start directory"))?;
    let mut cached_files = Vec::new();
    let mut pending = vec![(start_dir, self.start.clone())];
    while let Some((dir, path)) = pending.pop() {
      let cached = &cache.dirs[dir];
      if cached.check_only {
        if path == self.start {
          bail!("the start directory is untracked");
        }
        self.walk_roots.push(path);
      } else if cached.valid {
        let mut untracked_dirs = HashSet::new();
        for name in &cached.untracked {
          match name.strip_suffix(b"/") {
            Some(dir_name) => {
              untracked_dirs.insert(dir_name);
              self.walk_roots.push(join(&path, dir_name));
            }
            None => cached_files.push(join(&path, name)),
          }
        }
        for subdir in &cached.subdirs {
          let name = &cache.dirs[*subdir].name;
          if !untracked_dirs.contains(name.as_slice()) {
            pending.push((*subdir, join(&path, name)));
          }
        }
      } else {
        let Ok(entries) = self.environment.dir_info(self.path(&path)) else {
          continue;
        };
        for entry in entries {
          match entry {
            DirEntry::Directory(dir_path) => {
              let Some(name) = dir_path.file_name() else {
                continue;
              };
              let name = name.as_bytes();
              if name == b".git" {
                continue;
              }
              let subdir_path = join(&path, name);
              match cache.child(dir, name) {
                Some(subdir) if self.environment.path_kind(dir_path.join(".git")).is_none() => pending.push((subdir, subdir_path)),
                _ => self.walk_roots.push(subdir_path),
              }
            }
            DirEntry::File { name, .. } => {
              let file_path = join(&path, name.as_bytes());
              if !self.snapshot.index.has_entry(&file_path) {
                self.untracked_files.push(file_path);
              }
            }
          }
        }
      }
    }
    for file_path in cached_files {
      if !self.snapshot.index.has_entry(&file_path) && self.environment.path_kind(self.path(&file_path)) == Some(PathKind::File) {
        self.untracked_files.push(file_path);
      }
    }
    Ok(())
  }

  fn into_output(self) -> Result<DirScanOutput> {
    let candidates = self
      .tracked_files
      .iter()
      .map(|path| (*path, true))
      .chain(self.untracked_files.iter().map(|path| (path.as_slice(), false)))
      .collect::<Vec<_>>();
    let mut filter = Filter {
      environment: self.environment,
      options: self.options,
      gitignore: self.gitignore,
      snapshot: self.snapshot,
      start: &self.start,
      special_files: candidates.iter().map(|(path, _)| *path).filter(|path| is_special_file(path)).collect(),
      states: HashMap::new(),
      config_files: Vec::new(),
    };
    let mut output = DirScanOutput {
      file_paths: Vec::with_capacity(candidates.len()),
      config_files: Vec::new(),
    };
    let mut matched = Vec::with_capacity(candidates.len());
    let mut current_dir = None;
    let mut state = DirState::EXCLUDED;
    for (file_path, is_tracked) in candidates {
      let dir = parent_of(file_path);
      if current_dir != Some(dir) {
        state = filter.dir_state(dir);
        current_dir = Some(dir);
      }
      if let Some(path) = filter.matched_file(file_path, &state, is_tracked) {
        output.file_paths.push(path);
        matched.push(file_path);
      }
    }
    let index_count = output.file_paths.len();
    let mut walked = HashSet::new();
    let mut found = None;
    for root in &self.walk_roots {
      if walked.insert(root.as_slice())
        && let Some(walk_options) = filter.walk_options(root)
      {
        let result = walk_dir(self.environment, walk_options)?;
        let found = found.get_or_insert_with(|| matched.iter().map(|path| path.to_vec()).collect::<HashSet<_>>());
        for path in result.file_paths {
          let is_new = match self.snapshot.index.relative(&path) {
            Some(relative) => found.insert(relative.into_owned()),
            None => true,
          };
          if is_new {
            output.file_paths.push(path);
          }
        }
        filter.config_files.extend(result.config_files);
      }
    }
    for config_file in filter.config_files {
      if !output.config_files.contains(&config_file) {
        output.config_files.push(config_file);
      }
    }
    log_debug!(
      self.environment,
      "Read {} files from the git index for {} and walked {} directories for {} more",
      index_count,
      self.options.start_dir.display(),
      walked.len(),
      output.file_paths.len() - index_count,
    );
    Ok(output)
  }
}

fn work_tree_path(work_tree: &Path, relative: &[u8]) -> PathBuf {
  let mut path = PathBuf::with_capacity(work_tree.as_os_str().len() + 1 + relative.len());
  path.push(work_tree);
  if !relative.is_empty() {
    path.push(OsStr::from_bytes(relative));
  }
  path
}

/// Checks directories and files like a walk does.
struct Filter<'a, TEnvironment: Environment> {
  environment: &'a TEnvironment,
  options: &'a DirScanOptions,
  gitignore: &'a DirScanGitIgnore,
  snapshot: &'a Snapshot,
  start: &'a [u8],
  /// The found `.gitignore` and config files.
  special_files: HashSet<&'a [u8]>,
  states: HashMap<Vec<u8>, DirState>,
  config_files: Vec<PathBuf>,
}

impl<TEnvironment: Environment> Filter<'_, TEnvironment> {
  fn path(&self, relative: &[u8]) -> PathBuf {
    work_tree_path(self.snapshot.work_tree(), relative)
  }

  /// `state` is the state of the file's directory.
  fn matched_file(&self, file_path: &[u8], state: &DirState, is_tracked: bool) -> Option<PathBuf> {
    if state.excluded {
      return None;
    }
    let path = self.path(file_path);
    match self.options.matcher.matches_detail_with_shebang_checked(&path, /* has matching shebang */ true) {
      GlobMatchesDetail::Matched if !is_tracked && self.is_gitignored(state, &path) => None,
      GlobMatchesDetail::Matched | GlobMatchesDetail::MatchedOptedOutExclude => Some(path),
      GlobMatchesDetail::Excluded | GlobMatchesDetail::NotMatched => None,
    }
  }

  /// Whether git ignores an untracked file in a directory with `state`.
  fn is_gitignored(&self, state: &DirState, path: &Path) -> bool {
    state.gitignored || state.gitignore.as_ref().is_some_and(|gitignore| gitignore.is_ignored(path, /* is dir */ false))
  }

  /// Whether a directory with `parent` as its parent's state is gitignored,
  /// or `None` when a walk wouldn't descend into it.
  fn child_dir_gitignored(&self, parent: &DirState, dir: &[u8], path: &Path) -> Option<bool> {
    if parent.excluded || file_name_of(dir) == b".git" {
      return None;
    }
    let gitignored = match self.options.matcher.is_dir_ignored(path) {
      ExcludeMatchDetail::Excluded => return None,
      // an explicitly opted out exclude takes precedence over the gitignore
      ExcludeMatchDetail::OptedOutExclude => false,
      ExcludeMatchDetail::NotExcluded => {
        parent.gitignored || parent.gitignore.as_ref().is_some_and(|gitignore| gitignore.is_ignored(path, /* is dir */ true))
      }
    };
    // a gitignored directory only matters for the tracked paths in it
    if gitignored && !self.snapshot.index.contains_dir(path) {
      return None;
    }
    Some(gitignored)
  }

  /// The options for walking a directory, or `None` when a walk wouldn't
  /// descend into it.
  fn walk_options(&mut self, dir: &[u8]) -> Option<DirScanOptions> {
    let path = self.path(dir);
    if self.environment.path_kind(&path) != Some(PathKind::Dir) {
      return None;
    }
    let parent = self.dir_state(parent_of(dir));
    let gitignored = self.child_dir_gitignored(&parent, dir, &path)?;
    let has_git = self.environment.path_kind(path.join(".git")).is_some();
    // the index lists the tracked files of a gitignored directory, and git
    // ignores the rest
    if gitignored && !has_git {
      return None;
    }
    let index = if has_git {
      RepoIndex::load(self.environment, &path)
    } else {
      Some(self.snapshot.index.clone())
    };
    let hint = DirEntriesHint { has_gitignore: true, has_git };
    let gitignore = DirGitIgnores::for_listed_dir(self.environment, &path, hint, parent.gitignore.as_ref(), &self.gitignore.options);
    if self.options.discover_configs
      && let Some(config_file) = POSSIBLE_CONFIG_FILE_NAMES
        .iter()
        .map(|file_name| path.join(file_name))
        .filter(|config_file| Some(config_file) != self.options.current_config_path.as_ref())
        .filter(|config_file| self.environment.path_kind(config_file) == Some(PathKind::File))
        .find(|config_file| {
          index.as_ref().is_some_and(|index| index.contains_file(config_file))
            || !gitignore
              .as_ref()
              .is_some_and(|gitignore| gitignore.is_ignored(config_file, /* is dir */ false))
        })
    {
      self.config_files.push(config_file);
      return None;
    }
    Some(DirScanOptions {
      start_dir: path,
      matcher: self.options.matcher.clone(),
      gitignore: Some(DirScanGitIgnore {
        above_start_dir: parent.gitignore,
        options: self.gitignore.options.clone(),
        index,
        start_dir_gitignored: false,
      }),
      discover_configs: self.options.discover_configs,
      current_config_path: self.options.current_config_path.clone(),
    })
  }

  /// The walk's `DiscoveryPolicy::child_context` for a directory at or below
  /// the start directory. The found files replace a directory listing.
  fn dir_state(&mut self, dir: &[u8]) -> DirState {
    if let Some(state) = self.states.get(dir) {
      return state.clone();
    }
    let path = self.path(dir);
    let state = if dir == self.start {
      DirState {
        excluded: false,
        gitignored: false,
        gitignore: self.listed_dir_gitignore(dir, &path, self.gitignore.above_start_dir.clone()),
      }
    } else {
      let parent = self.dir_state(parent_of(dir));
      match self.child_dir_gitignored(&parent, dir, &path) {
        None => DirState::EXCLUDED,
        Some(gitignored) => {
          let state = DirState {
            excluded: false,
            gitignored,
            gitignore: if gitignored {
              None
            } else {
              self.listed_dir_gitignore(dir, &path, parent.gitignore)
            },
          };
          match self.options.discover_configs.then(|| self.config_file(dir, &state)).flatten() {
            Some(config_file) => {
              self.config_files.push(config_file);
              DirState::EXCLUDED
            }
            None => state,
          }
        }
      }
    };
    self.states.insert(dir.to_vec(), state.clone());
    state
  }

  /// The config file that starts a new scope in `dir`. A gitignored config file
  /// only counts when git tracks it.
  fn config_file(&self, dir: &[u8], state: &DirState) -> Option<PathBuf> {
    POSSIBLE_CONFIG_FILE_NAMES
      .iter()
      .filter(|file_name| self.special_files.contains(join(dir, file_name.as_bytes()).as_slice()))
      .map(|file_name| self.path(&join(dir, file_name.as_bytes())))
      .filter(|config_file| Some(config_file) != self.options.current_config_path.as_ref())
      .find(|config_file| self.snapshot.index.contains_file(config_file) || !self.is_gitignored(state, config_file))
  }

  fn listed_dir_gitignore(&self, dir: &[u8], path: &Path, parent: Option<Arc<DirGitIgnores>>) -> Option<Arc<DirGitIgnores>> {
    let cache = &self.snapshot.untracked_cache;
    let hint = DirEntriesHint {
      has_gitignore: self.special_files.contains(join(dir, b".gitignore").as_slice())
        || cache.find(dir).is_some_and(|cached| cache.dirs[cached].has_exclude_file),
      has_git: dir.is_empty(),
    };
    DirGitIgnores::for_listed_dir(self.environment, path, hint, parent.as_ref(), &self.gitignore.options)
  }
}

#[cfg(test)]
mod test {
  use std::path::Path;
  use std::path::PathBuf;
  use std::sync::Arc;

  use dprint_git::test_util::*;
  use dprint_platform::environment::*;
  use pretty_assertions::assert_eq;

  use super::*;
  use crate::GlobMatcher;
  use crate::GlobMatcherOptions;
  use crate::GlobPattern;
  use crate::GlobPatterns;
  use crate::environment::CanonicalizedPathBuf;
  use crate::environment::TestEnvironment;
  use crate::gitignore::GitIgnoreTree;
  use crate::gitignore::GitIgnoreTreeOptions;
  use dprint_host_api::ui::LogLevel;

  const TOKEN: &str = "builtin:test:1";
  const SOCKET: &str = "/repo/.git/fsmonitor--daemon.ipc";

  struct Fixture {
    environment: TestEnvironment,
    entries: Vec<TestEntry>,
    dirty_entries: Vec<usize>,
    root: TestDir,
    ident: String,
    extra_extensions: Vec<(&'static [u8; 4], Vec<u8>)>,
    has_fsmonitor: bool,
    has_daemon: bool,
  }

  impl Fixture {
    /// A repository after `git status` with the untracked cache on. `new.ts` and
    /// `newdir/` are untracked, `x.log` and `ignored/` are gitignored and `sub`
    /// has its own config file.
    fn new() -> Self {
      let environment = TestEnvironment::new();
      environment.mk_dir_all("/repo/.git").unwrap();
      environment.write_file("/repo/.git/config", "[core]\n\tbare = false\n").unwrap();
      for (path, text) in [
        ("/repo/.gitignore", "ignored/\n*.log\n"),
        ("/repo/a.ts", ""),
        ("/repo/src/b.ts", ""),
        ("/repo/src/c.json", ""),
        ("/repo/sub/dprint.json", "{}"),
        ("/repo/sub/f.ts", ""),
        ("/repo/x.log", ""),
        ("/repo/ignored/e.ts", ""),
        ("/repo/new.ts", ""),
        ("/repo/newdir/d.ts", ""),
      ] {
        write(&environment, path, text);
      }
      let mut newdir = TestDir::valid("newdir", &[], vec![]);
      newdir.check_only = true;
      let mut root = TestDir::valid(
        "",
        &["new.ts", "newdir/"],
        vec![newdir, TestDir::valid("src", &[], vec![]), TestDir::valid("sub", &[], vec![])],
      );
      root.has_exclude_file = true;
      Fixture {
        environment,
        entries: [".gitignore", "a.ts", "src/b.ts", "src/c.json", "sub/dprint.json", "sub/f.ts"]
          .into_iter()
          .map(TestEntry::file)
          .collect(),
        dirty_entries: Vec::new(),
        root,
        ident: "Location /repo, system Linux".to_string(),
        extra_extensions: Vec::new(),
        has_fsmonitor: true,
        has_daemon: true,
      }
    }

    fn write_index(self) -> TestEnvironment {
      let untracked = write_untracked_cache(&TestUntrackedCache {
        ident: self.ident,
        dir_flags: DIR_SHOW_OTHER_DIRECTORIES | DIR_HIDE_EMPTY_DIRECTORIES,
        info_exclude_oid: vec![0; 20],
        excludes_file_oid: vec![0; 20],
        root: Some(self.root),
      });
      let mut extensions = vec![(b"UNTR", untracked)];
      if self.has_fsmonitor {
        extensions.push((b"FSMN", fsmonitor_extension(TOKEN, &self.dirty_entries)));
      }
      extensions.extend(self.extra_extensions);
      let index = write_index(2, 20, &self.entries, &extensions);
      self.environment.write_file_bytes("/repo/.git/index", &index).unwrap();
      if self.has_daemon {
        self.environment.set_git_ipc_response(SOCKET, b"builtin:test:2\0".to_vec());
      }
      self.environment
    }
  }

  fn write(environment: &TestEnvironment, path: &str, text: &str) {
    environment.mk_dir_all(Path::new(path).parent().unwrap()).unwrap();
    environment.write_file(path, text).unwrap();
  }

  fn matcher(includes: &str, excludes: &[&str]) -> Arc<GlobMatcher> {
    let base_dir = CanonicalizedPathBuf::new_for_testing("/repo");
    Arc::new(
      GlobMatcher::new(
        GlobPatterns {
          arg_includes: None,
          config_includes: Some(vec![GlobPattern::new(includes.to_string(), base_dir.clone())]),
          arg_excludes: None,
          config_excludes: excludes.iter().map(|pattern| GlobPattern::new(pattern.to_string(), base_dir.clone())).collect(),
          shebangs: Vec::new(),
        },
        &GlobMatcherOptions {
          case_sensitive: true,
          base_dir,
        },
      )
      .unwrap(),
    )
  }

  fn options(environment: &TestEnvironment, start_dir: &str, matcher: Arc<GlobMatcher>, gitignore_options: GitIgnoreTreeOptions) -> DirScanOptions {
    let start_dir = PathBuf::from(start_dir);
    let index = RepoIndex::load(environment, &start_dir);
    let above_start_dir = if environment.path_exists(start_dir.join(".git")) {
      None
    } else {
      GitIgnoreTree::new(environment.clone(), gitignore_options.clone()).get_resolved_git_ignore_for_file(&start_dir)
    };
    DirScanOptions {
      start_dir,
      matcher,
      gitignore: Some(DirScanGitIgnore {
        above_start_dir,
        index,
        options: gitignore_options,
        start_dir_gitignored: false,
      }),
      discover_configs: true,
      current_config_path: None,
    }
  }

  fn sorted(mut output: DirScanOutput) -> (Vec<PathBuf>, Vec<PathBuf>) {
    output.file_paths.sort();
    output.config_files.sort();
    (output.file_paths, output.config_files)
  }

  /// Scans with the git index, checks the result against a walk of the same
  /// tree, and returns it.
  fn scan_like_walk(environment: &TestEnvironment, start_dir: &str, includes: &str) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let scanned = scan_with_git_index(environment, &options(environment, start_dir, matcher(includes, &[]), Default::default()))
      .unwrap()
      .unwrap_or_else(|| panic!("didn't use the git index: {:?}", environment.take_stderr_messages()));
    let walked = walk_dir(environment, options(environment, start_dir, matcher(includes, &[]), Default::default())).unwrap();
    let scanned = sorted(scanned);
    assert_eq!(scanned, sorted(walked));
    scanned
  }

  fn paths(paths: &[&str]) -> Vec<PathBuf> {
    paths.iter().map(PathBuf::from).collect()
  }

  #[test]
  fn matches_a_walk() {
    let environment = Fixture::new().write_index();
    let (files, configs) = scan_like_walk(&environment, "/repo", "**/*.{ts,json}");
    assert_eq!(
      files,
      paths(&["/repo/a.ts", "/repo/new.ts", "/repo/newdir/d.ts", "/repo/src/b.ts", "/repo/src/c.json"])
    );
    assert_eq!(configs, paths(&["/repo/sub/dprint.json"]));
    assert_eq!(environment.take_git_ipc_requests(), vec![(PathBuf::from(SOCKET), TOKEN.as_bytes().to_vec())]);
  }

  #[test]
  fn relists_changed_directories() {
    let environment = Fixture::new().write_index();
    environment.remove_file("/repo/src/b.ts").unwrap();
    write(&environment, "/repo/src/z.ts", "");
    environment.set_git_ipc_response(SOCKET, b"builtin:test:2\0src/b.ts\0src/z.ts\0".to_vec());
    let (files, _) = scan_like_walk(&environment, "/repo", "**/*.ts");
    assert_eq!(files, paths(&["/repo/a.ts", "/repo/new.ts", "/repo/newdir/d.ts", "/repo/src/z.ts"]));
  }

  #[test]
  fn checks_dirty_tracked_files() {
    let mut fixture = Fixture::new();
    fixture.dirty_entries = vec![1];
    let environment = fixture.write_index();
    environment.remove_file("/repo/a.ts").unwrap();
    let (files, _) = scan_like_walk(&environment, "/repo", "**/*.ts");
    assert_eq!(files, paths(&["/repo/new.ts", "/repo/newdir/d.ts", "/repo/src/b.ts"]));
  }

  #[test]
  fn lists_everything_below_a_changed_gitignore_again() {
    let fixture = Fixture::new();
    write(&fixture.environment, "/repo/src/keep.log", "");
    let environment = fixture.write_index();
    environment.write_file("/repo/.gitignore", "ignored/\n").unwrap();
    environment.set_git_ipc_response(SOCKET, b"builtin:test:2\0.gitignore\0".to_vec());
    let (files, _) = scan_like_walk(&environment, "/repo", "**/*.{ts,log}");
    assert!(files.contains(&PathBuf::from("/repo/src/keep.log")), "{files:?}");
    assert!(files.contains(&PathBuf::from("/repo/x.log")), "{files:?}");
  }

  #[test]
  fn walks_submodules_and_nested_repositories() {
    let mut fixture = Fixture::new();
    fixture.entries.insert(2, TestEntry::with_mode("mod", 0o160000));
    write(&fixture.environment, "/repo/mod/.git", "gitdir: ../.git/modules/mod\n");
    write(&fixture.environment, "/repo/mod/m.ts", "");
    write(&fixture.environment, "/repo/mod/.gitignore", "m.ts\n");
    write(&fixture.environment, "/repo/mod/n.ts", "");
    let environment = fixture.write_index();
    let (files, _) = scan_like_walk(&environment, "/repo", "**/*.ts");
    assert!(files.contains(&PathBuf::from("/repo/mod/n.ts")), "{files:?}");
    assert!(!files.contains(&PathBuf::from("/repo/mod/m.ts")), "{files:?}");
  }

  #[test]
  fn starts_below_the_work_tree() {
    let environment = Fixture::new().write_index();
    let (files, configs) = scan_like_walk(&environment, "/repo/src", "**/*.{ts,json}");
    assert_eq!(files, paths(&["/repo/src/b.ts", "/repo/src/c.json"]));
    assert!(configs.is_empty());
  }

  #[test]
  fn skips_sparse_entries_and_lists_conflicts_once() {
    let mut fixture = Fixture::new();
    for stage in [3, 2, 1] {
      fixture.entries.insert(
        2,
        TestEntry {
          stage,
          ..TestEntry::file("conflict.ts")
        },
      );
    }
    fixture.entries.insert(
      5,
      TestEntry {
        skip_worktree: true,
        ..TestEntry::file("sparse.ts")
      },
    );
    fixture.environment.write_file("/repo/conflict.ts", "").unwrap();
    let environment = fixture.write_index();
    let (files, _) = scan_like_walk(&environment, "/repo", "**/*.ts");
    assert_eq!(files.iter().filter(|path| path.ends_with("conflict.ts")).count(), 1);
    assert!(!files.contains(&PathBuf::from("/repo/sparse.ts")));
  }

  fn assert_walks_instead(environment: &TestEnvironment, options: DirScanOptions, reason: &str) {
    environment.set_log_level(LogLevel::Debug);
    environment.take_stderr_messages();
    assert!(scan_with_git_index(environment, &options).unwrap().is_none());
    let messages = environment.take_stderr_messages();
    assert!(
      messages.iter().any(|message| message.contains("the git index for") && message.contains(reason)),
      "expected {reason:?} in {messages:#?}"
    );
  }

  fn default_options(environment: &TestEnvironment, start_dir: &str) -> DirScanOptions {
    options(environment, start_dir, matcher("**/*.ts", &[]), Default::default())
  }

  #[test]
  fn walks_when_the_daemon_has_no_history_for_the_token() {
    let environment = Fixture::new().write_index();
    environment.set_git_ipc_response(SOCKET, b"builtin:test:2\0/\0".to_vec());
    assert_walks_instead(&environment, default_options(&environment, "/repo"), "no history for the index's token");
  }

  #[test]
  fn walks_when_no_daemon_answers() {
    let mut fixture = Fixture::new();
    fixture.has_daemon = false;
    let environment = fixture.write_index();
    assert_walks_instead(&environment, default_options(&environment, "/repo"), "asking the fsmonitor daemon");
  }

  #[test]
  fn walks_when_the_index_has_no_fsmonitor_token() {
    let mut fixture = Fixture::new();
    fixture.has_fsmonitor = false;
    let environment = fixture.write_index();
    assert_walks_instead(&environment, default_options(&environment, "/repo"), "no fsmonitor token");
  }

  #[test]
  fn walks_when_the_untracked_cache_is_for_another_work_tree() {
    let mut fixture = Fixture::new();
    fixture.ident = "Location /elsewhere, system Linux".to_string();
    let environment = fixture.write_index();
    assert_walks_instead(&environment, default_options(&environment, "/repo"), "the untracked cache is for");
  }

  #[test]
  fn walks_when_info_exclude_changed() {
    let environment = Fixture::new().write_index();
    write(&environment, "/repo/.git/info/exclude", "*.ts\n");
    assert_walks_instead(&environment, default_options(&environment, "/repo"), ".git/info/exclude differs");
  }

  #[test]
  fn walks_when_the_global_excludes_file_changed() {
    let environment = Fixture::new().write_index();
    write(&environment, "/home/.config/git/ignore", "*.ts\n");
    environment.set_global_gitignore_path("/home/.config/git/ignore");
    assert_walks_instead(&environment, default_options(&environment, "/repo"), "global excludes file differs");
  }

  #[test]
  fn walks_on_an_unsupported_index_extension() {
    let mut fixture = Fixture::new();
    fixture.extra_extensions.push((b"link", vec![0; 20]));
    let environment = fixture.write_index();
    assert_walks_instead(&environment, default_options(&environment, "/repo"), "the index uses the link extension");
  }

  #[test]
  fn walks_when_the_repository_ignores_case() {
    let environment = Fixture::new().write_index();
    environment.write_file("/repo/.git/config", "[core]\n\tignorecase = true\n").unwrap();
    assert_walks_instead(&environment, default_options(&environment, "/repo"), "core.ignoreCase");
  }

  #[test]
  fn walks_when_git_is_pointed_elsewhere() {
    let environment = Fixture::new().write_index();
    environment.set_env_var("GIT_DIR", Some("/other/.git"));
    assert_walks_instead(&environment, default_options(&environment, "/repo"), "GIT_DIR is set");
  }

  #[test]
  fn walks_when_excludes_opt_out_of_the_gitignore() {
    let environment = Fixture::new().write_index();
    let options = options(&environment, "/repo", matcher("**/*.ts", &["!ignored/**"]), Default::default());
    assert_walks_instead(&environment, options, "excludes opt paths out");
  }

  #[test]
  fn walks_when_paths_override_the_gitignore() {
    let environment = Fixture::new().write_index();
    let gitignore_options = GitIgnoreTreeOptions {
      include_paths: vec![PathBuf::from("/repo/x.log")],
      ..Default::default()
    };
    let options = options(&environment, "/repo", matcher("**/*.ts", &[]), gitignore_options);
    assert_walks_instead(&environment, options, "paths override the gitignore");
  }

  #[test]
  fn walks_an_untracked_or_unlisted_start_directory() {
    let environment = Fixture::new().write_index();
    assert_walks_instead(&environment, default_options(&environment, "/repo/newdir"), "the start directory is untracked");
    assert_walks_instead(
      &environment,
      default_options(&environment, "/repo/ignored"),
      "git hasn't listed the start directory",
    );
  }

  #[test]
  fn walks_outside_a_repository() {
    let environment = TestEnvironment::new();
    write(&environment, "/plain/a.ts", "");
    assert_walks_instead(&environment, default_options(&environment, "/plain"), "not in a git work tree");
  }

  #[test]
  fn walks_without_gitignore_support() {
    let environment = Fixture::new().write_index();
    let mut options = default_options(&environment, "/repo");
    options.gitignore = None;
    assert!(scan_with_git_index(&environment, &options).unwrap().is_none());
    assert!(environment.take_git_ipc_requests().is_empty());
  }

  #[test]
  fn builds_the_untracked_cache_ident() {
    if cfg!(target_os = "linux") {
      assert_eq!(system_name().unwrap(), "Linux");
    }
    assert_eq!(file_name_of(b"a/b/c"), b"c");
    assert_eq!(parent_of(b"a/b/c"), b"a/b");
    assert_eq!(parent_of(b"c"), b"");
    assert_eq!(join(b"", b"c"), b"c");
    assert_eq!(join(b"a", b"c"), b"a/c");
  }

  type RealEnvironment = crate::environment::RealEnvironment;

  /// A real git repository. A hook replaces the fsmonitor daemon, so git
  /// writes a token into the index.
  struct RealRepo {
    _dir: tempfile::TempDir,
    root: PathBuf,
    environment: RealEnvironment,
  }

  impl RealRepo {
    /// `None` without a git CLI.
    fn new(index_version: &str) -> Option<Self> {
      std::process::Command::new("git").arg("--version").output().ok()?;
      let environment = RealEnvironment::new(crate::environment::HeadlessServices, "test").unwrap();
      let dir = tempfile::tempdir().unwrap();
      let root = environment.canonicalize(dir.path()).unwrap().into_path_buf();
      let repo = RealRepo { _dir: dir, root, environment };
      repo.git(&["init", "-q"]);
      let hook = repo.root.join(".git").join("fsmonitor-hook");
      repo.environment.write_file(&hook, "printf 'builtin:dprint-test:1\\0/'\n").unwrap();
      repo.git(&["config", "core.untrackedCache", "true"]);
      repo.git(&["config", "core.fsmonitor", &format!("sh {}", hook.display())]);
      repo.git(&["config", "core.fsmonitorHookVersion", "2"]);
      repo.git(&["config", "index.version", index_version]);
      Some(repo)
    }

    fn git(&self, args: &[&str]) -> Vec<u8> {
      let output = std::process::Command::new("git").args(args).current_dir(&self.root).output().unwrap();
      assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
      output.stdout
    }

    fn git_succeeds(&self, args: &[&str]) -> bool {
      std::process::Command::new("git")
        .args(args)
        .current_dir(&self.root)
        .output()
        .unwrap()
        .status
        .success()
    }

    fn git_paths(&self, args: &[&str]) -> std::collections::BTreeSet<PathBuf> {
      self
        .git(args)
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| PathBuf::from(OsStr::from_bytes(path)))
        .collect()
    }

    fn write(&self, path: &str, text: &str) {
      assert!(self.try_write(path, text), "writing {path}");
    }

    /// `false` when a file is in the way of the path's directories.
    fn try_write(&self, path: &str, text: &str) -> bool {
      let path = self.root.join(path);
      self.environment.mk_dir_all(path.parent().unwrap()).is_ok() && self.environment.write_file(&path, text).is_ok()
    }

    /// Every existing tracked file and the untracked files git doesn't ignore.
    fn git_files(&self) -> std::collections::BTreeSet<PathBuf> {
      let cached = self.git_paths(&["ls-files", "-z", "--cached"]);
      let deleted = self.git_paths(&["ls-files", "-z", "--deleted"]);
      let others = self.git_paths(&["ls-files", "-z", "--others", "--exclude-standard"]);
      cached.into_iter().filter(|path| !deleted.contains(path)).chain(others).collect()
    }

    fn options(&self, includes: &str, discover_configs: bool) -> DirScanOptions {
      let base_dir = CanonicalizedPathBuf::new_for_testing(&self.root);
      let matcher = GlobMatcher::new(
        GlobPatterns {
          arg_includes: None,
          config_includes: Some(vec![GlobPattern::new(includes.to_string(), base_dir.clone())]),
          arg_excludes: None,
          config_excludes: Vec::new(),
          shebangs: Vec::new(),
        },
        &GlobMatcherOptions {
          case_sensitive: true,
          base_dir,
        },
      )
      .unwrap();
      DirScanOptions {
        start_dir: self.root.clone(),
        matcher: Arc::new(matcher),
        gitignore: Some(DirScanGitIgnore {
          above_start_dir: None,
          index: RepoIndex::load(&self.environment, &self.root),
          start_dir_gitignored: false,
          options: GitIgnoreTreeOptions {
            include_paths: Vec::new(),
            global_gitignore_lines: crate::gitignore::resolve_global_gitignore_lines(&self.environment),
          },
        }),
        discover_configs,
        current_config_path: None,
      }
    }

    /// Scans with the index. A fake daemon answers with `changes`. Panics when
    /// dprint walks instead.
    fn scan_with_index(&self, options: &DirScanOptions, changes: &[u8]) -> DirScanOutput {
      let listener = std::os::unix::net::UnixListener::bind(self.root.join(".git").join(FSMONITOR_SOCKET_FILE_NAME)).unwrap();
      let mut response = b"builtin:dprint-test:2\0".to_vec();
      response.extend_from_slice(changes);
      let daemon = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = dprint_git::read_pkt_line_message(&mut stream).unwrap();
        dprint_git::write_pkt_line_message(&mut stream, &response).unwrap();
        request
      });
      let gitignore = options.gitignore.as_ref().unwrap();
      let snapshot = Snapshot::load(&self.environment, options, gitignore).unwrap_or_else(|err| panic!("{err:#}"));
      assert_eq!(daemon.join().unwrap(), b"builtin:dprint-test:1");
      self.environment.remove_file(self.root.join(".git").join(FSMONITOR_SOCKET_FILE_NAME)).unwrap();
      let mut discovery = Discovery::new(&self.environment, options, gitignore, &snapshot);
      discovery.collect().unwrap();
      discovery.into_output().unwrap()
    }

    fn relative(&self, paths: &[PathBuf]) -> std::collections::BTreeSet<PathBuf> {
      paths.iter().map(|path| path.strip_prefix(&self.root).unwrap().to_path_buf()).collect()
    }
  }

  #[test]
  fn reads_a_real_git_index() {
    for index_version in ["2", "4"] {
      let Some(repo) = RealRepo::new(index_version) else {
        return;
      };
      for (path, text) in [
        (".gitignore", "ignored/\n*.log\n"),
        ("a.ts", ""),
        ("src/b.ts", ""),
        ("src/deep/c.ts", ""),
        ("x.log", ""),
        ("ignored/e.ts", ""),
        ("new.ts", ""),
        ("newdir/d.ts", ""),
        ("sub/dprint.json", "{}"),
        ("sub/f.ts", ""),
      ] {
        repo.write(path, text);
      }
      repo.git(&["add", ".gitignore", "a.ts", "src", "sub"]);
      repo.git(&["status", "--porcelain"]);
      repo.git(&["status", "--porcelain"]);
      repo.environment.remove_file(repo.root.join("src/b.ts")).unwrap();
      repo.write("src/deep/z.ts", "");

      let options = repo.options("**/*.{ts,json}", true);
      let scanned = sorted(repo.scan_with_index(&options, b"src/b.ts\0src/deep/z.ts\0"));
      assert_eq!(
        scanned,
        sorted(walk_dir(&repo.environment, repo.options("**/*.{ts,json}", true)).unwrap()),
        "index version {index_version}"
      );
      assert_eq!(
        repo.relative(&scanned.0).into_iter().collect::<Vec<_>>(),
        paths(&["a.ts", "new.ts", "newdir/d.ts", "src/deep/c.ts", "src/deep/z.ts"]),
        "index version {index_version}"
      );
      assert_eq!(scanned.1, vec![repo.root.join("sub/dprint.json")]);
    }
  }

  #[test]
  fn matches_tracked_ignored_files_and_configs() {
    let Some(repo) = RealRepo::new("2") else {
      return;
    };
    for path in [
      "a.txt",
      "tracked.log",
      "new.log",
      "ignored/kept.txt",
      "ignored/new.txt",
      "ignored/sub/deep.txt",
      "ignored/sub/other.txt",
      "ignored/untracked/x.txt",
      "cfg/b.txt",
      "nocfg/c.txt",
    ] {
      repo.write(path, "");
    }
    repo.write("cfg/dprint.json", "{}");
    repo.write("nocfg/dprint.json", "{}");
    repo.write(".gitignore", "ignored/\n*.log\n/cfg/dprint.json\n/nocfg/dprint.json\n");
    repo.git(&["add", "-f", "cfg/dprint.json", "ignored/kept.txt", "ignored/sub/deep.txt", "tracked.log"]);
    repo.git(&["status", "--porcelain"]);
    repo.git(&["status", "--porcelain"]);
    repo.write("ignored/sub/later.txt", "");

    let options = repo.options("**/*.{txt,log}", true);
    let scanned = sorted(repo.scan_with_index(&options, b"ignored/sub/later.txt\0ignored/sub/\0"));
    assert_eq!(scanned, sorted(walk_dir(&repo.environment, repo.options("**/*.{txt,log}", true)).unwrap()));
    assert_eq!(
      repo.relative(&scanned.0).into_iter().collect::<Vec<_>>(),
      paths(&["a.txt", "ignored/kept.txt", "ignored/sub/deep.txt", "nocfg/c.txt", "tracked.log"])
    );
    assert_eq!(scanned.1, vec![repo.root.join("cfg/dprint.json")]);
  }

  /// A xorshift generator, so a failing seed reproduces.
  struct Rng(u64);

  impl Rng {
    fn below(&mut self, bound: usize) -> usize {
      self.0 ^= self.0 << 13;
      self.0 ^= self.0 >> 7;
      self.0 ^= self.0 << 17;
      (self.0 % bound as u64) as usize
    }

    fn chance(&mut self, percent: usize) -> bool {
      self.below(100) < percent
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
      items[self.below(items.len())]
    }
  }

  const FILE_NAMES: &[&str] = &[
    "a", "b", "foo", "bar.log", "x.txt", "y.rs", "Ab", ".hidden", "[x]", "a b", "#c", "!d", "e*f", "g?h", "deep", "logs", "z.LOG",
  ];
  const DIR_NAMES: &[&str] = &["a", "src", "build", "logs", "x", "[d]", "sp ace", "node", ".dot", "deep", "foo"];
  const PATTERNS: &[&str] = &[
    "a",
    "foo",
    "logs",
    "build",
    "x",
    "*",
    "*.log",
    "*.txt",
    "?",
    "[ab]*",
    "[!a]*",
    "[a-c]",
    "**",
    "x*",
    "*/*.rs",
    "a/*",
    "**/foo",
    "src/**",
    "**/logs/**",
    "a/**/b",
    "\\[x]",
    "[[]d]",
    "\\#c",
    "\\!d",
    "e\\*f",
    "g?h",
    "deep/",
    "*.[lL][oO][gG]",
    "sp ace",
    "a\\ b",
    "**/[d]/**",
    ".*",
    "node/",
    "src/*/",
    "*/deep",
  ];

  fn random_pattern(rng: &mut Rng) -> String {
    match rng.below(20) {
      0 => "# a comment".to_string(),
      1 => String::new(),
      _ => {
        let mut pattern = rng.pick(PATTERNS).to_string();
        if rng.chance(20) && !pattern.starts_with('/') {
          pattern.insert(0, '/');
        }
        if rng.chance(15) && !pattern.ends_with('/') {
          pattern.push('/');
        }
        if rng.chance(25) {
          pattern.insert(0, '!');
        }
        if rng.chance(5) {
          pattern.push_str("  ");
        }
        pattern
      }
    }
  }

  fn random_tree(rng: &mut Rng, repo: &RealRepo, dir: &str, depth: usize, files: &mut Vec<String>) {
    let join = |name: &str| if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") };
    for _ in 0..1 + rng.below(4) {
      let path = join(rng.pick(FILE_NAMES));
      if !files.contains(&path) && repo.try_write(&path, "") {
        files.push(path);
      }
    }
    if rng.chance(if dir.is_empty() { 70 } else { 35 }) {
      let lines = (0..1 + rng.below(6)).map(|_| random_pattern(rng)).collect::<Vec<_>>();
      if repo.try_write(&join(".gitignore"), &format!("{}\n", lines.join("\n"))) {
        files.push(join(".gitignore"));
      }
    }
    if depth < 3 {
      for _ in 0..rng.below(4) {
        let name = rng.pick(DIR_NAMES);
        if !files.iter().any(|file| file == &join(name)) {
          random_tree(rng, repo, &join(name), depth + 1, files);
        }
      }
    }
  }

  /// Builds a random repository, runs `git status`, changes files, and compares
  /// the walk and the index scan with `git ls-files`.
  fn check_against_git(seed: u64) {
    let Some(repo) = RealRepo::new(if seed.is_multiple_of(2) { "2" } else { "4" }) else {
      return;
    };
    let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let mut files = Vec::new();
    random_tree(&mut rng, &repo, "", 0, &mut files);
    if rng.chance(20) {
      let lines = (0..1 + rng.below(3)).map(|_| random_pattern(&mut rng)).collect::<Vec<_>>();
      repo.write(".git/info/exclude", &format!("{}\n", lines.join("\n")));
    }
    repo.git(&["add", "-A"]);
    for _ in 0..rng.below(3) {
      let path = files[rng.below(files.len())].clone();
      repo.git(&["add", "-f", "--", &path]);
    }
    repo.git(&["status", "--porcelain"]);
    repo.git(&["status", "--porcelain"]);

    let mut changes = Vec::new();
    for _ in 0..rng.below(4) {
      let dir = rng.pick(DIR_NAMES);
      let path = format!(
        "{}/{}",
        if rng.chance(50) { dir.to_string() } else { format!("{dir}/new") },
        rng.pick(FILE_NAMES)
      );
      if repo.try_write(&path, "") {
        changes.push(path.clone());
        changes.push(format!("{}/", parent_of(path.as_bytes()).escape_ascii()));
      }
    }
    for _ in 0..rng.below(3) {
      let path = files[rng.below(files.len())].clone();
      if repo.environment.remove_file(repo.root.join(&path)).is_ok() {
        changes.push(path);
      }
    }
    if rng.chance(30) {
      let lines = (0..1 + rng.below(4)).map(|_| random_pattern(&mut rng)).collect::<Vec<_>>();
      repo.write(".gitignore", &format!("{}\n", lines.join("\n")));
      changes.push(".gitignore".to_string());
    }
    let changes = changes.iter().flat_map(|path| [path.as_bytes(), b"\0"]).flatten().copied().collect::<Vec<_>>();

    let expected = repo.git_files();
    let walked = repo.relative(&walk_dir(&repo.environment, repo.options("**/*", false)).unwrap().file_paths);
    let scanned = repo.relative(&repo.scan_with_index(&repo.options("**/*", false), &changes).file_paths);
    let gitignores = files
      .iter()
      .filter(|path| path.ends_with(".gitignore"))
      .filter_map(|path| repo.environment.read_file(repo.root.join(path)).ok().map(|text| format!("{path}:\n{text}")))
      .collect::<Vec<_>>()
      .join("\n");
    assert_eq!(walked, expected, "seed {seed}: the walk differs from git\n{gitignores}");
    assert_eq!(scanned, expected, "seed {seed}: the index scan differs from git\n{gitignores}");
  }

  #[test]
  fn matches_git_ls_files() {
    for seed in 1..=40 {
      check_against_git(seed);
    }
  }

  /// `cargo test -p dprint-discovery --release -- --ignored matches_git_ls_files_in_many_repositories`
  #[test]
  #[ignore = "creates thousands of git repositories"]
  fn matches_git_ls_files_in_many_repositories() {
    for seed in 41..=3000 {
      check_against_git(seed);
    }
  }

  struct Daemon<'a>(&'a RealRepo);

  impl Drop for Daemon<'_> {
    fn drop(&mut self) {
      self.0.git_succeeds(&["fsmonitor--daemon", "stop"]);
    }
  }

  fn median(mut times: Vec<std::time::Duration>) -> std::time::Duration {
    times.sort();
    times[times.len() / 2]
  }

  fn time(runs: usize, mut run: impl FnMut()) -> std::time::Duration {
    run();
    median(
      (0..runs)
        .map(|_| {
          let start = std::time::Instant::now();
          run();
          start.elapsed()
        })
        .collect(),
    )
  }

  /// `cargo test -p dprint-discovery --release -- --ignored --nocapture compares_speed_with_git`
  #[test]
  #[ignore = "creates a repository with 110000 files"]
  fn compares_speed_with_git() {
    let Some(repo) = RealRepo::new("4") else {
      return;
    };
    repo.git(&["config", "core.fsmonitor", "true"]);
    for top in 0..10 {
      repo.write(&format!("t{top}/.gitignore"), "*.tmp\nbuild/\n");
      for mid in 0..10 {
        for leaf in 0..20 {
          for file in 0..50 {
            repo.write(&format!("t{top}/m{mid}/l{leaf}/f{file}.ts"), "");
          }
        }
        repo.write(&format!("t{top}/m{mid}/x.tmp"), "");
        repo.write(&format!("t{top}/m{mid}/new.ts"), "");
      }
    }
    for dir in 0..100 {
      for file in 0..100 {
        repo.write(&format!("node_modules/p{dir}/f{file}.js"), "");
      }
    }
    repo.write(".gitignore", "node_modules/\n");
    repo.git(&["add", "--", ".gitignore", "t*/m*/l*"]);
    let _daemon = Daemon(&repo);
    repo.git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "x"]);
    if !repo.git_succeeds(&["fsmonitor--daemon", "status"]) {
      repo.git(&["fsmonitor--daemon", "start"]);
    }
    repo.git(&["status", "--porcelain"]);
    repo.git(&["status", "--porcelain"]);
    repo.write("t3/m3/l3/f3.ts", "changed");
    repo.write("t5/m5/added.ts", "");
    std::thread::sleep(std::time::Duration::from_millis(500));

    let runs = 21;
    let options = || repo.options("**/*.ts", false);
    let scanned = scan_with_git_index(&repo.environment, &options()).unwrap().expect("the index scan ran");
    let walked = walk_dir(&repo.environment, options()).unwrap();
    assert_eq!(sorted(scanned), sorted(walked));
    let git_baseline = time(runs, || {
      repo.git(&["rev-parse", "--git-dir"]);
    });
    let git_status = time(runs, || {
      repo.git(&["status", "--porcelain"]);
    });
    let git_status_all = time(runs, || {
      repo.git(&["status", "--porcelain", "--untracked-files=all"]);
    });
    let git_ls_files = time(runs, || {
      repo.git(&["ls-files", "-z", "--cached", "--others", "--exclude-standard"]);
    });
    let dprint_index = time(runs, || {
      scan_with_git_index(&repo.environment, &options()).unwrap().unwrap();
    });
    let dprint_walk = time(runs, || {
      walk_dir(&repo.environment, options()).unwrap();
    });
    let options = options();
    let gitignore = options.gitignore.as_ref().unwrap();
    let read_index = time(runs, || {
      RepoIndex::read(&repo.environment, &repo.root).unwrap();
    });
    let load_snapshot = time(runs, || {
      Snapshot::load(&repo.environment, &options, gitignore).unwrap();
    });
    let snapshot = Snapshot::load(&repo.environment, &options, gitignore).unwrap();
    let collect = time(runs, || {
      Discovery::new(&repo.environment, &options, gitignore, &snapshot).collect().unwrap();
    });
    let collect_and_filter = time(runs, || {
      let mut discovery = Discovery::new(&repo.environment, &options, gitignore, &snapshot);
      discovery.collect().unwrap();
      discovery.into_output().unwrap();
    });
    println!("median of {runs} runs");
    println!("git rev-parse (process start): {git_baseline:?}");
    println!("git status: {git_status:?}");
    println!("git status --untracked-files=all: {git_status_all:?}");
    println!("git ls-files --cached --others: {git_ls_files:?}");
    println!("dprint git index: {dprint_index:?}");
    println!("dprint walk: {dprint_walk:?}");
    println!("dprint phases: read the index {read_index:?}, ask the daemon {load_snapshot:?}, collect {collect:?}, collect and filter {collect_and_filter:?}");
    let trace = repo.root.join(".git").join("trace2");
    let output = std::process::Command::new("git")
      .args(["status", "--porcelain"])
      .env("GIT_TRACE2_PERF", &trace)
      .current_dir(&repo.root)
      .output()
      .unwrap();
    assert!(output.status.success());
    println!("git status trace2 regions:");
    for line in repo.environment.read_file(&trace).unwrap().lines().filter(|line| line.contains("region_leave")) {
      println!("{line}");
    }
  }
}
