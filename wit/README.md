# dprint Wasm plugin ABI, as WIT

Each directory describes one plugin system schema version of dprint's Wasm plugin ABI in [WIT](https://component-model.bytecodealliance.org/design/wit.html), the interface definition language of the Wasm component model: what a plugin exports, what the host provides as imports, the byte-passing protocol between them, and the JSON payloads they exchange.

| Schema | Package               | dprint                      | dprint-core             | Status                                      |
| ------ | --------------------- | --------------------------- | ----------------------- | ------------------------------------------- |
| 4      | `dprint:plugin@4.0.0` | 0.47.0 (2024-07-01) onwards | 0.67.0 onwards          | current: what `generate_plugin_code!` emits |
| 3      | `dprint:plugin@3.0.0` | 0.8.0 (2020-08-05) onwards  | 0.27.0 to 0.66.x        | still run by the current host               |
| 2      | `dprint:plugin@2.0.0` | 0.7.0 to 0.7.4              | 0.26.x                  | historical                                  |
| 1      | `dprint:plugin@1.0.0` | 0.4.0 to 0.6.x              | 0.20.0-alpha3 to 0.25.x | historical                                  |

## What these files are, and aren't

A dprint plugin is a **core Wasm module**, not a component. It exports a linear `memory` and plain functions whose parameters and results are all `i32` (on wasm32 `u32`, `u8`, `usize` and pointers are one type), and it passes every payload as bytes through a buffer it owns. WIT can't express that core ABI directly (the component model's canonical ABI lifts and lowers values itself), so these files describe the ABI's _logical_ interface:

- each WIT function is one core export or import, with its core name and signature in its doc comment (`get_shared_bytes_ptr() -> *const u8` is `get-shared-bytes-ptr: func() -> pointer`);
- `u32` stands for every `i32`; `pointer` is an offset into the plugin's memory; a `format-result` is the `u8` code 0, 1 or 2;
- the payloads' shapes are records in a `types` interface, transported as JSON text (`type json = string`), since WIT has no recursive types for a configuration value;
- the byte-passing protocol (shared bytes, chunking through the 4 KiB transfer buffer in versions 1 to 3) is in comments on the interfaces, as it is what makes two core functions one logical call.

A component that implements the `plugin` world of a version would need an adapter to and from the core functions. The files are a specification to read and to check an implementation against (`wasm-tools component wit wit/v4` parses them), not something to generate bindings from as they are.

## How the versions differ

- **1 to 2**: the same functions; `set_plugin_config`'s values went from strings only to a string, a number or a boolean.
- **2 to 3**: `set_override_config` (plugin) and `host_take_override_config` (host) for per-file overrides. Within 3, without a bump: `fileNames` in the plugin info (dprint 0.15.0), `updateUrl` (0.22.0), `reset_config` (0.24.2), array, object and null configuration values (0.28.0), formatted output as arbitrary bytes (0.43.2).
- **3 to 4**: configurations are registered by id (`register_config`, `release_config`) instead of set one at a time (`set_global_config`, `set_plugin_config`); the file matching moved out of the plugin info into `get_config_file_matching`; `format_range` and `check_config_updates` were added; the chunked transfer buffer was replaced by direct access to the shared bytes (`get_shared_bytes_ptr`, `clear_shared_bytes` returning the pointer); the host's buffer protocol (`host_clear_bytes`, `host_read_buffer`, `host_take_*`) was replaced by pointer/length arguments to `host_format` and a one-call `host_write_buffer`; `host_has_cancelled` and `env.fd_write` were added.
- **Version detection**: up to dprint 0.46.1 the host called `get_plugin_schema_version()` and required its own version exactly. Since 0.46.2 it looks at export names only: `dprint_plugin_version_<n>` means n, and `get_plugin_schema_version` is taken to mean 3 (so a version 1 or 2 plugin would be mistaken for a version 3 one).

## Sources

Current: `crates/dprint-wasm-plugin/src/lib.rs` (the plugin side, `generate_plugin_code!`), `crates/dprint/src/plugins/implementations/wasm/instance/{mod,v3,v4}.rs` (the host side), `crates/dprint-plugin-types/src/{plugin_info,plugin_handler}.rs` and `crates/dprint-configuration/src/lib.rs` (the payloads), `docs/wasm-plugin-development.md`. Historical: the same files at the commits that introduced each version (d3f4866 for 1, 209c386 for 2, 100b0a8 for 3, 3b0c101 for 4).
