#[macro_use]
extern crate kprint_host_api;
pub mod cache;
pub mod compiler;
pub mod environment;
pub mod utils;
#[cfg(test)]
pub use kprint_test_support::test_helpers;
#[cfg(test)]
extern crate kprint_test_support;

pub use kprint_host_api::log_all;
pub use kprint_host_api::log_debug;
pub use kprint_host_api::log_error;
pub use kprint_host_api::log_stderr_info;
pub use kprint_host_api::log_stdout_info;
pub use kprint_host_api::log_warn;

extern crate self as kprint_platform;
