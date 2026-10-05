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

The CLI has version 0.7.3 of this plugin built in. When a config references that version (ex. `npm:@dprint/exec@0.7.3/plugin.json` or `https://plugins.dprint.dev/exec-0.7.3.json`), the CLI runs it in its own process rather than downloading it, so it works on every platform the CLI does (ex. FreeBSD). A reference to another version, or to none in particular (ex. `npm:@dprint/exec` resolved from `node_modules`), gets the plugin it asks for, downloaded and run as a separate process like before. So does a reference with a checksum other than 0.7.3's (of its npm package for an `npm:` reference, or of its `plugin.json` for a url), which then fails the checksum check like any plugin, so a checksum keeps pinning what it pins. To run the separate plugin process for 0.7.3 too, set `DPRINT_BUILTIN_EXEC=0`.

Compared to the downloaded plugin, the built-in one:

- Kills a command that times out or is cancelled instead of leaving it running.
- Supports a `"setupTimeout"` option (default: `300` seconds) after which a `setupCommand` is killed.
- On Windows, finds commands through the `PATHEXT` extensions like the Windows shell does, so `.cmd` and `.bat` commands (ex. ones installed with `npm install -g`) work.
- Passes `{{file_path}}`, `{{line_width}}`, `{{use_tabs}}`, `{{indent_width}}`, `{{cwd}}` and `{{timeout}}` to commands as they are, where the plugin escaped them for HTML (ex. a `&` in a file path became `&amp;`). They may also be written `{{ file_path }}` or `{{{file_path}}}`, and `\{{` is a literal `{{`. Anything else in `{{` and `}}` (ex. `{{filePath}}`) is reported as a configuration error, rather than failing each file.
- Has its configuration schema built in, so [`dprint schema`](/config#schema) and the language server describe all of the options above without downloading it.

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

This goes by what version 0.7.3 of the plugin has, so of a remote configuration's other exec properties, at the root or in an override, only `"lineWidth"`, `"indentWidth"`, `"useTabs"`, `"cacheKey"`, `"timeout"` and `"setupTimeout"` are used without `"playWithFire": true`. Any other might decide what runs (ex. one a later version adds), and so might a property of a command that 0.7.3 doesn't have, so such a command only runs with `"playWithFire": true` too.

A nested configuration file with `"inherit": true` inherits the remote commands its ancestor allowed, unless it specifies `"playWithFire"` itself: then it only inherits the ones that allows.

## Install, Setup, and Configuration

```shellsession
dprint add exec
# or install from npm
dprint add npm:@dprint/exec
```

See further setup and configuration instructions at [https://github.com/dprint/dprint-plugin-exec/](https://github.com/dprint/dprint-plugin-exec/).
