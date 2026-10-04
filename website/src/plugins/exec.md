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

The CLI has this plugin built in. When a config references it, the CLI runs it in its own process rather than downloading it, so it works on every platform the CLI does (ex. FreeBSD). To download and run the separate plugin process instead, set `DPRINT_BUILTIN_EXEC=0`.

Compared to the downloaded plugin, the built-in one kills a command that times out or is cancelled instead of leaving it running, and supports a `"setupTimeout"` option (default: `300` seconds) after which a `setupCommand` is killed.

## Install, Setup, and Configuration

```shellsession
dprint add exec
# or install from npm
dprint add npm:@dprint/exec
```

See further setup and configuration instructions at [https://github.com/dprint/dprint-plugin-exec/](https://github.com/dprint/dprint-plugin-exec/).
