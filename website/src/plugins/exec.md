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

The CLI has exec built in, and formats every reference to this plugin with it rather than downloading anything: any release, the latest one or none in particular, with any checksum, written any way (ex. `npm:@dprint/exec@0.7.3/plugin.json`, `npm:@dprint/exec`, `https://plugins.dprint.dev/exec-0.5.0.json`, `https://plugins.dprint.dev/dprint/dprint-plugin-exec/latest.json`, its GitHub releases or the npm package on jsDelivr or unpkg). So it works on every platform the CLI does (ex. FreeBSD), and `dprint config update` leaves such references alone. The built-in reads the configuration of release 0.7.3. To run the separate plugin process a reference names instead, set `DPRINT_BUILTIN_EXEC=0`.

An `exec` section in your local configuration activates the built-in formatter directly. No entry in `plugins` or download is required:

```toml
[exec]
cwd = "${configDir}"

[[exec.commands]]
command = "gofmt -s"
exts    = ["go"]
```

An explicit exec plugin reference still selects that plugin and its position in the formatter chain. Without a reference, built-in exec runs after the listed plugins. `DPRINT_BUILTIN_EXEC=0` only disables substitution of legacy plugin references; it does not disable a directly configured built-in formatter. Commands inherited from remote configurations still require local `playWithFire` permission as described below.

Compared to the downloaded plugin, the built-in one:

- Kills a command that times out or is cancelled instead of leaving it running.
- Keeps a command's output within ten times the file's size plus 1 MiB. More than that can't be the formatted file, so it fails the file right away rather than being kept in memory, and the command is killed. Of what a command writes to stderr, the last 64 KiB are kept for the error when it fails.
- Supports a `"setupTimeout"` option (default: `300` seconds): how long a file waits for its `setupCommand`, which is killed once no file waits for it.
- Runs a `setupCommand` once for the files that need it, also when formatting them in parallel. Each file waits for it until its own `"setupTimeout"` or until its formatting is cancelled, while it keeps running for the files that still wait. Its success applies to every file after it. A failure (it couldn't start, it exited unsuccessfully, or the last file waiting for it timed out) applies to every file for 10 seconds rather than being retried for each one, after which the next file runs it again, waiting twice as long after each failure in a row. So a long running process such as an editor's language server recovers from a failure that was transient. After 5 failures in a row (about two and a half minutes), the failure is final until the plugin restarts. A file whose formatting is cancelled doesn't count as a failure.
- On Windows, finds commands through the `PATHEXT` extensions like the Windows shell does, so `.cmd` and `.bat` commands (ex. ones installed with `npm install -g`) work.
- Passes `{{file_path}}`, `{{line_width}}`, `{{use_tabs}}`, `{{indent_width}}`, `{{cwd}}` and `{{timeout}}` to commands as they are, where the plugin escaped them for HTML (ex. a `&` in a file path became `&amp;`). They may also be written `{{ file_path }}` or `{{{file_path}}}`, and `\{{` is a literal `{{`. Anything else in `{{` and `}}` (ex. `{{filePath}}`) is reported as a configuration error, rather than failing each file.
- Has its configuration schema built in, so [`dprint schema`](/config#schema) and the language server describe all of the options above without downloading it.
- Takes a `"command"` and `"setupCommand"` as an array or an object too (see [Command forms](#command-forms)).
- Formats the files a command's `"associations"` glob matches, relative to the configuration file's directory like dprint's own `"associations"`, after the formatter that owns them (ex. markdown for `.md`), or alone when none does. The plugin also needed the glob in its own `"associations"`, which takes the files from their formatter.

The built-in exec is part of the CLI, not a release of this plugin. It's named `exec` (ex. in `dprint help` and in configuration diagnostics), has the CLI's version, and is updated by upgrading the CLI: `dprint config update` and `dprint add exec` leave a reference it serves as it is. When an upgrade changes how it formats a file, the incremental cache forgets the files it formatted, and only then.

### Command forms

`"command"` and `"setupCommand"` each take one of three forms:

```toml
# a command line: parts are separated by spaces, and a part in double quotes may contain spaces
[[exec.commands]]
command = "rustfmt --emit stdout"
exts    = ["rs"]

# the program and its arguments
[[exec.commands]]
command = ["gofmt", "-s"]
exts    = ["go"]

# the program, its arguments and environment variables
[[exec.commands]]
command = { executable = "/opt/My Tools/taplo", args = ["fmt", "-"], env = { RUST_LOG = "warn" } }
exts    = ["toml"]
```

- An array or `"args"` passes each argument as it is, through no shell. Empty arguments, spaces and quotes stay.
- `"env"` sets variables over the ones dprint runs with. The program is looked up on the `"PATH"` it sets.
- A command's arguments and `"env"` values may use `{{file_path}}` and the other variables. A setup command's arguments and `"env"` values are used as they are.

### Served commands

A command with `"serve": true` starts once and formats every file it matches. This suits tools that are slow to start, such as a .NET or JVM formatter:

```toml
[[exec.commands]]
command = "dotnet tools/roslyn-format/bin/RoslynFormat.dll --serve"
exts    = ["cs"]
serve   = true
workers = 2
```

dprint writes each file to the command's stdin as one line of JSON:

```json
{ "path": "src/Program.cs", "text": "...", "lineWidth": 120, "indentWidth": 4, "useTabs": false }
```

The command answers each line with one line on stdout:

- `{"text": "..."}` with the formatted text.
- `{"error": "..."}` when it can't format the file, ex. for a syntax error. That file fails, and the command goes on with the next one.

The rest of how it runs:

- `"workers"` (default: `1`) sets how many processes of the command format files at once.
- Its arguments and `"env"` values can't use `{{file_path}}`, as one process formats every file. Each request carries the file's path and settings, and the other variables are available.
- `"timeout"` applies to each answer. A process that exits, answers with something else, or takes longer fails the file it was formatting and is killed. The next file starts a new process.
- A process that formats no file for 5 minutes ends, and every process ends with dprint.
- Files travel as JSON text, so a file that isn't UTF-8 fails.

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

This goes by what version 0.7.3 of the plugin has, so of a remote configuration's other exec properties, at the root or in an override, only `"lineWidth"`, `"indentWidth"`, `"useTabs"` and `"cacheKey"` are used without `"playWithFire": true`. `"timeout"` and `"setupTimeout"` set how long a command may run on your machine, so a remote command runs within what your local configuration allows unless it specifies `"playWithFire": true`. Any other property might decide what runs (ex. one a later version adds), and so might a property of a command that 0.7.3 doesn't have (ex. `"serve"`) or a `"command"` or `"setupCommand"` given as an array or object, so such a command only runs with `"playWithFire": true` too. Likewise, the program a command runs is read the way 0.7.3 reads it, so a list of programs only applies when the exec plugin that runs the commands is 0.7.3 (or the remote configuration's): with another version, or one installed in `node_modules` without a version, remote commands only run with `"playWithFire": true`. A command's own `"cwd"` decides what a program it runs by a relative path (ex. `./formatter`) is, so a remote command that sets both only runs with `"playWithFire": true` too; one that runs a program by name (ex. `cargo fmt`, found on the PATH) or by an absolute path may set its `"cwd"`, unless the PATH has a relative entry (ex. `.`), which is searched in that directory: then a command that sets its `"cwd"` and runs a program by name only runs with `"playWithFire": true` too. On Windows, a remote command whose program is a batch file (`.cmd` or `.bat`, ex. a shim `npm install -g` makes) runs through `cmd.exe`, which arguments can't be passed to safely, so it only runs with `"playWithFire": true` as well. A remote command with `"cacheKeyFiles"` has the plugin read those files on this machine to key its cache, which isn't running a program, so it only runs with `"playWithFire": true` as well.

A nested configuration file with `"inherit": true` inherits the remote commands its ancestor allowed, unless it specifies `"playWithFire"` itself: then it only inherits the ones that allows, and what its ancestor's local configuration files specified in their place (ex. their own `"commands"` or `"cwd"`).

## Install, Setup, and Configuration

```shellsession
dprint add exec
# or install from npm
dprint add npm:@dprint/exec
```

See further setup and configuration instructions at [https://github.com/dprint/dprint-plugin-exec/](https://github.com/dprint/dprint-plugin-exec/).
