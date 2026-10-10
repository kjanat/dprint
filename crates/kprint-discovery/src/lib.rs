#[macro_use]
extern crate kprint_platform;
#[cfg(unix)]
mod git_index;
mod git_repo;
mod gitignore;
mod glob;
mod repo_index;

mod config_patterns;
pub use config_patterns::*;
pub use gitignore::*;
pub use glob::*;
pub use kprint_host_api::selection::*;
pub mod environment {
  pub use kprint_platform::environment::*;
  #[cfg(test)]
  pub use kprint_test_support::environment::*;
}
pub const POSSIBLE_CONFIG_FILE_NAMES: &[&str] = &["dprint.json", "dprint.jsonc", ".dprint.json", ".dprint.jsonc", "dprint.toml", ".dprint.toml"];
mod utils {
  pub(crate) use crate::gitignore;
  pub use crate::glob::*;
}
#[cfg(test)]
pub use kprint_test_support::test_helpers;
