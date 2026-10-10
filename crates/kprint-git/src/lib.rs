//! Readers for git's on-disk formats and its gitignore rules, written from
//! git's documentation.

mod config;
mod ewah;
mod hash;
mod index_file;
mod oid;
mod pattern;
mod pkt_line;
mod reader;
#[cfg(test)]
mod test_git;
mod untracked_cache;

pub use config::*;
pub use index_file::*;
pub use oid::*;
pub use pattern::*;
pub use pkt_line::*;
pub use untracked_cache::*;

#[cfg(any(test, feature = "test-util"))]
pub mod test_util {
  pub use crate::index_file::test_writer::*;
  pub use crate::untracked_cache::test_writer::*;
}
