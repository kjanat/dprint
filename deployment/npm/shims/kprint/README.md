# @kprint/kprint

[kprint](https://dprint.kjanat.dev) is a pluggable and configurable code
formatter written in Rust, forked from dprint. It has formatters built in (ex.
exec) and runs dprint's plugins.

## Install

```sh
npm install --global @kprint/kprint
```

This installs the `kprint` command, also as `dprint` for tools
that call that. npm only downloads the binary for your platform, from its
`@kprint/kprint-<platform>` package.

## Use

```sh
kprint init   # create a configuration file
kprint fmt    # format the files
kprint check  # check they're formatted
```

Or in a project, as a dev dependency:

```sh
npm install --save-dev @kprint/kprint
npx kprint fmt
```

[@kjanat/fmt](https://www.npmjs.com/package/@kjanat/fmt) runs `kprint fmt` as `fmt`.

See [dprint.kjanat.dev](https://dprint.kjanat.dev) for the configuration and the
plugins, and [the repository](https://github.com/kjanat/dprint) for its source.
