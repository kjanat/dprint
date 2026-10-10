use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use kprint_git::IndexFile;
use kprint_git::parse_index;
use kprint_git::parse_repo_config;

use crate::environment::DiscoveryEnvironment as Environment;
use crate::environment::PathKind;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct GitRepo {
  pub work_tree: PathBuf,
  pub git_dir: PathBuf,
  pub common_dir: PathBuf,
}

/// Finds the repository of `dir`.
pub(crate) fn find_repo(environment: &impl Environment, dir: &Path) -> Result<Option<GitRepo>> {
  for work_tree in dir.ancestors() {
    let dot_git = work_tree.join(".git");
    let git_dir = match environment.path_kind(&dot_git) {
      Some(PathKind::Dir) => dot_git,
      Some(PathKind::File) => {
        let text = environment.read_file(&dot_git)?;
        let Some(path) = text.trim_end_matches(['\r', '\n']).strip_prefix("gitdir: ") else {
          bail!("{} has no gitdir line", dot_git.display());
        };
        work_tree.join(path)
      }
      Some(PathKind::Symlink) => bail!("{} is a symlink", dot_git.display()),
      None => continue,
    };
    let common_dir = match environment.maybe_read_file(git_dir.join("commondir"))? {
      Some(text) => git_dir.join(text.trim_end_matches(['\r', '\n'])),
      None => git_dir.clone(),
    };
    return Ok(Some(GitRepo {
      work_tree: work_tree.to_path_buf(),
      git_dir,
      common_dir,
    }));
  }
  Ok(None)
}

/// Reads the index of a repository with a config dprint understands, and the
/// repository's hash length.
pub(crate) fn read_index(environment: &impl Environment, repo: &GitRepo) -> Result<(IndexFile, usize)> {
  let config = parse_repo_config(&environment.maybe_read_file(repo.common_dir.join("config"))?.unwrap_or_default());
  if config.bare || config.has_work_tree_setting || config.has_includes || config.has_worktree_config {
    bail!("the repository config is bare, moves the work tree or has includes");
  }
  if config.ignore_case {
    bail!("core.ignoreCase is set");
  }
  let hash_len = match config.object_format.as_deref() {
    None | Some("sha1") => 20,
    Some("sha256") => 32,
    Some(other) => bail!("unknown object format {}", other),
  };
  let index_path = repo.git_dir.join("index");
  let bytes = match environment.read_file_bytes(&index_path) {
    Ok(bytes) => bytes,
    Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok((IndexFile::default(), hash_len)),
    Err(err) => return Err(err.into()),
  };
  let index = parse_index(&bytes, hash_len).with_context(|| format!("reading {}", index_path.display()))?;
  Ok((index, hash_len))
}

#[cfg(test)]
mod test {
  use super::*;
  use crate::environment::TestEnvironment;
  use kprint_platform::environment::*;

  #[test]
  fn finds_the_repository_above_a_directory() {
    let environment = TestEnvironment::new();
    environment.mk_dir_all("/repo/.git").unwrap();
    environment.mk_dir_all("/repo/a/b").unwrap();
    assert_eq!(
      find_repo(&environment, Path::new("/repo/a/b")).unwrap(),
      Some(GitRepo {
        work_tree: PathBuf::from("/repo"),
        git_dir: PathBuf::from("/repo/.git"),
        common_dir: PathBuf::from("/repo/.git"),
      })
    );
    assert_eq!(find_repo(&environment, Path::new("/other")).unwrap(), None);
  }

  #[test]
  fn follows_a_linked_worktree_to_its_git_dir() {
    let environment = TestEnvironment::new();
    environment.mk_dir_all("/main/.git/worktrees/wt").unwrap();
    environment.write_file("/main/.git/worktrees/wt/commondir", "../..\n").unwrap();
    environment.mk_dir_all("/wt").unwrap();
    environment.write_file("/wt/.git", "gitdir: /main/.git/worktrees/wt\n").unwrap();
    assert_eq!(
      find_repo(&environment, Path::new("/wt")).unwrap(),
      Some(GitRepo {
        work_tree: PathBuf::from("/wt"),
        git_dir: PathBuf::from("/main/.git/worktrees/wt"),
        common_dir: PathBuf::from("/main/.git/worktrees/wt/../.."),
      })
    );
  }

  #[test]
  fn errors_on_a_git_file_without_a_gitdir() {
    let environment = TestEnvironment::new();
    environment.mk_dir_all("/repo").unwrap();
    environment.write_file("/repo/.git", "nonsense").unwrap();
    assert!(find_repo(&environment, Path::new("/repo")).is_err());
  }
}
