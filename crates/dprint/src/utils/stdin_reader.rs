use anyhow::Context;
use anyhow::Result;
use std::io::BufRead;
use std::io::Read;
use std::io::{self};

#[cfg(test)]
pub use tests::TestStdInReader;

pub trait StdInReader: Clone + Send + Sync {
  fn read(&self) -> Result<Vec<u8>>;

  /// Whether stdin is attached to a terminal, in which case there's no
  /// piped input waiting to be read.
  fn is_terminal(&self) -> bool;

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

  #[derive(Default, Clone)]
  pub struct TestStdInReader {
    text: Arc<Mutex<Option<Vec<u8>>>>,
    is_terminal: Arc<Mutex<bool>>,
  }

  impl<S: ToString> From<S> for TestStdInReader {
    fn from(value: S) -> Self {
      Self {
        text: Arc::new(Mutex::new(Some(value.to_string().into_bytes()))),
        is_terminal: Arc::new(Mutex::new(false)),
      }
    }
  }

  impl TestStdInReader {
    pub fn set_is_terminal(&self, value: bool) {
      *self.is_terminal.lock() = value;
    }
  }

  impl StdInReader for TestStdInReader {
    fn is_terminal(&self) -> bool {
      *self.is_terminal.lock()
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
