use anyhow::Context;
use anyhow::Result;
use std::io::BufRead;
use std::io::Read;
use std::io::{self};
use std::path::Path;

#[cfg(test)]
pub use tests::TestStdInReader;

pub trait StdInReader: Clone + Send + Sync {
  fn read(&self) -> Result<Vec<u8>>;

  /// Whether stdin is attached to a terminal, in which case there's no
  /// piped input waiting to be read.
  fn is_terminal(&self) -> bool;

  /// Whether the path names stdin itself rather than a file of its own (ex.
  /// a symlink to `/dev/stdin`, or `/proc/thread-self/fd/0`), so that
  /// reading it reads stdin. A regular file never is, even when stdin is
  /// redirected from it.
  fn is_stdin_path(&self, path: &Path) -> bool;

  /// Reads stdin line by line, skipping blank lines, without buffering the
  /// entire input into a single string. Useful for large lists of file paths.
  fn read_non_empty_lines(&self) -> Result<Vec<String>>;
}

#[derive(Default, Clone, Copy)]
pub struct RealStdInReader;

impl StdInReader for RealStdInReader {
  fn is_terminal(&self) -> bool {
    use std::io::IsTerminal;
    io::stdin().is_terminal()
  }

  #[cfg(unix)]
  // this is the real stdin, like the real environment is the real file
  // system, and it's asked before an environment exists
  #[allow(clippy::disallowed_methods)]
  fn is_stdin_path(&self, path: &Path) -> bool {
    use std::os::fd::AsFd;
    use std::os::unix::fs::MetadataExt;

    // compare what the path and stdin are rather than how the path is
    // spelled. stat follows symlinks and doesn't open the path, so a fifo
    // with no writer doesn't block
    let Ok(path_metadata) = std::fs::metadata(path) else {
      return false;
    };
    // a regular file is a file of its own even when stdin happens to be
    // redirected from it (`-c dprint.json < dprint.json`): opening it by path
    // reads it independently, from its own directory. Only a pipe, terminal
    // or other device is stdin itself
    if path_metadata.is_file() {
      return false;
    }
    let Ok(stdin_fd) = io::stdin().as_fd().try_clone_to_owned() else {
      return false;
    };
    let Ok(stdin_metadata) = std::fs::File::from(stdin_fd).metadata() else {
      return false;
    };
    path_metadata.dev() == stdin_metadata.dev() && path_metadata.ino() == stdin_metadata.ino()
  }

  #[cfg(not(unix))]
  fn is_stdin_path(&self, _path: &Path) -> bool {
    false
  }

  fn read(&self) -> Result<Vec<u8>> {
    let mut text = Vec::new();
    io::stdin().read_to_end(&mut text)?;
    Ok(text)
  }

  fn read_non_empty_lines(&self) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    for line in io::stdin().lock().lines() {
      let line = line.context("Failed reading line from stdin.")?;
      if !line.is_empty() {
        lines.push(line);
      }
    }
    Ok(lines)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use parking_lot::Mutex;
  use std::sync::Arc;

  #[cfg(unix)]
  #[test]
  fn real_reader_never_treats_a_regular_file_as_stdin() {
    // whatever stdin is while the tests run, a regular file is a file of its
    // own, so this holds even when stdin is redirected from it
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("dprint.json");
    std::fs::write(&file_path, "{}").unwrap();
    assert!(!RealStdInReader.is_stdin_path(&file_path));
    // and a path that doesn't exist isn't stdin either
    assert!(!RealStdInReader.is_stdin_path(&dir.path().join("missing.json")));
  }

  #[derive(Default, Clone)]
  pub struct TestStdInReader {
    text: Arc<Mutex<Option<Vec<u8>>>>,
    is_terminal: Arc<Mutex<bool>>,
    /// Paths that turn out to be stdin when looked at, however they're spelled.
    stdin_paths: Arc<Mutex<Vec<std::path::PathBuf>>>,
  }

  impl<S: ToString> From<S> for TestStdInReader {
    fn from(value: S) -> Self {
      Self {
        text: Arc::new(Mutex::new(Some(value.to_string().into_bytes()))),
        is_terminal: Arc::new(Mutex::new(false)),
        stdin_paths: Default::default(),
      }
    }
  }

  impl TestStdInReader {
    pub fn set_is_terminal(&self, value: bool) {
      *self.is_terminal.lock() = value;
    }

    pub fn add_stdin_path(&self, path: impl AsRef<Path>) {
      self.stdin_paths.lock().push(path.as_ref().to_path_buf());
    }
  }

  impl StdInReader for TestStdInReader {
    fn is_terminal(&self) -> bool {
      *self.is_terminal.lock()
    }

    fn is_stdin_path(&self, path: &Path) -> bool {
      self.stdin_paths.lock().iter().any(|stdin_path| stdin_path == path)
    }

    fn read(&self) -> Result<Vec<u8>> {
      let text = self.text.lock();
      Ok(text.as_ref().expect("Expected to have stdin text set.").clone())
    }

    fn read_non_empty_lines(&self) -> Result<Vec<String>> {
      let bytes = self.read()?;
      let text = String::from_utf8(bytes).context("Failed reading stdin as UTF-8.")?;
      Ok(text.lines().filter(|line| !line.is_empty()).map(String::from).collect())
    }
  }
}
