#[macro_use]
extern crate dprint_host_api;
pub mod cache;
pub mod compiler;
pub mod environment;
pub mod utils;
#[cfg(test)]
pub use dprint_test_support::test_helpers;
#[cfg(test)]
#[macro_use]
extern crate dprint_test_support;

pub use dprint_host_api::log_all;
pub use dprint_host_api::log_debug;
pub use dprint_host_api::log_error;
pub use dprint_host_api::log_stderr_info;
pub use dprint_host_api::log_stdout_info;
pub use dprint_host_api::log_warn;

extern crate self as dprint_platform;
