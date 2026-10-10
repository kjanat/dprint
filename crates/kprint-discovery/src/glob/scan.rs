//! Finds files to format by walking a directory with a [tree-fucker] scan.
//!
//! The scan lists directories. All file system work runs under tree-fucker's
//! process-wide governor.
//!
//! [`DiscoveryPolicy`] decides what to do with each listing. A listing shows
//! the directory's `.gitignore`, `.git` and config file, so the policy uses it
//! to pick the directory's files to format and the subdirectories to descend
//! into.
//!
//! The scan reads no file contents except `.gitignore` files. Shebang lines are
//! read later, when resolving plugins, and only for extensionless files that no
//! plugin claims by name.
//!
//! [tree-fucker]: https://github.com/kjanat/tree-fucker

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Once;
use std::time::Duration;

use anyhow::Result;
use anyhow::anyhow;
use parking_lot::Mutex;
use tree_fucker::DirectoryListing;
use tree_fucker::DomainCrossing;
use tree_fucker::EntryInfo;
use tree_fucker::EntryKind;
use tree_fucker::FsError;
use tree_fucker::HostConfig;
use tree_fucker::HostGovernor;
use tree_fucker::ObservedKind;
use tree_fucker::PolicyContext;
use tree_fucker::PolicyRevision;
use tree_fucker::RelativePath;
use tree_fucker::Scan;
use tree_fucker::ScanDecision;
use tree_fucker::ScanEvent;
use tree_fucker::ScanFailure;
use tree_fucker::ScanOptions;
use tree_fucker::ScanPolicy;

use crate::POSSIBLE_CONFIG_FILE_NAMES;
use crate::environment::DiscoveryEnvironment as Environment;
use crate::repo_index::IndexDir;
use crate::repo_index::RepoIndex;
use crate::utils::gitignore::DirEntriesHint;
use crate::utils::gitignore::DirGitIgnores;
use crate::utils::gitignore::GitIgnoreTreeOptions;

use super::ExcludeMatchDetail;
use super::GlobMatcher;
use super::GlobMatchesDetail;
use super::path_to_slash_bytes;

pub struct DirScanOptions {
  /// Directory to walk.
  pub start_dir: PathBuf,
  pub matcher: Arc<GlobMatcher>,
  /// Gitignore settings, or `None` to ignore `.gitignore` files.
  pub gitignore: Option<DirScanGitIgnore>,
  /// Skip directories that have their own config file and report the file
  /// instead. Never applies to the start directory.
  pub discover_configs: bool,
  /// Config file already in use. It never starts a new scope.
  pub current_config_path: Option<PathBuf>,
}

pub struct DirScanGitIgnore {
  /// Gitignores from the directories above the start directory.
  pub above_start_dir: Option<Arc<DirGitIgnores>>,
  pub options: GitIgnoreTreeOptions,
  /// The index of the start directory's repository. A start directory with its
  /// own `.git` loads its own.
  pub index: Option<Arc<RepoIndex>>,
  /// The start directory is inside a gitignored directory.
  pub start_dir_gitignored: bool,
}

#[derive(Debug, Default)]
pub struct DirScanOutput {
  /// Files matching the patterns that aren't gitignored.
  ///
  /// Extensionless files that would match with a shebang line are included
  /// without reading them (see `GlobMatcher::matches_detail_with_shebang_checked`).
  pub file_paths: Vec<PathBuf>,
  /// Config files of the skipped directories.
  pub config_files: Vec<PathBuf>,
}

/// Finds the files to format in [`options.start_dir`][`DirScanOptions::start_dir`], from git's index when it
/// can and by walking the directory otherwise.
pub fn scan_dir<TEnvironment: Environment>(environment: &TEnvironment, options: DirScanOptions) -> Result<DirScanOutput> {
  #[cfg(unix)]
  if let Some(output) = crate::git_index::scan_with_git_index(environment, &options)? {
    return Ok(output);
  }
  walk_dir(environment, options)
}

/// Walks [`options.start_dir`][`DirScanOptions::start_dir`] and returns the files to format.
pub fn walk_dir<TEnvironment: Environment>(environment: &TEnvironment, options: DirScanOptions) -> Result<DirScanOutput> {
  scan_dir_with_file_system(environment, options, environment.scan_file_system())
}

fn scan_dir_with_file_system<TEnvironment: Environment>(
  environment: &TEnvironment,
  options: DirScanOptions,
  file_system: Arc<dyn tree_fucker::FileSystem>,
) -> Result<DirScanOutput> {
  install_governor();
  let start_dir = options.start_dir.clone();
  let policy = Arc::new(DiscoveryPolicy {
    environment: environment.clone(),
    options,
    found: Default::default(),
  });
  let scan = Scan::open(
    file_system,
    start_dir.clone(),
    policy.clone(),
    ScanOptions {
      // dprint has always descended into other mounted file systems
      crossing: DomainCrossing::Follow,
      // no time limit: the user is waiting for the result
      ceiling: Duration::MAX,
      // Excluded files count toward listings too. Discovery must finish even
      // when a directory exceeds the library's default entry limit.
      entries_per_directory: usize::MAX,
      ..ScanOptions::default()
    },
  )
  .map_err(|err| anyhow!("Error reading dir '{}': {}", start_dir.display(), err))?;
  for event in scan {
    match event {
      // the policy handled the entry when its directory was listed, and
      // there are no boundaries because crossings are followed
      Ok(ScanEvent::Entry(_) | ScanEvent::Boundary { .. }) => {}
      Ok(ScanEvent::Unlisted { path, failure }) => {
        let dir_path = path_of(environment, &start_dir, &path);
        match failure {
          // only listing a directory tells whether it can be listed
          ScanFailure::Fs(FsError::PermissionDenied) => {
            log_warn!(environment, "WARNING: Ignoring directory. Permission denied: {}", dir_path.display());
          }
          failure => return Err(anyhow!("Error reading dir '{}': {}", dir_path.display(), failure_message(&failure))),
        }
      }
      Err(err) => return Err(anyhow!("Error reading dir '{}': {}", start_dir.display(), err)),
    }
  }
  Ok(std::mem::take(&mut *policy.found.lock()))
}

/// Installs the process-wide governor that scans run under.
///
/// The user is waiting on file discovery, so a scan gets a full worker instead
/// of the default quarter of one. This matches how fsql sets up its scans.
fn install_governor() {
  static INSTALL: Once = Once::new();
  INSTALL.call_once(|| {
    // installing only fails if a governor already exists, and every scan
    // in dprint goes through here first
    let _ = HostGovernor::install(HostConfig {
      foreground_duty: 1.0,
      domain_foreground_duty: 1.0,
      // Preserve discovery of large directories: a listing is collected
      // before filtering, so the library's tree memory limits don't apply.
      in_flight_listing_bytes: u64::MAX,
      accounted_memory_ceiling: u64::MAX,
      ..HostConfig::default()
    });
  });
}

/// Message for a directory that couldn't be listed. Uses the underlying error
/// text where there is one.
fn failure_message(failure: &ScanFailure) -> String {
  match failure {
    ScanFailure::Fs(FsError::Transient(message) | FsError::Unsupported(message) | FsError::Fatal(message)) => message.clone(),
    failure => failure.to_string(),
  }
}

fn path_of(environment: &impl Environment, start_dir: &Path, path: &RelativePath) -> PathBuf {
  path
    .components()
    .iter()
    .fold(start_dir.to_path_buf(), |dir, name| environment.dir_entry_path(&dir, name))
}

/// Per-directory state used to classify the directory's entries.
struct DirContext {
  dir: PathBuf,
  /// Gitignores that apply to entries of `dir`.
  gitignore: Option<Arc<DirGitIgnores>>,
  /// `dir` is gitignored. Only its tracked entries count.
  gitignored: bool,
  index: Option<Arc<RepoIndex>>,
  /// The index entries below `dir`.
  index_dir: Option<IndexDir>,
  /// `dir` has its own config file. Its contents belong to that config's
  /// scope, so the scan skips them.
  has_config_file: bool,
}

impl DirContext {
  /// Whether git ignores an entry of `dir`. Git looks the path up in the index
  /// first and checks the gitignore only for untracked paths.
  fn is_gitignored(&self, path: &Path, is_dir: bool) -> bool {
    !self.is_tracked(path, is_dir) && (self.gitignored || self.gitignore.as_ref().is_some_and(|gitignore| gitignore.is_ignored(path, is_dir)))
  }

  /// Whether git tracks an entry of `dir`.
  fn is_tracked(&self, path: &Path, is_dir: bool) -> bool {
    let (Some(index), Some(index_dir), Some(name)) = (&self.index, &self.index_dir, path.file_name()) else {
      return false;
    };
    let name = path_to_slash_bytes(Path::new(name));
    if is_dir {
      index.contains_dir_in(index_dir, &name)
    } else {
      index.contains_file_in(index_dir, &name)
    }
  }

  /// Whether `dir` is gitignored, given its parent's context.
  fn child_dir_gitignored(&self, dir: &Path, matcher: &GlobMatcher) -> bool {
    match matcher.is_dir_ignored(dir) {
      // an explicitly opted out exclude takes precedence over the gitignore
      ExcludeMatchDetail::OptedOutExclude => false,
      ExcludeMatchDetail::Excluded | ExcludeMatchDetail::NotExcluded => {
        self.gitignored || self.gitignore.as_ref().is_some_and(|gitignore| gitignore.is_ignored(dir, /* is dir */ true))
      }
    }
  }
}

struct DiscoveryPolicy<TEnvironment: Environment> {
  environment: TEnvironment,
  options: DirScanOptions,
  found: Mutex<DirScanOutput>,
}

impl<TEnvironment: Environment> DiscoveryPolicy<TEnvironment> {
  fn context(&self, context: DirContext) -> PolicyContext {
    // the fingerprint only matters to trees, which compare contexts when the
    // policy changes; a scan never does
    PolicyContext::new(0, context)
  }

  /// Returns the config file in `dir` that starts a new scope, if any. A
  /// gitignored config file only counts when git tracks it.
  fn config_file(&self, context: &DirContext, listing: &DirectoryListing) -> Option<PathBuf> {
    POSSIBLE_CONFIG_FILE_NAMES
      .iter()
      .filter(|file_name| {
        listing
          .entries
          .iter()
          .any(|entry| entry.info.kind == ObservedKind::Resolved(EntryKind::File) && entry.name == **file_name)
      })
      .map(|file_name| self.environment.dir_entry_path(&context.dir, file_name.as_ref()))
      // the config file in use doesn't start a new scope
      .filter(|path| Some(path) != self.options.current_config_path.as_ref())
      .find(|path| !context.is_gitignored(path, /* is dir */ false))
  }
}

/// Windows keeps "System Volume Information" at a drive's root and never lets
/// it be listed, so it isn't tried.
fn is_system_volume_information(name: &std::ffi::OsStr) -> bool {
  cfg!(windows) && name == "System Volume Information"
}

fn dir_context(context: &PolicyContext) -> &DirContext {
  context.get::<DirContext>().expect("the discovery policy only creates directory contexts")
}

impl<TEnvironment: Environment> ScanPolicy for DiscoveryPolicy<TEnvironment> {
  fn revision(&self) -> PolicyRevision {
    PolicyRevision::new(0)
  }

  fn root_context(&self, _root: &EntryInfo) -> PolicyContext {
    let start_dir = &self.options.start_dir;
    let gitignore = self.options.gitignore.as_ref();
    let dir = start_dir.parent().unwrap_or(start_dir).to_path_buf();
    let index = gitignore.and_then(|gitignore| gitignore.index.clone());
    self.context(DirContext {
      index_dir: index.as_ref().and_then(|index| index.dir(&dir)),
      dir,
      gitignore: gitignore.and_then(|gitignore| gitignore.above_start_dir.clone()),
      gitignored: gitignore.is_some_and(|gitignore| gitignore.start_dir_gitignored),
      index,
      has_config_file: false,
    })
  }

  /// Decides whether to descend into a directory. The scan only classifies
  /// directories.
  fn classify(&self, parent: &PolicyContext, path: &RelativePath, _info: &EntryInfo) -> ScanDecision {
    const DESCEND: ScanDecision = ScanDecision::Eligible { initially_loaded: true };
    let Some(name) = path.file_name() else {
      // the start directory is always walked
      return DESCEND;
    };
    let parent = dir_context(parent);
    if parent.has_config_file || name == ".git" || is_system_volume_information(name) {
      return ScanDecision::Excluded;
    }
    let dir_path = self.environment.dir_entry_path(&parent.dir, name);
    if self.options.matcher.is_dir_ignored(&dir_path) == ExcludeMatchDetail::Excluded {
      return ScanDecision::Excluded;
    }
    // a gitignored directory only matters for the tracked paths in it
    if parent.child_dir_gitignored(&dir_path, &self.options.matcher) && !parent.is_tracked(&dir_path, /* is dir */ true) {
      return ScanDecision::Excluded;
    }
    DESCEND
  }

  /// Picks the files to format from a fresh listing and returns the context
  /// for classifying its subdirectories.
  fn child_context(&self, parent: &PolicyContext, path: &RelativePath, listing: &DirectoryListing) -> PolicyContext {
    let parent = dir_context(parent);
    let dir = match path.file_name() {
      Some(name) => self.environment.dir_entry_path(&parent.dir, name),
      None => self.options.start_dir.clone(),
    };
    let mut hint = DirEntriesHint::default();
    for entry in &listing.entries {
      // `.git` is usually a directory but a file in worktrees; symlinks to
      // either are ignored
      if matches!(entry.info.kind, ObservedKind::Resolved(EntryKind::File | EntryKind::Directory)) {
        hint.has_gitignore |= entry.name == ".gitignore";
        hint.has_git |= entry.name == ".git";
      }
    }
    // a repository root starts over with its own gitignores and index
    let (gitignored, index) = if path.is_root() {
      let index = match &parent.index {
        None if hint.has_git && self.options.gitignore.is_some() => RepoIndex::load(&self.environment, &dir),
        index => index.clone(),
      };
      (parent.gitignored && !hint.has_git, index)
    } else if hint.has_git && self.options.gitignore.is_some() {
      (false, RepoIndex::load(&self.environment, &dir))
    } else {
      (parent.child_dir_gitignored(&dir, &self.options.matcher), parent.index.clone())
    };
    let gitignore = match &self.options.gitignore {
      Some(gitignore) if !gitignored => DirGitIgnores::for_listed_dir(&self.environment, &dir, hint, parent.gitignore.as_ref(), &gitignore.options),
      _ => None,
    };
    let context = DirContext {
      index_dir: index.as_ref().and_then(|index| index.dir(&dir)),
      dir,
      gitignore,
      gitignored,
      index,
      has_config_file: false,
    };

    // skip the start directory: it holds the config in use, or it's below that
    // config's directory, and a config file there must not take over the scan
    if self.options.discover_configs
      && !path.is_root()
      && let Some(config_file) = self.config_file(&context, listing)
    {
      self.found.lock().config_files.push(config_file);
      return self.context(DirContext {
        gitignore: None,
        has_config_file: true,
        ..context
      });
    }

    let mut file_paths = Vec::new();
    for entry in &listing.entries {
      // symlinks are not followed
      if entry.info.kind != ObservedKind::Resolved(EntryKind::File) {
        continue;
      }
      let file_path = self.environment.dir_entry_path(&context.dir, &entry.name);
      // assume a matching shebang; plugin resolution reads the file later and
      // drops it if the shebang doesn't match
      let check_gitignore = match self
        .options
        .matcher
        .matches_detail_with_shebang_checked(&file_path, /* has matching shebang */ true)
      {
        GlobMatchesDetail::Excluded | GlobMatchesDetail::NotMatched => continue,
        GlobMatchesDetail::Matched => true,
        // an explicitly opted out exclude takes precedence over the gitignore
        GlobMatchesDetail::MatchedOptedOutExclude => false,
      };
      if !(check_gitignore && context.is_gitignored(&file_path, /* is dir */ false)) {
        file_paths.push(file_path);
      }
    }
    if !file_paths.is_empty() {
      self.found.lock().file_paths.extend(file_paths);
    }
    self.context(context)
  }
}

#[cfg(test)]
mod test {
  use super::*;
  use crate::environment::CanonicalizedPathBuf;
  use crate::environment::TestEnvironment;
  use crate::utils::GlobMatcherOptions;
  use crate::utils::GlobPattern;
  use crate::utils::GlobPatterns;

  use tree_fucker::WatcherKind;
  use tree_fucker::testing::FakeFileSystem;

  #[test]
  fn finds_matching_files_beyond_default_listing_limits() {
    let environment = TestEnvironment::new();
    let fs = FakeFileSystem::new(WatcherKind::None);
    // More than both the default 250,000 entries and the 32 MiB listing cap.
    // Nonmatching files must not prevent discovery of the one matching file.
    for i in 0..250_001 {
      fs.inject_child("", format!("{}.bin", i), EntryKind::File);
    }
    fs.inject_child("", "match.txt", EntryKind::File);
    let base_dir = CanonicalizedPathBuf::new_for_testing("/fake");
    let matcher = GlobMatcher::new(
      GlobPatterns {
        arg_includes: None,
        config_includes: Some(vec![GlobPattern::new("**/*.txt".to_string(), base_dir.clone())]),
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
    let result = scan_dir_with_file_system(
      &environment,
      DirScanOptions {
        start_dir: PathBuf::from("/fake"),
        matcher: Arc::new(matcher),
        gitignore: None,
        discover_configs: false,
        current_config_path: None,
      },
      Arc::new(fs),
    )
    .unwrap();
    assert_eq!(result.file_paths, vec![PathBuf::from("/fake/match.txt")]);
    assert!(result.config_files.is_empty());
  }
}
