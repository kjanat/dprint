use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use anyhow::bail;
use kprint_git::EntryKind;
use kprint_git::FsmonitorData;
use kprint_git::IndexEntry;
use kprint_git::UntrackedCache;

use crate::environment::DiscoveryEnvironment as Environment;
use crate::git_repo::GitRepo;
use crate::git_repo::find_repo;
use crate::git_repo::read_index;
use crate::utils::DirPrefix;
use crate::utils::path_to_slash_bytes;

/// The index entries below one directory of the work tree.
#[derive(Debug, Clone)]
pub(crate) struct IndexDir {
  /// The length of the directory's relative path and its `/`.
  prefix_len: usize,
  entries: Range<usize>,
}

/// A repository's index. Git applies no gitignore to its tracked paths, so
/// neither does dprint.
#[derive(Debug)]
pub struct RepoIndex {
  pub(crate) repo: GitRepo,
  work_tree: DirPrefix,
  pub(crate) hash_len: usize,
  /// Sorted by path, then by stage, like git's index.
  pub(crate) entries: Vec<IndexEntry>,
  pub(crate) fsmonitor: Option<FsmonitorData>,
  pub(crate) untracked_cache: Option<UntrackedCache>,
}

impl RepoIndex {
  /// `None` outside a repository and for an index dprint can't read.
  pub fn load(environment: &impl Environment, dir: &Path) -> Option<Arc<RepoIndex>> {
    match Self::read(environment, dir) {
      Ok(index) => Some(Arc::new(index)),
      Err(err) => {
        log_debug!(environment, "Not reading the git index for {}: {:#}", dir.display(), err);
        None
      }
    }
  }

  pub(crate) fn read(environment: &impl Environment, dir: &Path) -> Result<RepoIndex> {
    for name in ["GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR", "GIT_INDEX_FILE"] {
      if environment.env_var(name).is_some() {
        bail!("{} is set", name);
      }
    }
    let Some(repo) = find_repo(environment, dir)? else {
      bail!("not in a git work tree");
    };
    let (index, hash_len) = read_index(environment, &repo)?;
    Ok(RepoIndex {
      work_tree: DirPrefix::new(repo.work_tree.clone()),
      repo,
      hash_len,
      entries: index.entries,
      fsmonitor: index.fsmonitor,
      untracked_cache: index.untracked_cache,
    })
  }

  #[cfg(test)]
  pub(crate) fn for_entries(work_tree: &Path, entries: Vec<IndexEntry>) -> Self {
    RepoIndex {
      work_tree: DirPrefix::new(work_tree.to_path_buf()),
      repo: GitRepo {
        work_tree: work_tree.to_path_buf(),
        git_dir: work_tree.join(".git"),
        common_dir: work_tree.join(".git"),
      },
      hash_len: 20,
      entries,
      fsmonitor: None,
      untracked_cache: None,
    }
  }

  /// The entries for `path`, one per stage. Git's `index_name_pos`.
  fn entries_for(&self, path: &[u8]) -> &[IndexEntry] {
    let start = self.entries.partition_point(|entry| entry.path.as_slice() < path);
    let len = self.entries[start..].iter().take_while(|entry| entry.path == path).count();
    &self.entries[start..start + len]
  }

  /// Whether the index has an entry for a path relative to the work tree.
  pub(crate) fn has_entry(&self, path: &[u8]) -> bool {
    !self.entries_for(path).is_empty()
  }

  /// Whether git tracks a file or symlink at `path` in the work tree.
  pub fn contains_file(&self, path: &Path) -> bool {
    self.relative(path).is_some_and(|path| {
      self
        .entries_for(&path)
        .iter()
        .any(|entry| entry.kind != EntryKind::Gitlink && !entry.skip_worktree)
    })
  }

  /// Whether git tracks a submodule at `path`, or a path below it, in the work
  /// tree. Git's `directory_exists_in_index`.
  pub fn contains_dir(&self, path: &Path) -> bool {
    let Some(dir) = self.relative(path) else {
      return false;
    };
    if self
      .entries_for(&dir)
      .iter()
      .any(|entry| entry.kind == EntryKind::Gitlink && !entry.skip_worktree)
    {
      return true;
    }
    let start = self.entries.partition_point(|entry| sorts_before_dir_contents(&entry.path, &dir));
    self.entries[start..]
      .iter()
      .take_while(|entry| is_below(&entry.path, &dir))
      .any(|entry| !entry.skip_worktree)
  }

  /// The entries below a directory, or `None` for a directory outside the work
  /// tree.
  pub(crate) fn dir(&self, path: &Path) -> Option<IndexDir> {
    let dir = self.relative(path)?;
    if dir.is_empty() {
      return Some(IndexDir {
        prefix_len: 0,
        entries: 0..self.entries.len(),
      });
    }
    let start = self.entries.partition_point(|entry| sorts_before_dir_contents(&entry.path, &dir));
    let len = self.entries[start..].partition_point(|entry| is_below(&entry.path, &dir));
    Some(IndexDir {
      prefix_len: dir.len() + 1,
      entries: start..start + len,
    })
  }

  /// The entries for `name` in `dir`, one per stage.
  fn entries_named(&self, dir: &IndexDir, name: &[u8]) -> &[IndexEntry] {
    let entries = &self.entries[dir.entries.clone()];
    let start = entries.partition_point(|entry| &entry.path[dir.prefix_len..] < name);
    let len = entries[start..].iter().take_while(|entry| &entry.path[dir.prefix_len..] == name).count();
    &entries[start..start + len]
  }

  /// Whether git tracks a file or symlink named `name` in `dir`.
  pub(crate) fn contains_file_in(&self, dir: &IndexDir, name: &[u8]) -> bool {
    self
      .entries_named(dir, name)
      .iter()
      .any(|entry| entry.kind != EntryKind::Gitlink && !entry.skip_worktree)
  }

  /// Whether git tracks a submodule named `name` in `dir`, or a path below it.
  pub(crate) fn contains_dir_in(&self, dir: &IndexDir, name: &[u8]) -> bool {
    if self
      .entries_named(dir, name)
      .iter()
      .any(|entry| entry.kind == EntryKind::Gitlink && !entry.skip_worktree)
    {
      return true;
    }
    let entries = &self.entries[dir.entries.clone()];
    let start = entries.partition_point(|entry| sorts_before_dir_contents(&entry.path[dir.prefix_len..], name));
    entries[start..]
      .iter()
      .take_while(|entry| is_below(&entry.path[dir.prefix_len..], name))
      .any(|entry| !entry.skip_worktree)
  }

  /// A path in the work tree, relative to it with `/` separators.
  pub(crate) fn relative<'a>(&self, path: &'a Path) -> Option<std::borrow::Cow<'a, [u8]>> {
    self.work_tree.strip(path).map(path_to_slash_bytes)
  }
}

/// Whether `path` is below `dir`.
fn is_below(path: &[u8], dir: &[u8]) -> bool {
  path.len() > dir.len() && path.starts_with(dir) && path[dir.len()] == b'/'
}

/// Whether `path` sorts before `dir` followed by `/`.
fn sorts_before_dir_contents(path: &[u8], dir: &[u8]) -> bool {
  match path.len().cmp(&dir.len()) {
    std::cmp::Ordering::Less => path <= &dir[..path.len()],
    _ => match path[..dir.len()].cmp(dir) {
      std::cmp::Ordering::Less => true,
      std::cmp::Ordering::Greater => false,
      std::cmp::Ordering::Equal => path.get(dir.len()).is_none_or(|byte| *byte < b'/'),
    },
  }
}

#[cfg(test)]
mod test {
  use super::*;

  fn entry(path: &str, kind: EntryKind, skip_worktree: bool) -> IndexEntry {
    IndexEntry {
      path: path.as_bytes().to_vec(),
      kind,
      stage: 0,
      skip_worktree,
    }
  }

  #[test]
  fn knows_tracked_files_and_their_directories() {
    let index = RepoIndex::for_entries(
      Path::new("/repo"),
      vec![
        entry("a-b", EntryKind::File, false),
        entry("a/b/c.ts", EntryKind::File, false),
        entry("a0", EntryKind::File, false),
        entry("link", EntryKind::Symlink, false),
        entry("mod/sub", EntryKind::Gitlink, false),
        entry("sparse/x.ts", EntryKind::File, true),
      ],
    );
    assert!(index.contains_file(Path::new("/repo/a/b/c.ts")));
    assert!(index.contains_file(Path::new("/repo/a-b")));
    assert!(index.contains_file(Path::new("/repo/link")));
    assert!(!index.contains_file(Path::new("/repo/a/b")));
    assert!(!index.contains_file(Path::new("/repo/mod/sub")));
    assert!(index.contains_dir(Path::new("/repo/a")));
    assert!(index.contains_dir(Path::new("/repo/a/b")));
    assert!(!index.contains_dir(Path::new("/repo/a-b")));
    assert!(!index.contains_dir(Path::new("/repo/a/b/c")));
    assert!(index.contains_dir(Path::new("/repo/mod")));
    assert!(index.contains_dir(Path::new("/repo/mod/sub")));
    assert!(!index.contains_dir(Path::new("/repo/sparse")));
    assert!(!index.contains_file(Path::new("/repo/sparse/x.ts")));
    assert!(!index.contains_file(Path::new("/elsewhere/a/b/c.ts")));
    assert!(index.has_entry(b"sparse/x.ts"));
    assert!(!index.has_entry(b"sparse"));

    let root = index.dir(Path::new("/repo")).unwrap();
    assert!(index.contains_file_in(&root, b"a-b"));
    assert!(index.contains_file_in(&root, b"link"));
    assert!(!index.contains_file_in(&root, b"a"));
    assert!(index.contains_dir_in(&root, b"a"));
    assert!(index.contains_dir_in(&root, b"mod"));
    assert!(!index.contains_dir_in(&root, b"a-b"));
    assert!(!index.contains_dir_in(&root, b"sparse"));
    let a = index.dir(Path::new("/repo/a")).unwrap();
    assert!(index.contains_dir_in(&a, b"b"));
    assert!(!index.contains_file_in(&a, b"b"));
    assert!(!index.contains_file_in(&a, b"c.ts"));
    let b = index.dir(Path::new("/repo/a/b")).unwrap();
    assert!(index.contains_file_in(&b, b"c.ts"));
    assert!(!index.contains_dir_in(&b, b"c.ts"));
    let module = index.dir(Path::new("/repo/mod")).unwrap();
    assert!(index.contains_dir_in(&module, b"sub"));
    assert!(!index.contains_file_in(&module, b"sub"));
    let sparse = index.dir(Path::new("/repo/sparse")).unwrap();
    assert!(!index.contains_file_in(&sparse, b"x.ts"));
    let missing = index.dir(Path::new("/repo/missing")).unwrap();
    assert!(!index.contains_file_in(&missing, b"x.ts"));
    assert!(index.dir(Path::new("/elsewhere")).is_none());
  }

  #[test]
  fn sorts_paths_against_directory_contents() {
    assert!(sorts_before_dir_contents(b"a", b"a"));
    assert!(sorts_before_dir_contents(b"a-b", b"a"));
    assert!(!sorts_before_dir_contents(b"a/", b"a"));
    assert!(!sorts_before_dir_contents(b"a/b", b"a"));
    assert!(!sorts_before_dir_contents(b"a0", b"a"));
    assert!(sorts_before_dir_contents(b"", b"a"));
    assert!(!sorts_before_dir_contents(b"b", b"a"));
    assert!(sorts_before_dir_contents(b"Z", b"a"));
  }
}
