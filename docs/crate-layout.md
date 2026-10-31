# Crate layout

`kprint-core` is a compatibility facade. Implementations live in crates with distinct responsibilities, and the CLI depends on them directly.

| Crate                   | Responsibility                                                                    | Internal dependencies                                                  |
| ----------------------- | --------------------------------------------------------------------------------- | ---------------------------------------------------------------------- |
| `kprint-configuration`  | Format-neutral configuration values, diagnostics, global settings, and resolution | None                                                                   |
| `kprint-formatting`     | Formatting IR, printing engine, tokens, and optional tracing                      | None                                                                   |
| `kprint-plugin-types`   | Plugin metadata, shared configuration payloads, cancellation, and format errors   | Configuration; async runtime with `async_runtime`                      |
| `kprint-wasm-plugin`    | Synchronous plugin handlers and Wasm ABI code generation                          | Configuration, plugin types                                            |
| `kprint-process-plugin` | Process stdio protocol and child communication                                    | Configuration, plugin types, communication, async runtime, owned child |
| `kprint-communication`  | Framed message readers/writers and message tracking                               | Async runtime                                                          |
| `kprint-async-runtime`  | Current-thread task spawning and future helpers                                   | None                                                                   |
| `kprint-owned-child`    | OS process groups, job objects, and child cleanup                                 | None                                                                   |

The formatting engine has no plugin or async dependencies. Configuration has no dependency on the formatting engine, plugin APIs, or transports. Wasm plugins have no process or async dependencies by default. Process ownership depends only on platform libraries.

## Host ownership

| Crate                 | Responsibility                                                                                 |
| --------------------- | ---------------------------------------------------------------------------------------------- |
| `kprint-host-api`     | Capability traits, frontend options, compiler control and UI contracts; no host implementation |
| `kprint-platform`     | IO utilities, HTTP cache storage, and reusable `NativeEnvironment<S>`                          |
| `kprint-discovery`    | File traversal, globs, gitignore and selection patterns                                        |
| `kprint-config`       | Config syntax and schema, layered resolution, plugin references and remote execution policy    |
| `kprint-plugin-host`  | Plugin acquisition, caches and Wasm, process and in-process execution                          |
| `kprint-host`         | Configured formatting scopes, routing, batching, incremental state and `HostSession`           |
| `kprint-lsp`          | LSP protocol, documents, ranges and config completion                                          |
| `dprint`              | Command parsing, CLI commands, terminal UI and HTTP/TLS policy                                 |
| `kprint-test-support` | In-memory environment, config builders and binary/archive fixtures                             |

Dependencies point from frontends toward host services and capability contracts. Libraries never depend on the CLI, including through their test support. The test-support `plugins` feature enables engine-backed fixtures; filesystem, discovery and configuration tests use the lightweight default. Tests of command parsing and CLI behavior live in `dprint`.

Configuration values and `GlobalConfiguration` remain available through `kprint-core::configuration`. Their implementation is in `kprint-configuration`; document syntax, includes, remote policy and schema composition belong to `kprint-config`.

### Scopes and plugin lifetime

`HostSession` resolves a replacement scope before cancelling frontend requests or replacing the last valid scope. Plugins are shared by source, with separate configuration IDs. Refreshing one config never stops another config's plugins. The resolver retires a cached plugin only when scopes, pending resolutions and initialized-instance users have released it. An externally retained old scope can finish formatting with its original configuration. Explicit session shutdown stops all its plugins and clears the initialized instances from their wrappers.

### Environment capabilities

`FileSystemEnvironment` requires filesystem operations without logging, clocks, randomness or sleep. The sys-traits bridge needed by native plugin execution is separate (`FileSystemSys`). The HTTP cache adapts the filesystem's atomic write operation instead of requiring an embedder to implement that bridge.

`ConfigEnvironment` combines file access, caching, platform identity, clocks, downloads and consent. It requires neither process management nor executable discovery, CPU measurements, terminal streams, selections or a compiler. `DiscoveryEnvironment` needs file access, environment variables, VCS metadata and output. `PluginEnvironment` adds the compiler and native sys interfaces; `HostEnvironment` combines those with config and discovery policy and formatting concurrency. The full `Environment` is reserved for CLI orchestration.

`NativeEnvironment<S>` supplies native filesystem, directories, git and runtime behavior from `kprint-platform`. Services `S` supply output, interaction, downloads and optional compilation. `HeadlessServices` provides silent local embedding with errors for operations that require unavailable services. The CLI injects its terminal, TLS and supervised compiler adapter. Its application version is passed explicitly, so library versions cannot leak into cache or update checks.

## Using the crates directly

Replace imports as follows, and declare the corresponding crates in `Cargo.toml`:

| Existing path                                                     | Direct path                                 |
| ----------------------------------------------------------------- | ------------------------------------------- |
| `kprint_core::configuration`                                      | `kprint_configuration`                      |
| `kprint_core::formatting`                                         | `kprint_formatting`                         |
| `kprint_core::async_runtime`                                      | `kprint_async_runtime`                      |
| `kprint_core::communication`                                      | `kprint_communication`                      |
| `kprint_core::owned_child`                                        | `kprint_owned_child`                        |
| Shared types in `kprint_core::plugins`                            | `kprint_plugin_types`                       |
| `AsyncPluginHandler`, `FormatRequest`, `HostFormatRequest`        | `kprint_plugin_types`                       |
| `SyncPluginHandler`, `SyncFormatRequest`, `SyncHostFormatRequest` | `kprint_wasm_plugin`                        |
| `kprint_core::plugins::process`                                   | `kprint_process_plugin`                     |
| `kprint_core::plugins::wasm`                                      | `kprint_wasm_plugin`                        |
| `kprint_core::generate_plugin_code!`                              | `kprint_wasm_plugin::generate_plugin_code!` |

`kprint-formatting` exposes its printing API at the crate root and has an optional `tracing` feature. `kprint-plugin-types` provides an optional `async_runtime` feature for asynchronous handlers, nested host callbacks, cancellation and error conversions; process plugins enable it automatically.

The Wasm macro resolves its dependencies through its defining crate, so it works with renamed dependencies and through the facade. Plugins do not need a direct `serde_json` dependency just to expand the macro.

## Compatibility

Existing `kprint-core` module paths and the `formatting`, `tracing`, `wasm`, `process`, `communication`, and `async_runtime` features remain available. Re-exports preserve type identity, allowing consumers of the facade and the direct crates to exchange configuration and plugin values. `kprint-core-macros` continues to generate formatting paths through the facade.

## Publishing

The new library crates begin at `0.1.0`. The existing core publishing workflow defaults to publishing them in dependency order before publishing the facade. Its `packages` input can select just the changed packages for subsequent releases; leave out versions already published. When preparing a release, update versions and exact workspace dependency pins together. The facade's existing version is unchanged by this refactor; publishing an already released version requires a version bump as usual.

The CLI publishing workflow requires its library dependencies to have been published first. This refactor does not publish any packages.
