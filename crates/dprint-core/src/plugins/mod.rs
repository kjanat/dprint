pub use dprint_plugin_types::*;

#[cfg(feature = "process")]
pub use dprint_process_plugin as process;
#[cfg(feature = "process")]
pub use dprint_process_plugin::AsyncPluginHandler;
#[cfg(feature = "process")]
pub use dprint_process_plugin::FormatRequest;
#[cfg(feature = "process")]
pub use dprint_process_plugin::HostFormatRequest;

#[cfg(feature = "wasm")]
pub use dprint_wasm_plugin as wasm;
#[cfg(feature = "wasm")]
pub use dprint_wasm_plugin::SyncFormatRequest;
#[cfg(feature = "wasm")]
pub use dprint_wasm_plugin::SyncHostFormatRequest;
#[cfg(feature = "wasm")]
pub use dprint_wasm_plugin::SyncPluginHandler;
