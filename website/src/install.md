---
title: Install
description: Documentation on installing kprint.
layout: layouts/documentation.njk
---

# Install kprint <!-- rumdl-disable-line single-title -->

kprint is a fork of dprint. Every method below installs the `kprint` command,
and `dprint` too, for the tools and editor extensions that call that.

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

- [Scoop](https://scoop.sh/) (Windows), from the [kjanat bucket](https://github.com/kjanat/scoop-bucket):

  ```sh
  scoop bucket add kjanat https://github.com/kjanat/scoop-bucket
  scoop install kjanat/kprint
  ```

- [Homebrew](https://brew.sh/) (Mac, Linux), from the [kjanat tap](https://github.com/kjanat/homebrew-tap):

  ```sh
  brew trust kjanat/tap
  brew install kjanat/tap/kprint
  ```

- [AUR](https://aur.archlinux.org/packages?K=kprint) (Arch Linux):

  ```sh
  # prebuilt binary
  yay -S kprint-bin
  # or built from the release source
  yay -S kprint
  # or built from master
  yay -S kprint-git
  ```

- [Cargo](https://doc.rust-lang.org/cargo/) (builds and installs from the [repository](https://github.com/kjanat/kprint) source):

  ```sh
  # this will be slower since it builds from the source
  cargo install --locked --git https://github.com/kjanat/kprint kprint --bin kprint
  ```

  This installs only `kprint`.

- [npm](https://www.npmjs.com/package/@kprint/kprint):

  ```sh
  # for your project
  npm install @kprint/kprint
  npx kprint help

  # or install globally
  npm install -g @kprint/kprint
  kprint help
  ```

  [@kjanat/fmt](https://www.npmjs.com/package/@kjanat/fmt) runs `kprint fmt` as
  `fmt`: `npx @kjanat/fmt`.

- [mise](https://mise.jdx.dev), from the GitHub releases:

  ```sh
  # for your project
  mise use github:kjanat/kprint
  mise x github:kjanat/kprint -- kprint help

  # or install globally
  mise use --global github:kjanat/kprint
  kprint help
  ```

- [Docker](https://github.com/kjanat/kprint/pkgs/container/kprint), for Linux amd64, arm64 and ppc64le:

  ```sh
  docker run --rm -v "$PWD:/work" ghcr.io/kjanat/kprint fmt
  ```

For binaries and source, see the [GitHub releases](https://github.com/kjanat/kprint/releases).

FreeBSD builds target amd64 on FreeBSD 14.4 or newer. Wasm plugins and built-in
`exec` formatting are supported. Process plugins must publish a
`freebsd-x86_64` entry in their manifest; Linux binaries are not used as a fallback.

## Editor Extensions

These are dprint's, and call `dprint`, which every method above installs besides `kprint` except Cargo.

- [Visual Studio Code](https://marketplace.visualstudio.com/items?itemName=dprint.dprint)
- [IntelliJ](https://plugins.jetbrains.com/plugin/18192-dprint) - Thanks to the developers at [Canva](https://canva.com)
- Neovim with [nvim-lspconfig](https://github.com/neovim/nvim-lspconfig/blob/master/doc/server_configurations.md#dprint)
- The `kprint lsp` subcommand provides code formatting over the language server protocol. This can be used to format in other editors. <!-- rumdl-disable-line line-length -->

Next step: [Setup](/setup)
