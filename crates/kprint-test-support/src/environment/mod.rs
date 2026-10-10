pub use kprint_platform::environment::*;
pub type RealEnvironment = NativeEnvironment<HeadlessServices>;
mod environment_file_system;
mod test_environment;
mod test_environment_builder;
pub use environment_file_system::*;
pub use test_environment::*;
pub use test_environment_builder::*;
