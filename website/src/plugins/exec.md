---
title: Exec Plugin
description: Documentation on the Exec code formatting plugin for dprint.
layout: layouts/documentation.njk
---

# Exec Plugin

Plugin that formats code via mostly any formatting CLI found on the host machine.

<div class="message is-warning">
  <div class="message-body">
    This plugin runs the commands you configure, which are not sandboxed (unlike Wasm plugins).
  </div>
</div>

The CLI has version 0.7.3 of this plugin built in. When a config references that version (ex. `npm:@dprint/exec@0.7.3/plugin.json` or `https://plugins.dprint.dev/exec-0.7.3.json`), the CLI runs it in its own process rather than downloading it, so it works on every platform the CLI does (ex. FreeBSD). A reference to another version, or to none in particular (ex. `npm:@dprint/exec` resolved from `node_modules`), gets the plugin it asks for, downloaded and run as a separate process like before. To run the separate plugin process for 0.7.3 too, set `DPRINT_BUILTIN_EXEC=0`.

Compared to the downloaded plugin, the built-in one:

- Kills a command that times out or is cancelled instead of leaving it running.
- Supports a `"setupTimeout"` option (default: `300` seconds) after which a `setupCommand` is killed.
- On Windows, finds commands through the `PATHEXT` extensions like the Windows shell does, so `.cmd` and `.bat` commands (ex. ones installed with `npm install -g`) work.

### Commands in remote configuration

Commands run programs, so like process plugins and `"includes"`, the exec commands and plugin of a remote configuration file (ex. one `"extends"` refers to by url) are ignored unless a local configuration file allows them with `"playWithFire"` in its exec configuration:

```jsonc
{
  "extends": "https://example.com/dprint.json",
  "exec": {
    // run any program the remote configuration's commands specify
    "playWithFire": true
    // or only commands (and setup commands) that run these programs
    // "playWithFire": ["tombi", "rustfmt"]
  }
}
```

This includes the commands in its `"overrides"`. Its `"cwd"` (the directory commands run in, which decides what a command with a relative path runs, including your local commands) is only used with `"playWithFire": true`. A remote configuration can't allow itself. When none of its commands are allowed, its exec plugin is ignored too.

A nested configuration file with `"inherit": true` inherits the remote commands its ancestor allowed, unless it specifies `"playWithFire"` itself: then it only inherits the ones that allows.

## Install, Setup, and Configuration

```shellsession
dprint add exec
# or install from npm
dprint add npm:@dprint/exec
```

See further setup and configuration instructions at [https://github.com/dprint/dprint-plugin-exec/](https://github.com/dprint/dprint-plugin-exec/).
