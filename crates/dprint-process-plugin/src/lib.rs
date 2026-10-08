#![allow(clippy::bool_to_int_with_if)]
#![deny(clippy::print_stderr)]
#![deny(clippy::print_stdout)]
#![deny(clippy::unused_async)]

mod plugin_handler;
pub use plugin_handler::*;

mod communicator;
mod context;
mod message_processor;
mod messages;
mod parent_process_checker;
mod shared_types;
mod utils;

pub use communicator::*;
pub use message_processor::*;
pub use parent_process_checker::*;
use shared_types::*;
pub use utils::setup_exit_process_panic_hook;
