#[macro_use]
extern crate dprint_platform;
mod gitignore;
mod glob;

mod config_patterns;
pub use config_patterns::*;
pub use dprint_host_api::selection::*;
pub use gitignore::*;
pub use glob::*;
pub mod environment {
  pub use dprint_platform::environment::*;
  #[cfg(test)]
  pub use dprint_test_support::environment::*;
}
pub const POSSIBLE_CONFIG_FILE_NAMES: &[&str] = &["dprint.json", "dprint.jsonc", ".dprint.json", ".dprint.jsonc", "dprint.toml", ".dprint.toml"];
mod utils {
  pub(crate) use crate::gitignore;
  pub use crate::glob::*;
}
#[cfg(test)]
pub use dprint_test_support::test_helpers;
