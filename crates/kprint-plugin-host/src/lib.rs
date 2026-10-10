#[macro_use]
extern crate kprint_platform;
pub use kprint_platform::cache;
pub use kprint_platform::utils;
pub mod environment {
  pub use kprint_platform::environment::*;
  #[cfg(test)]
  pub use kprint_test_support::environment::*;
}
pub use kprint_config::resolution as configuration;
mod plugins;
#[cfg(test)]
pub use kprint_test_support::test_helpers;
pub use plugins::*;
#[cfg(test)]
extern crate kprint_test_support;
