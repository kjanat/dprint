use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;

use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;

use dprint_core::plugins::PluginInfo;

use super::implementations::WASM_CACHE_VERSION;
use crate::environment::Environment;
use crate::utils::FastInsecureHasher;
use crate::utils::PluginKind;
use std::hash::Hasher;

/// Bumped when the on-disk cache layout or meta format changes in a way that
/// should invalidate existing entries. Folded into each entry's signature so a
/// bump simply orphans old entries (they stay on disk until `clear-cache`)
/// rather than busting the whole cache.
const PLUGIN_CACHE_SCHEMA_VERSION: usize = 11;

/// Size + modification time of a local file, captured at setup. A cache hit
/// requires every stamp to still match (cheap stat, no read/hash).
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LocalStamp {
  pub path: String,
  pub len: u64,
  pub modified_ms: u64,
}

/// Sidecar describing a single cached plugin. Lives next to its artifact at
/// `plugins/<hash>.json`, where `<hash>` is derived from the plugin's source
/// (see [`entry_hash`]). Replaces the old global `plugin-cache-manifest.json`
/// so changing one plugin never rewrites state for the others.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PluginCacheMeta {
  /// The cache key this entry was stored under (e.g. `remote:<url>`). Compared
  /// against the looked-up key to reject the astronomically unlikely event of
  /// two distinct sources hashing to the same filename.
  pub source: String,
  /// Identifies the toolchain/arch the artifact was built for. Correctness is
  /// already enforced by folding this into the entry's hash (a change produces a
  /// fresh filename); this stored copy isn't read today, but is retained so a
  /// future cleanup pass could identify entries left by an old toolchain, and to
  /// keep the sidecar self-describing.
  pub signature: String,
  pub plugin_kind: PluginKind,
  /// Created time in *seconds* since epoch. Recorded for human inspection of the
  /// sidecar only — nothing reads it today (kept in case a future cleanup pass
  /// wants an age signal).
  pub created_time: u64,
  pub info: PluginInfo,
  /// Executable path relative to the plugin's extract dir. Process plugins only.
  #[serde(skip_serializing_if = "Option::is_none", default)]
  pub executable_sub_path: Option<String>,
  /// Modification stamps for the local source file(s). Present only for local
  /// sources, where edits must invalidate the cache; absent for content-pinned
  /// remote and versioned-npm sources, whose mere presence is a cache hit.
  #[serde(skip_serializing_if = "Option::is_none", default)]
  pub local_stamps: Option<Vec<LocalStamp>>,
  /// SHA-256 of the plugin file it was set up from, which identifies the
  /// build (see [`build_id`]) even when a url without a checksum serves a
  /// different one under the same name and version. Absent in entries set up
  /// before it was recorded.
  #[serde(skip_serializing_if = "Option::is_none", default)]
  pub source_checksum: Option<String>,
  /// The file name of the module of the Wasm plugin build this one replaced,
  /// which is kept so a process that read the entry before (ex. `dprint lsp`)
  /// still loads the build it read about. It's removed once the next build
  /// replaces this one.
  #[serde(skip_serializing_if = "Option::is_none", default)]
  pub previous_module_file_name: Option<String>,
}

impl PluginCacheMeta {
  /// The path of the module of the build this one replaced (see
  /// `previous_module_file_name`). It's only ever a file in the plugins
  /// directory, whatever the entry says.
  pub fn previous_module_file_path(&self, environment: &impl Environment) -> Option<PathBuf> {
    let file_name = self.previous_module_file_name.as_deref()?;
    let is_file_name = Path::new(file_name).file_name().is_some_and(|name| name == file_name);
    is_file_name.then(|| plugins_dir(environment).join(file_name))
  }

  /// The on-disk file path of this entry's artifact: the module for wasm
  /// plugins, or the executable within the extract dir for process plugins.
  pub fn artifact_file_path(&self, hash: &str, environment: &impl Environment) -> PathBuf {
    match self.plugin_kind {
      PluginKind::Wasm => wasm_module_path(hash, self.source_checksum.as_deref(), environment),
      PluginKind::Process => {
        let sub_path = self.executable_sub_path.as_deref().unwrap_or_default();
        process_dir_path(hash, environment).join(sub_path)
      }
    }
  }
}

/// The signature folded into every cache key + stored in each entry. A change
/// here means existing artifacts are no longer valid for this machine/build, so
/// they get a fresh hash; the now-unreferenced old files stay on disk until
/// `clear-cache`.
pub fn current_signature(environment: &impl Environment) -> String {
  // `wasm_cache_key` already covers cpu arch, rustc version and wasmtime's
  // precompile compatibility (cpu features etc.); `os` distinguishes
  // musl/glibc/etc for process plugins. Process plugins technically don't care
  // about the wasm bits, but folding everything in keeps the key kind-agnostic
  // (so we can hash before resolving an npm plugin's kind) at the cost of a
  // rare, cheap re-extract on a dprint upgrade and, in a cache directory shared
  // across machines with different cpu features, an extracted copy per variant.
  format!(
    "{}-{}-{}-{}",
    PLUGIN_CACHE_SCHEMA_VERSION,
    WASM_CACHE_VERSION,
    environment.wasm_cache_key(),
    environment.os(),
  )
}

/// Hashes a plugin's cache key (e.g. `remote:<url>`) together with the current
/// signature into the stable, opaque filename stem used for its sidecar and
/// artifact. Folding the signature in means different arches/toolchains get
/// distinct files and can coexist in a shared cache dir.
pub fn entry_hash(cache_key: &str, environment: &impl Environment) -> String {
  let mut hasher = FastInsecureHasher::default();
  hasher.write(current_signature(environment).as_bytes());
  hasher.write(&[0]);
  hasher.write(cache_key.as_bytes());
  format!("{:016x}", hasher.finish())
}

/// Reads the sidecar for `hash`. Returns `None` if it's missing or unparseable
/// (treated as a cache miss — the caller re-sets-up and overwrites).
pub fn read_meta(hash: &str, environment: &impl Environment) -> Option<PluginCacheMeta> {
  let text = environment.read_file(meta_path(hash, environment)).ok()?;
  serde_json::from_str(&text).ok()
}

pub fn write_meta(hash: &str, meta: &PluginCacheMeta, environment: &impl Environment) -> Result<()> {
  let serialized = serde_json::to_string(meta)?;
  Ok(environment.atomic_write_file_bytes(meta_path(hash, environment), serialized.as_bytes())?)
}

/// Removes an entry's sidecar and artifact(s). Kind-agnostic: deletes the meta
/// json, the wasm modules with their native code, and the process extract dir,
/// ignoring whichever don't exist.
pub fn remove_entry(hash: &str, environment: &impl Environment) {
  if let Some(meta) = read_meta(hash, environment)
    && meta.plugin_kind == PluginKind::Wasm
  {
    remove_wasm_module(&meta.artifact_file_path(hash, environment), environment);
    if let Some(path) = meta.previous_module_file_path(environment) {
      remove_wasm_module(&path, environment);
    }
  }
  let _ = environment.remove_file(meta_path(hash, environment));
  remove_wasm_module(&wasm_module_path(hash, None, environment), environment);
  let _ = environment.remove_file(resolutions_path(hash, environment));
  environment.try_remove_dir_all(process_dir_path(hash, environment));
}

/// Converts a modification time to milliseconds since the unix epoch for stable
/// serialization. Saturates to 0 for the (pre-epoch) edge case.
pub fn to_unix_millis(time: SystemTime) -> u64 {
  time.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub fn plugins_dir(environment: &impl Environment) -> PathBuf {
  environment.get_cache_dir().join("plugins")
}

/// Where a Wasm plugin's module is kept. Each build of the plugin (by the
/// checksum of the file it was set up from) has a file of its own, so a
/// module is never replaced by another build: a process that loads a plugin
/// later than it read the entry loads the build it read about, or finds it
/// gone. Entries set up before the checksum was recorded use the name
/// without it.
pub fn wasm_module_path(hash: &str, source_checksum: Option<&str>, environment: &impl Environment) -> PathBuf {
  let file_name = match source_checksum {
    Some(checksum) => format!("{hash}-{}.wasm", &checksum[..checksum.len().min(16)]),
    None => format!("{hash}.wasm"),
  };
  plugins_dir(environment).join(file_name)
}

/// Where the native code compiled from a Wasm plugin's module is kept: next
/// to the module, as a file of the same build. It's written the first time
/// the plugin formats.
pub fn native_module_path(module_path: &Path) -> PathBuf {
  module_path.with_extension("cwasm")
}

/// Where a Wasm plugin's formatting rate in the interpreter is kept: next
/// to the module, for the same build.
pub fn format_rate_path(module_path: &Path) -> PathBuf {
  module_path.with_extension("rate.json")
}

/// Removes a Wasm plugin's module, the native code compiled from it and its
/// formatting rate.
pub fn remove_wasm_module(module_path: &Path, environment: &impl Environment) {
  let _ = environment.remove_file(module_path);
  let _ = environment.remove_file(native_module_path(module_path));
  let _ = environment.remove_file(format_rate_path(module_path));
}

/// What the plugin resolved configurations to (see `PluginResolutionCache`).
pub fn resolutions_path(hash: &str, environment: &impl Environment) -> PathBuf {
  plugins_dir(environment).join(format!("{hash}.resolutions.json"))
}

/// Identifies the plugin set up for an entry, which changes whenever it's set
/// up again (ex. a local plugin that was rebuilt).
pub fn artifact_id(meta: &PluginCacheMeta) -> u64 {
  let mut hasher = FastInsecureHasher::default();
  hasher.write(serde_json::to_string(meta).unwrap_or_default().as_bytes());
  hasher.finish()
}

/// Identifies the build of the plugin set up for an entry (its source and the
/// checksum of its contents, local file stamps, toolchain and info). Unlike
/// [`artifact_id`], it stays the same
/// when the same plugin is compiled again, and only changes when it's a
/// different one (ex. a local plugin that was rebuilt).
pub fn build_id(meta: &PluginCacheMeta) -> u64 {
  let mut meta = meta.clone();
  meta.created_time = 0;
  meta.previous_module_file_name = None;
  artifact_id(&meta)
}

pub fn process_dir_path(hash: &str, environment: &impl Environment) -> PathBuf {
  plugins_dir(environment).join(hash)
}

fn meta_path(hash: &str, environment: &impl Environment) -> PathBuf {
  plugins_dir(environment).join(format!("{hash}.json"))
}

#[cfg(test)]
mod test {
  use super::*;
  use crate::environment::TestEnvironment;
  use pretty_assertions::assert_eq;

  fn make_meta(signature: &str) -> PluginCacheMeta {
    PluginCacheMeta {
      source: "remote:https://example.com/test.wasm".to_string(),
      signature: signature.to_string(),
      plugin_kind: PluginKind::Wasm,
      created_time: 123,
      info: PluginInfo {
        name: "test-plugin".to_string(),
        version: "0.1.0".to_string(),
        config_key: "test".to_string(),
        help_url: "help".to_string(),
        config_schema_url: "schema".to_string(),
        update_url: None,
      },
      executable_sub_path: None,
      local_stamps: None,
      source_checksum: None,
      previous_module_file_name: None,
    }
  }

  #[test]
  fn should_roundtrip_meta() {
    let environment = TestEnvironment::new();
    environment.mk_dir_all(plugins_dir(&environment)).unwrap();
    let meta = make_meta(&current_signature(&environment));
    write_meta("abc123", &meta, &environment).unwrap();
    assert_eq!(read_meta("abc123", &environment), Some(meta));
  }

  #[test]
  fn read_meta_is_none_for_missing_or_corrupt() {
    let environment = TestEnvironment::new();
    assert_eq!(read_meta("missing", &environment), None);
    environment.mk_dir_all(plugins_dir(&environment)).unwrap();
    environment.write_file(plugins_dir(&environment).join("bad.json"), "{ not json").unwrap();
    assert_eq!(read_meta("bad", &environment), None);
  }

  #[test]
  fn remove_entry_deletes_the_module_of_the_build() {
    let environment = TestEnvironment::new();
    environment.mk_dir_all(plugins_dir(&environment)).unwrap();
    let mut meta = make_meta("sig");
    meta.source_checksum = Some("0123456789abcdef0123".to_string());
    write_meta("h", &meta, &environment).unwrap();
    let module_path = wasm_module_path("h", Some("0123456789abcdef0123"), &environment);
    assert_eq!(module_path, plugins_dir(&environment).join("h-0123456789abcdef.wasm"));
    let native_path = native_module_path(&module_path);
    assert_eq!(native_path, plugins_dir(&environment).join("h-0123456789abcdef.cwasm"));
    environment.write_file(&module_path, "module").unwrap();
    environment.write_file(&native_path, "compiled").unwrap();

    remove_entry("h", &environment);

    assert!(read_meta("h", &environment).is_none());
    assert!(!environment.path_exists(&module_path));
    assert!(!environment.path_exists(&native_path));
  }

  #[test]
  fn remove_entry_deletes_meta_and_both_artifact_forms() {
    let environment = TestEnvironment::new();
    let dir = plugins_dir(&environment);
    environment.mk_dir_all(&dir).unwrap();
    write_meta("h", &make_meta("sig"), &environment).unwrap();
    let module_path = wasm_module_path("h", None, &environment);
    environment.write_file(&module_path, "module").unwrap();
    environment.write_file(native_module_path(&module_path), "compiled").unwrap();
    environment.mk_dir_all(process_dir_path("h", &environment)).unwrap();
    environment.write_file(process_dir_path("h", &environment).join("exe"), "bin").unwrap();
    environment.write_file(resolutions_path("h", &environment), "{}").unwrap();

    remove_entry("h", &environment);

    assert!(read_meta("h", &environment).is_none());
    assert!(!environment.path_exists(&module_path));
    assert!(!environment.path_exists(native_module_path(&module_path)));
    assert!(!environment.path_exists(process_dir_path("h", &environment)));
    assert!(!environment.path_exists(resolutions_path("h", &environment)));
  }
}
