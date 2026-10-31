# @kjanat/fmt

`fmt` (or `kfmt`, the same command where `fmt` is taken) runs `kprint fmt` of
[@kprint/kprint](https://www.npmjs.com/package/@kprint/kprint) with the arguments it's given.

```sh
npx @kjanat/fmt            # format the files of the configuration
npx @kjanat/fmt src/a.ts   # or only these
```

Without a configuration, which is no `--config` and no `dprint.json`,
`dprint.jsonc`, `dprint.toml` (or a dotted one) in the directory or its
ancestors, it formats with [kjanat's](https://raw.githubusercontent.com/kjanat/kjanat/refs/heads/master/configs/dprint.remote.json).

Install @kprint/kprint for the other commands (`kprint check`, `kprint init`, ...).
