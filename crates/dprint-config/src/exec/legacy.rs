//! Compatibility with references to the external exec plugin,
//! dprint-plugin-exec.
//!
//! Configuration files reference the exec plugin by its releases. dprint
//! serves the references to the release listed here with its built-in exec,
//! and recognizes references to any release. Nothing else about the external
//! plugin is the built-in's identity: the built-in has its own name, is
//! versioned with dprint, and has its own cache revision (see `super::BUILT_IN`).

use crate::plugins::PluginSourceReference;
use crate::utils::PathSource;

/// The external exec plugin's name.
pub const PLUGIN_NAME: &str = "dprint-plugin-exec";

/// The release whose configuration dprint reads, both for the references the
/// built-in exec serves and to check commands from a remote configuration
/// file (see `configuration/remote_exec.rs`).
pub const COMMANDS_RELEASE: &str = SERVED_RELEASES[0].version;

/// The releases whose references the built-in exec serves. A reference to
/// another release, or to no specific release, gets that release, run as a
/// process plugin. A release is only added here together with tests that the
/// built-in exec handles its configuration the same way.
const SERVED_RELEASES: &[ServedRelease] = &[ServedRelease {
  version: "0.7.3",
  plugin_file_checksum: "a7898d5f1897e77bff474cec3d948c3ec3a7f455e32de2cc60c8adb9a5dd24aa",
  npm_tarball_checksum: "704701df449dd7e942a71144773778ac529d68c2e4657bfc236d393b898b9a67",
}];

/// A release of the exec plugin whose references the built-in exec serves.
///
/// A reference may pin a release's checksum. It's only served built in when
/// the checksum is that release's, so a pin keeps meaning that release.
/// Another checksum gets the plugin it names, which then fails its checksum
/// check like any plugin.
struct ServedRelease {
  version: &'static str,
  /// Of its process plugin file (plugin.json), as a URL reference pins it.
  plugin_file_checksum: &'static str,
  /// Of its npm package's tarball, as an npm reference pins it.
  npm_tarball_checksum: &'static str,
}

/// The file of the exec process plugin in its npm package.
const NPM_FILE: &str = "plugin.json";

/// The release the built-in exec serves the reference as, or why it doesn't.
pub fn served_release(reference: &PluginSourceReference) -> Result<&'static str, String> {
  let served_versions = || SERVED_RELEASES.iter().map(|release| release.version).collect::<Vec<_>>().join(", ");
  let release = exec_plugin_reference_version(reference)
    .and_then(|version| SERVED_RELEASES.iter().find(|release| release.version == version))
    .ok_or_else(|| format!("the built-in exec only serves references to {} {}.", PLUGIN_NAME, served_versions()))?;
  let expected_checksum = match &reference.path_source {
    PathSource::Npm(_) => release.npm_tarball_checksum,
    PathSource::Remote(_) | PathSource::Local(_) => release.plugin_file_checksum,
  };
  match &reference.checksum {
    Some(checksum) if checksum != expected_checksum => Err(format!(
      "its checksum isn't the one of {} {} ({}), so it's checked against the plugin it names.",
      PLUGIN_NAME, release.version, expected_checksum
    )),
    _ => Ok(release.version),
  }
}

/// Whether the reference is to a release of the exec plugin whose commands
/// dprint knows how to read (ex. which program a command runs). These are
/// the releases it serves built in, also when one runs as the process plugin.
pub fn knows_exec_plugin_commands(reference: &PluginSourceReference) -> bool {
  served_release(reference).is_ok()
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
    // ex. npm:@dprint/exec@0.7.3/plugin.json, or without a version for the one
    // installed in node_modules. Only the exec process plugin's file
    // (plugin.json) can be served built in: any other file in the package is
    // resolved as asked, so a wrong path is reported rather than replaced.
    PathSource::Npm(npm) => (npm.specifier.name == "@dprint/exec").then(|| npm.specifier.version.as_deref().filter(|_| npm.specifier.path == NPM_FILE)),
    PathSource::Remote(remote) => {
      let path = remote.url.path();
      match remote.url.host_str() {
        Some("plugins.dprint.dev") => {
          if let Some(version) = path.strip_prefix("/exec-").and_then(|rest| rest.strip_suffix(".json")) {
            // ex. https://plugins.dprint.dev/exec-0.5.0.json
            Some(Some(version))
          } else {
            // ex. https://plugins.dprint.dev/dprint/dprint-plugin-exec/0.7.3/plugin.json,
            // or latest.json for the latest release
            path.strip_prefix("/dprint/dprint-plugin-exec/").map(|rest| rest.strip_suffix("/plugin.json"))
          }
        }
        Some("github.com") => {
          // ex. https://github.com/dprint/dprint-plugin-exec/releases/download/0.5.0/plugin.json,
          // or releases/latest/download/plugin.json for the latest release
          let rest = path.strip_prefix("/dprint/dprint-plugin-exec/releases/")?;
          Some(rest.strip_prefix("download/").and_then(|rest| rest.strip_suffix("/plugin.json")))
        }
        _ => None,
      }
    }
    // a local exec plugin is most likely a build being worked on, so run it
    PathSource::Local(_) => None,
  }
}
