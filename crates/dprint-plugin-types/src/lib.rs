#![allow(clippy::bool_to_int_with_if)]
#![deny(clippy::print_stderr)]
#![deny(clippy::print_stdout)]
#![deny(clippy::unused_async)]

mod plugin_handler;
mod plugin_info;

pub use plugin_handler::*;
pub use plugin_info::*;
