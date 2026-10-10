#![allow(clippy::bool_to_int_with_if)]
#![deny(clippy::print_stderr)]
#![deny(clippy::print_stdout)]
#![deny(clippy::unused_async)]

//! Compatibility facade for dprint's independently usable libraries.

pub use kprint_configuration as configuration;

#[cfg(feature = "async_runtime")]
pub use kprint_async_runtime as async_runtime;
#[cfg(feature = "communication")]
pub use kprint_communication as communication;
#[cfg(feature = "formatting")]
pub use kprint_formatting as formatting;
#[cfg(feature = "process")]
pub use kprint_owned_child as owned_child;

#[cfg(any(feature = "process", feature = "wasm"))]
pub mod plugins;

#[cfg(all(feature = "wasm", target_arch = "wasm32", target_os = "unknown"))]
pub use kprint_wasm_plugin::generate_plugin_code;
