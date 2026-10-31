//! Compatibility with references to the external exec plugin,
//! dprint-plugin-exec.
//!
//! Configuration files reference the exec plugin by its releases. dprint
//! serves every one of them with its built-in exec: whatever release or
//! checksum a reference names, nothing is downloaded, and a configuration's
//! `exec` table works without any reference at all. Nothing else about the
//! external plugin is the built-in's identity: the built-in has its own name,
//! is versioned with dprint, and has its own cache revision (see
//! `super::BUILT_IN`).

use crate::plugins::PluginSourceReference;
use crate::utils::PathSource;

/// The external exec plugin's name.
pub const PLUGIN_NAME: &str = "dprint-plugin-exec";

/// The release whose configuration the built-in exec reads, which is also how
/// commands from a remote configuration file are checked (see
/// `configuration/remote_exec.rs`).
pub const COMMANDS_RELEASE: &str = "0.7.3";

/// Whether the reference is to a release of the exec plugin whose commands
/// dprint knows how to read (ex. which program a command runs) when it runs
/// as the process plugin (`DPRINT_BUILTIN_EXEC=0`): only `COMMANDS_RELEASE`.
pub fn knows_exec_plugin_commands(reference: &PluginSourceReference) -> bool {
  exec_plugin_reference_version(reference) == Some(COMMANDS_RELEASE)
}

/// Whether the reference is to the exec plugin, in any release.
pub fn is_exec_plugin_reference(reference: &PluginSourceReference) -> bool {
  exec_plugin_reference(reference).is_some()
}

/// The exec plugin release the reference asks for, when it's to the exec
/// plugin and names one.
fn exec_plugin_reference_version(reference: &PluginSourceReference) -> Option<&str> {
  exec_plugin_reference(reference).flatten()
}

/// `Some` when the reference is to the exec plugin, with the release it asks
/// for when it names one.
fn exec_plugin_reference(reference: &PluginSourceReference) -> Option<Option<&str>> {
  match &reference.path_source {
    // ex. npm:@dprint/exec@0.7.3/plugin.json, npm:@dprint/exec@0.7.3, or
    // without a version for the one installed in node_modules
    PathSource::Npm(npm) => (npm.specifier.name == "@dprint/exec").then_some(npm.specifier.version.as_deref()),
    PathSource::Remote(remote) => {
      let path = remote.url.path();
      match remote.url.host_str() {
        Some("plugins.dprint.dev") => {
          // also under /dprint/, ex. https://plugins.dprint.dev/dprint/exec-0.7.3.json
          let path = path.strip_prefix("/dprint").filter(|rest| rest.starts_with('/')).unwrap_or(path);
          if let Some(version) = path.strip_prefix("/exec-").and_then(|rest| rest.strip_suffix(".json")) {
            // ex. https://plugins.dprint.dev/exec-0.5.0.json
            Some(Some(version))
          } else {
            // ex. https://plugins.dprint.dev/dprint/dprint-plugin-exec/0.7.3/plugin.json,
            // or the latest.json of the latest release, also under /dprint/exec/
            ["/dprint-plugin-exec/", "/exec/"]
              .into_iter()
              .find_map(|prefix| path.strip_prefix(prefix))
              .map(|rest| rest.strip_suffix("/plugin.json"))
          }
        }
        Some("github.com") => {
          // ex. https://github.com/dprint/dprint-plugin-exec/releases/download/0.5.0/plugin.json,
          // or releases/latest/download/plugin.json for the latest release
          let rest = path.strip_prefix("/dprint/dprint-plugin-exec/releases/")?;
          Some(rest.strip_prefix("download/").and_then(|rest| rest.strip_suffix("/plugin.json")))
        }
        // the npm package from a CDN, ex.
        // https://cdn.jsdelivr.net/npm/@dprint/exec@0.7.3/plugin.json or
        // https://unpkg.com/@dprint/exec@0.7.3/plugin.json
        Some("cdn.jsdelivr.net") => path.strip_prefix("/npm/").and_then(npm_package_version),
        Some("unpkg.com") => path.strip_prefix('/').and_then(npm_package_version),
        _ => None,
      }
    }
    // a local exec plugin is most likely a build being worked on, so run it
    PathSource::Local(_) => None,
  }
}

/// For a path in the exec plugin's npm package on a CDN (ex.
/// `@dprint/exec@0.7.3/plugin.json`), `Some` with the release it names.
fn npm_package_version(path: &str) -> Option<Option<&str>> {
  let rest = path.strip_prefix("@dprint/exec")?;
  if rest.is_empty() || rest.starts_with('/') {
    return Some(None);
  }
  let version = rest.strip_prefix('@')?;
  Some(version.split('/').next().filter(|version| !version.is_empty()))
}
