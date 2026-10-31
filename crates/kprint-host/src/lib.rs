#[macro_use]
extern crate kprint_platform;
pub mod environment {
  pub use kprint_platform::environment::*;
  #[cfg(test)]
  pub use kprint_test_support::environment::*;
}
pub use kprint_plugin_host as plugins;
mod get_plugin_config_map;
pub use get_plugin_config_map::*;
pub mod configuration {
  pub use crate::get_plugin_config_map::*;
  pub use kprint_config::resolution::*;
}
pub mod format;
pub mod incremental;
pub mod paths;
pub mod patterns;
pub mod resolution;
mod utils {
  pub use kprint_discovery::*;
  pub use kprint_platform::utils::*;
}
#[cfg(test)]
pub use kprint_test_support::test_helpers;

mod name_resolution;
pub use name_resolution::PluginNameResolutionMaps;
pub mod session;
pub use kprint_config::options::ConfigOptions;
pub use kprint_config::options::SessionOptions;
pub use session::HostSession;

#[cfg(test)]
mod backend_tests;
