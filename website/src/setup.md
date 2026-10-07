---
title: Setup
description: Documentation on setting up dprint to format a collection of code.
layout: layouts/documentation.njk
---

# Setup dprint

After [installing](/install), the main part of getting setup is to create a _dprint.json_/_dprint.jsonc_, or hidden _.dprint.json_/_.dprint.jsonc_ file in your project.

This file will outline:

1. The plugins to use.
2. The configuration to use for formatting files.
3. Which files to include and exclude from formatting.

## Quick Setup

Using the `dprint init` command is a quick way to get setup formatting your project.

Open a terminal in the root directory of your project and run the following command:

```sh
dprint init
```

This will create a _dprint.json_ file in the current working directory. If you are connected to the internet, it will prompt you to select from the latest plugins, pre-selecting the ones that match the files found in the current directory. Use the spacebar to toggle a plugin, type to filter the list, and press enter when finished.

### Non-interactive init

Pass the `--yes` or `-y` flag to skip the prompt and accept the plugins selected based on the files in the current directory. This is useful in scripts:

```sh
dprint init --yes
```

The prompt is also skipped automatically when there is no interactive terminal.

## Manual Setup

Create a _dprint.json_/_dprint.jsonc_ or hidden _.dprint.json_/_.dprint.jsonc_ file in the root directory of the project and read the [configuration documentation](/config).

## Hidden Config File

The dprint CLI supports a default hidden configuration at _.dprint.json_ or _.dprint.jsonc_.

## Custom Config File Location

It is recommended to use an auto-discoverable dprint configuration file name (ex. _dprint.json_) as the location of your configuration file because it will be automatically picked up by the CLI and editor plugins. If you place it in another other location then it will need to be manually specified using the `--config <path>` or `-c <path>` flag whenever you run a command.

### `dprint init` with custom config file location

You may specify a custom path for the creation of a configuration file via `dprint init` by specifying it with the `-c` or `--config` flag.

```sh
dprint init --config .dprint.jsonc
dprint init --config path/to/dprint.json
```

## Global Config File

See [global configuration](/global-config)

## Custom Cache Directory

By default, dprint stores information in the current system user's cache directory (`~/.cache/dprint` on Linux, `~/Library/Caches/dprint` on Mac, and `%LOCALAPPDATA%/dprint` on Windows) such as cached plugins and incremental formatting information. If you would like to store the cache in a custom location, then specify a `DPRINT_CACHE_DIR` environment variable. Note that this directory may be periodically deleted by the CLI, so if you set it please make sure it's set correctly and you're ok with the custom directory being deleted.

## Proxy

You may specify a proxy for dprint to use when downloading plugins or configuration files by setting the `HTTPS_PROXY`/`https_proxy` and `HTTP_PROXY`/`http_proxy` environment variables.

Additionally, the `NO_PROXY`/`no_proxy` environment variable can be set, which is a comma-separated list of hosts which should not use the proxy.

## TLS Certificates

dprint downloads plugins via HTTPS. In some cases you may wish to configure this. This is possible via the following environment variables:

- `DPRINT_CERT` - Load certificate authority from PEM encoded file.
- `DPRINT_TLS_CA_STORE` - Comma-separated list of order dependent certificate stores.
  - Possible values: `mozilla` and `system`
  - Defaults to `mozilla,system`

Requires dprint >= 0.46.0

### Unsafely ignoring certificates

You can unsafely ignore all or some TLS certificates via the `DPRINT_IGNORE_CERTS` environment variable:

- `DPRINT_IGNORE_CERTS=1` - Ignore all TLS certificates.
- `DPRINT_IGNORE_CERTS=dprint.dev,localhost,[::],127.0.0.1` - Ignore certs from the specified hosts.

This is very unsafe to do and not recommended. A warning will be displayed on first download when this is done.

## Limiting Parallelism

By default, dprint only runs for a short period of time and so it will try to take advantage of as many CPU cores as it can. This might be an issue in some scenarios, and so you can limit the amount of parallelism by setting the `DPRINT_MAX_THREADS` environment variable in version 0.32 and up (ex. `DPRINT_MAX_THREADS=4`).

Finding the files to format doesn't use these threads. dprint scans directories with [tree-fucker](https://github.com/kjanat/tree-fucker), which limits how much file system work runs at once across the whole process.

## Stalled Plugin Setup

The first time dprint uses a Wasm plugin, it compiles the plugin to native code and caches the result. dprint does this in a separate process that it watches while it works. When that process crashes, stops making progress (its CPU time stops increasing for 5 seconds, which is also what it looks like when it gets no CPU time at all), or spends far longer on a step than the step needs, dprint kills it and tries again, up to three times. A compile that keeps the CPU busy for too long, or that fails twice, is retried without optimizations. That avoids slow paths in the optimizer, but the plugin may format more slowly until you run `dprint clear-cache`. Together these processes use at most `DPRINT_MAX_THREADS` threads, and each one exits as soon as the dprint process that started it does.

Each attempt also has a time limit however much CPU time it gets, of twice the CPU time all its steps may use, and the compile as a whole (waiting for a free compile process, the attempts and the retries) has twice that, after which dprint gives up on the plugin. The limits grow with the plugin's size, up to 10 minutes in all: the largest plugins compile in about 13 seconds on one thread, so a compile that takes longer than that is stuck rather than large. If a module genuinely needs longer, set `DPRINT_WASM_COMPILE_TIMEOUT` to the number of seconds it may take in all (ex. `DPRINT_WASM_COMPILE_TIMEOUT=1800`), which also works to give it less. A compile is stopped as soon as nothing waits for it anymore.

When dprint can't start that process (ex. it can't find its own executable, or the operating system refuses to [contain](#child-processes) the process), compiling the plugin fails rather than happening in the dprint process, where nothing could stop it.

Process plugins are watched the same way while they start: one that hasn't reported its plugin info within 20 seconds is killed and started once more.

To compile Wasm plugins inside the dprint process instead, without this supervision, set `DPRINT_WASM_COMPILE_WORKER=0`. A compile that hangs then hangs dprint.

## Child Processes

dprint owns the processes it starts (process plugins, Wasm compile processes, and the exec plugin's commands) together with the processes those start, such as the `node` process behind a command installed with `npm install -g`. On Windows each is in a job object, which no process in it can leave. On Linux and macOS each is in a process group of its own, which a process can leave on purpose (`setsid`, ex. a formatter's server started with Node's `detached: true`, which is meant to outlive the command that started it): such a process isn't owned, and outlives everything below.

What's owned is killed once dprint is done with it, when a command times out or formatting is cancelled, and when dprint is interrupted or terminated (ex. Ctrl+C).

When dprint itself is killed (ex. `kill -9`), Windows kills all of them too. On Linux, the processes dprint started directly are killed, except the exec plugin's formatting commands, which usually end on their own once their input and output close. The processes those started can outlive it on Linux and macOS.

Next step: [Configuration](/config)
