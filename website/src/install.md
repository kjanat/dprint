---
title: Install
description: Documentation on installing dprint.
layout: layouts/documentation.njk
---

# Install dprint

Install using one of the methods below.

- Shell (Mac, Linux, WSL, FreeBSD amd64):

  Requires `curl`, `unzip`, and `jq`.

  On FreeBSD 14.4 or newer, install these with `pkg install curl unzip jq`.

  ```sh
  curl -fsSL https://dprint.kjanat.dev/install.sh | sh
  ```

- Powershell (Windows):

  ```sh
  iwr https://dprint.kjanat.dev/install.ps1 -useb | iex
  ```

- [Scoop](https://scoop.sh/) (Windows):

  ```sh
  scoop install dprint
  ```

- [Homebrew](https://brew.sh/) (Mac):

  ```sh
  brew install dprint
  ```

- [Cargo](https://doc.rust-lang.org/cargo/) (builds and installs from the [repository](https://github.com/kjanat/dprint) source):

  ```sh
  # this will be slower since it builds from the source
  cargo install --locked --git https://github.com/kjanat/dprint dprint --bin dprint
  ```

- [npm](https://www.npmjs.com/):

  ```sh
  # for your project
  npm install dprint
  npx dprint help

  # or install globally
  npm install -g dprint
  dprint help
  ```

- python/[uv](https://docs.astral.sh/uv/) via [https://github.com/trim21/dprint-py](https://github.com/trim21/dprint-py):

  ```sh
  uv add dprint-py
  uv run dprint help
  ```

  [mise](https://mise.jdx.dev):

  ```sh
  # for your project
  mise use dprint
  mise x dprint -- dprint help

  # or install globally
  mise use dprint --global
  dprint help
  ```

- [asdf-vm](https://asdf-vm.com/) ([asdf-dprint](https://github.com/asdf-community/asdf-dprint)):

  ```sh
  asdf plugin-add dprint https://github.com/asdf-community/asdf-dprint
  asdf install dprint latest
  ```

- [Arch Linux](https://aur.archlinux.org/packages/dprint):

  Install with any AUR helper, for example:

  ```sh
  paru -S dprint
  ```

  or binaries

  ```sh
  paru -S dprint-bin
  ```

For binaries and source, see the [GitHub releases](https://github.com/kjanat/dprint/releases).

FreeBSD builds target amd64 on FreeBSD 14.4 or newer. Wasm plugins and built-in
`exec` formatting are supported. Process plugins must publish a
`freebsd-x86_64` entry in their manifest; Linux binaries are not used as a fallback.

## Editor Extensions

- [Visual Studio Code](https://marketplace.visualstudio.com/items?itemName=dprint.dprint)
- [IntelliJ](https://plugins.jetbrains.com/plugin/18192-dprint) - Thanks to the developers at [Canva](https://canva.com)
- Neovim with [nvim-lspconfig](https://github.com/neovim/nvim-lspconfig/blob/master/doc/server_configurations.md#dprint)
- The `dprint lsp` subcommand provides code formatting over the language server protocol. This can be used to format in other editors.

Next step: [Setup](/setup)
