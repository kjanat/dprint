#[macro_use]
extern crate dprint_platform;
pub use dprint_platform::cache;
pub use dprint_platform::utils;
pub mod environment {
  pub use dprint_platform::environment::*;
  #[cfg(test)]
  pub use dprint_test_support::environment::*;
}
pub use dprint_config::resolution as configuration;
mod plugins;
#[cfg(test)]
pub use dprint_test_support::test_helpers;
pub use plugins::*;
#[cfg(test)]
extern crate dprint_test_support;
