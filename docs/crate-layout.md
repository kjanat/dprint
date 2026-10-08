# Crate layout

`dprint-core` is a compatibility facade. Implementations live in crates with distinct responsibilities, and the CLI depends on them directly.

| Crate                   | Responsibility                                                                    | Internal dependencies                                                  |
| ----------------------- | --------------------------------------------------------------------------------- | ---------------------------------------------------------------------- |
| `dprint-configuration`  | Format-neutral configuration values, diagnostics, global settings, and resolution | None                                                                   |
| `dprint-formatting`     | Formatting IR, printing engine, tokens, and optional tracing                      | None                                                                   |
| `dprint-plugin-types`   | Plugin metadata, shared configuration payloads, cancellation, and format errors   | Configuration; async runtime with `async_runtime`                      |
| `dprint-wasm-plugin`    | Synchronous plugin handlers and Wasm ABI code generation                          | Configuration, plugin types                                            |
| `dprint-process-plugin` | Asynchronous plugin handlers and process stdio protocol                           | Configuration, plugin types, communication, async runtime, owned child |
| `dprint-communication`  | Framed message readers/writers and message tracking                               | Async runtime                                                          |
| `dprint-async-runtime`  | Current-thread task spawning and future helpers                                   | None                                                                   |
| `dprint-owned-child`    | OS process groups, job objects, and child cleanup                                 | None                                                                   |

The formatting engine has no plugin or async dependencies. Configuration has no dependency on the formatting engine, plugin APIs, or transports. Wasm plugins have no process or async dependencies by default. Process ownership depends only on platform libraries.

## Using the crates directly

Replace imports as follows, and declare the corresponding crates in `Cargo.toml`:

| Existing path                                                     | Direct path                                 |
| ----------------------------------------------------------------- | ------------------------------------------- |
| `dprint_core::configuration`                                      | `dprint_configuration`                      |
| `dprint_core::formatting`                                         | `dprint_formatting`                         |
| `dprint_core::async_runtime`                                      | `dprint_async_runtime`                      |
| `dprint_core::communication`                                      | `dprint_communication`                      |
| `dprint_core::owned_child`                                        | `dprint_owned_child`                        |
| Shared types in `dprint_core::plugins`                            | `dprint_plugin_types`                       |
| `AsyncPluginHandler`, `FormatRequest`, `HostFormatRequest`        | `dprint_process_plugin`                     |
| `SyncPluginHandler`, `SyncFormatRequest`, `SyncHostFormatRequest` | `dprint_wasm_plugin`                        |
| `dprint_core::plugins::process`                                   | `dprint_process_plugin`                     |
| `dprint_core::plugins::wasm`                                      | `dprint_wasm_plugin`                        |
| `dprint_core::generate_plugin_code!`                              | `dprint_wasm_plugin::generate_plugin_code!` |

`dprint-formatting` exposes its printing API at the crate root and has an optional `tracing` feature. `dprint-plugin-types` provides an optional `async_runtime` feature for asynchronous cancellation and error conversions; process plugins enable it automatically.

The Wasm macro resolves its dependencies through its defining crate, so it works with renamed dependencies and through the facade. Plugins do not need a direct `serde_json` dependency just to expand the macro.

## Compatibility

Existing `dprint-core` module paths and the `formatting`, `tracing`, `wasm`, `process`, `communication`, and `async_runtime` features remain available. Re-exports preserve type identity, allowing consumers of the facade and the direct crates to exchange configuration and plugin values. `dprint-core-macros` continues to generate formatting paths through the facade.

## Publishing

The new library crates begin at `0.1.0`. The existing core publishing workflow defaults to publishing them in dependency order before publishing the facade. Its `packages` input can select just the changed packages for subsequent releases; leave out versions already published. When preparing a release, update versions and exact workspace dependency pins together. The facade's existing version is unchanged by this refactor; publishing an already released version requires a version bump as usual.

The CLI publishing workflow requires its library dependencies to have been published first. This refactor does not publish any packages.
