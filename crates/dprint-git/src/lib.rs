//! Ports of git's on-disk formats and matching rules.
//!
//! https://github.com/git/git/commit/6de20f6092dcf9bdb1c8efe03db4b70c82b423dd

mod bytes;
mod config;
mod ewah;
mod index_file;
mod oid;
mod pattern;
mod pkt_line;
mod untracked_cache;
mod wildmatch;

pub use config::*;
pub use index_file::*;
pub use oid::*;
pub use pattern::*;
pub use pkt_line::*;
pub use untracked_cache::*;
pub use wildmatch::*;

#[cfg(any(test, feature = "test-util"))]
pub mod test_util {
  pub use crate::bytes::encode_varint;
  pub use crate::ewah::write_ewah;
  pub use crate::index_file::test_writer::*;
  pub use crate::untracked_cache::test_writer::*;
}
