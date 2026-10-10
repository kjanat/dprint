use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

pub(crate) struct TempRepo {
  dir: tempfile::TempDir,
  root: PathBuf,
}

impl TempRepo {
  pub fn new(init_args: &[&str]) -> Option<Self> {
    let dir = tempfile::tempdir().ok()?;
    let root = dir.path().join("repo");
    std::fs::create_dir_all(dir.path().join("home")).ok()?;
    let repo = TempRepo { dir, root };
    let output = repo
      .command(repo.dir.path())
      .arg("init")
      .arg("-q")
      .args(init_args)
      .arg(&repo.root)
      .output()
      .ok()?;
    output.status.success().then_some(repo)
  }

  pub fn root(&self) -> &Path {
    &self.root
  }

  fn command(&self, current_dir: &Path) -> Command {
    let home = self.dir.path().join("home");
    let mut command = Command::new("git");
    command
      .env_clear()
      .env("PATH", std::env::var_os("PATH").unwrap_or_default())
      .env("HOME", &home)
      .env("XDG_CONFIG_HOME", home.join(".config"))
      .env("GIT_CONFIG_NOSYSTEM", "1")
      .env("GIT_AUTHOR_NAME", "Test")
      .env("GIT_AUTHOR_EMAIL", "test@example.com")
      .env("GIT_COMMITTER_NAME", "Test")
      .env("GIT_COMMITTER_EMAIL", "test@example.com")
      .env("LC_ALL", "C")
      .current_dir(current_dir);
    command
  }

  pub fn try_git(&self, args: &[&str]) -> std::process::Output {
    self.command(&self.root).args(args).output().unwrap()
  }

  pub fn git(&self, args: &[&str]) -> Vec<u8> {
    let output = self.try_git(args);
    assert!(output.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&output.stderr));
    output.stdout
  }

  pub fn write(&self, path: &str, contents: &[u8]) {
    let path = self.root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
  }

  pub fn path(&self, path: &str) -> PathBuf {
    self.root.join(path)
  }

  pub fn read(&self, path: &str) -> Vec<u8> {
    std::fs::read(self.root.join(path)).unwrap()
  }
}
