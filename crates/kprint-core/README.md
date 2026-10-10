# kprint-core

[![](https://img.shields.io/crates/v/dprint-core.svg)](https://crates.io/crates/kprint-core)

Compatibility facade for the modular dprint libraries. Implementations live in separate crates; see [the crate layout](../../docs/crate-layout.md) for responsibilities, dependencies, and direct imports.

Features:

- `formatting` - Code to help build a code formatter in Rust (not required for creating a plugin).
- `process` - Code to help build a "process plugin"
- `wasm` - Code to help build a "wasm plugin" (recommended over process plugins)

## Formatting Api

Use:

<!-- dprint-ignore -->
```rust
let result = kprint_core::formatting::format(|| {
    let print_items = ...; // parsed out IR (see example below)
    print_items
}, PrintOptions {
    indent_width: 4,
    max_width: 10,
    use_tabs: false,
    newline_kind: "\n",
});
```

## Example

See [overview.md](../../docs/overview.md).
