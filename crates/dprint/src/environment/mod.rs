#[cfg(any(test, feature = "test-support"))]
mod environment_file_system;
mod real_environment;
#[cfg(any(test, feature = "test-support"))]
mod test_environment;
#[cfg(any(test, feature = "test-support"))]
mod test_environment_builder;

pub use dprint_platform::environment::*;
pub use real_environment::*;

#[cfg(any(test, feature = "test-support"))]
pub use environment_file_system::*;

#[cfg(any(test, feature = "test-support"))]
pub use test_environment::*;
#[cfg(any(test, feature = "test-support"))]
pub use test_environment_builder::*;
