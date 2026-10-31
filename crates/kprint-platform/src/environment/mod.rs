pub use kprint_host_api::environment::*;
pub use native::HeadlessServices;
pub use native::NativeEnvironment;
pub use native::NativeServices;
mod native;
#[cfg(test)]
pub type RealEnvironment = NativeEnvironment<HeadlessServices>;
#[cfg(test)]
pub use kprint_test_support::environment::TestEnvironment;
#[cfg(test)]
pub use kprint_test_support::environment::TestEnvironmentBuilder;
