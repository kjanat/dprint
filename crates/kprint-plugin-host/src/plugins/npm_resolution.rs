use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;

use anyhow::Context;
use anyhow::Error;
use anyhow::Result;
use anyhow::bail;
use flate2::read::GzDecoder;
use tar::Archive;

use deno_semver::Version;

use crate::environment::PluginEnvironment as Environment;
use crate::utils::DependencyAgeCutoff;
use crate::utils::MinimumDependencyAgeArg;
use crate::utils::NpmSpecifier;
use crate::utils::PathSource;
use crate::utils::PluginKind;
use crate::utils::get_sha256_checksum;
use crate::utils::parse_rfc3339;
use crate::utils::verify_sha256_checksum;

const NPM_REGISTRY_HOST: &str = "registry.npmjs.org";
const NPM_CDN_URL: &str = "https://cdn.jsdelivr.net/npm/";
const NPM_VERSIONS_URL: &str = "https://data.jsdelivr.com/v1/packages/npm";

/// The result of resolving an npm plugin specifier.
pub struct NpmResolvedPlugin {
  /// The extracted plugin bytes (wasm binary or plugin.json contents).
  pub plugin_bytes: Vec<u8>,
  /// Whether this is a wasm or process plugin.
  pub plugin_kind: PluginKind,
  /// The source of the plugin file (local extraction or direct CDN URL).
  /// Used as the PathSource for setup so process plugin manifests
  /// can resolve relative URLs against the package directory.
  pub local_path: PathSource,
  /// For npm-resolved process plugins, the per-platform tarball whose contents
  /// `setup_process_plugin` will extract into the plugin cache directory.
  /// Carries the full tarball bytes (verified against the plugin.json
  /// checksum) so the executable can be unpacked alongside any sibling
  /// files it depends on — node_modules-style installs, data files, etc.
  pub pre_resolved_tarball: Option<PreResolvedProcessPluginTarball>,
  /// The plugin file path within the package — the same as `specifier.path`,
  /// except when the path was detected (pathless `dprint add`), in which case
  /// it's the detected `plugin.wasm` / `plugin.json`.
  pub resolved_path: String,
  /// SHA-256 of the package tarball. For `dprint add` this is the checksum to
  /// write into the config. `None` for node_modules or direct CDN resolution.
  pub tarball_checksum: Option<String>,
}

/// Per-platform npm tarball that's been fetched and SHA-verified for a
/// process plugin's plugin.json reference. The tarball is unpacked in
/// full into the plugin cache so it can ship things alongside the binary;
/// `executable_sub_path` is the binary's location inside the tarball's
/// top-level wrapper directory (e.g. `foo` for `package/foo`).
pub struct PreResolvedProcessPluginTarball {
  pub name: String,
  pub version: String,
  pub tarball_bytes: Vec<u8>,
  pub executable_sub_path: String,
}

/// Information about the latest published version of an npm-distributed plugin.
pub struct NpmLatestInfo {
  pub version: String,
  /// SHA-256 of the latest tarball. Populated when `want_tarball_sha` was set
  /// on the request, or whenever the plugin is non-wasm (where a checksum is
  /// always required in the dprint.json specifier).
  pub tarball_sha256: Option<String>,
}

/// Inputs to [`fetch_npm_latest_info`].
pub struct FetchNpmLatestInfo<'a> {
  pub specifier: &'a NpmSpecifier,
  /// Force computing the tarball checksum even for wasm plugins. Used on
  /// update when the existing specifier carries a checksum, so the upgrade
  /// pins to the new tarball rather than carrying the stale hash. Non-wasm
  /// plugins always compute the checksum regardless.
  pub want_tarball_sha: bool,
  /// When set, a version published after the cutoff is passed over for the
  /// newest one old enough. See [`select_version_from_packument`].
  pub minimum_dependency_age: Option<&'a DependencyAgeCutoff>,
}

/// Fetches the latest version of an npm-distributed plugin (and, when needed,
/// the SHA-256 of its tarball). Used by `dprint config update` and `dprint add`.
pub async fn fetch_npm_latest_info(args: FetchNpmLatestInfo<'_>, environment: &impl Environment) -> Result<NpmLatestInfo> {
  let FetchNpmLatestInfo {
    specifier,
    want_tarball_sha,
    minimum_dependency_age,
  } = args;
  let version = resolve_npm_latest_version(&specifier.name, minimum_dependency_age, environment).await?;
  let tarball_sha256 = if want_tarball_sha || specifier.plugin_kind() != PluginKind::Wasm {
    Some(get_sha256_checksum(&download_npm_tarball(&specifier.name, &version, environment).await?))
  } else {
    None
  };
  Ok(NpmLatestInfo { version, tarball_sha256 })
}

/// Resolves the version of a package to pin: its latest release, or with a
/// minimum dependency age the newest release old enough, which needs the
/// publish times only the registry's packument has.
pub async fn resolve_npm_latest_version(name: &str, minimum_dependency_age: Option<&DependencyAgeCutoff>, environment: &impl Environment) -> Result<String> {
  match minimum_dependency_age {
    Some(cutoff) => select_version_from_packument(&fetch_packument(name, environment).await?, name, cutoff, environment),
    None => fetch_latest_version(name, environment).await,
  }
}

async fn fetch_latest_version(name: &str, environment: &impl Environment) -> Result<String> {
  let url = url::Url::parse(&format!("{}/{}/resolved?specifier=latest", NPM_VERSIONS_URL, name))?;
  log_debug!(environment, "Fetching latest npm version: {}", url);
  let (_, file) = environment
    .download_file_err_404(&url)
    .await
    .with_context(|| format!("Failed to resolve the latest version of {}", name))?;
  let response: serde_json::Value = serde_json::from_slice(&file.content).with_context(|| format!("Failed to parse the latest version of {}", name))?;
  match response.get("version").and_then(|version| version.as_str()) {
    Some(version) => Ok(version.to_string()),
    None => bail!("{} has no published version.", name),
  }
}

async fn download_npm_tarball(name: &str, version: &str, environment: &impl Environment) -> Result<Vec<u8>> {
  let file_name = name.rsplit('/').next().unwrap_or(name);
  let url = url::Url::parse(&format!("https://{}/{}/-/{}-{}.tgz", NPM_REGISTRY_HOST, name, file_name, version))?;
  log_debug!(environment, "Downloading npm tarball: {}", url);
  let (_, file) = environment
    .download_file_err_404(&url)
    .await
    .with_context(|| format!("Failed to download npm tarball for {}@{}", name, version))?;
  Ok(file.content)
}

/// Reads the cached tarball checksum for `name@version`, if a prior setup
/// recorded one. The checksum is a package-version property (the same for any
/// plugin file the package ships), so `dprint add` can reuse it without
/// re-downloading the tarball.
pub fn read_npm_tarball_checksum(name: &str, version: &str, environment: &impl Environment) -> Option<String> {
  Some(read_npm_tarball_meta(name, version, environment)?.tarball_sha256)
}

/// For a pathless `dprint add` on a repeat add: if `name@version` is already
/// extracted in the cache, detect which plugin file it ships (preferring
/// `plugin.wasm` over `plugin.json`) without re-downloading. Returns the path
/// and its kind, or `None` if the package isn't extracted or ships neither
/// conventional file (so the caller falls back to a full download).
pub fn detect_extracted_npm_plugin(name: &str, version: &str, environment: &impl Environment) -> Option<(String, PluginKind)> {
  let extract_dir = get_npm_extract_dir(name, version, environment);
  if !environment.path_exists(&extract_dir) {
    return None;
  }
  let path = detect_plugin_path_in_dir(&extract_dir, environment)?;
  let kind = plugin_kind_from_path(&path);
  Some((path, kind))
}

/// Inspects whether an npm package installed in `node_modules` (walking up from
/// `start_dir`) is a wasm or process plugin, based on which plugin file sits at
/// its root. Returns `None` if the package isn't installed or contains neither
/// file. Lets `dprint add` write `/plugin.json` for an unversioned process
/// plugin without hitting the registry.
pub fn detect_npm_plugin_kind_in_node_modules(package_name: &str, start_dir: &Path, environment: &impl Environment) -> Option<PluginKind> {
  let package_dir = find_package_in_node_modules(package_name, start_dir, environment)?;
  if environment.path_exists(package_dir.join("plugin.wasm")) {
    Some(PluginKind::Wasm)
  } else if environment.path_exists(package_dir.join("plugin.json")) {
    Some(PluginKind::Process)
  } else {
    None
  }
}

/// The `--minimum-dependency-age` flag takes precedence over the configuration
/// file's `minimumDependencyAge`. `None` means no age requirement, which is
/// dprint's default — a version is only held back when the user asks for it.
pub fn resolve_dependency_age_cutoff(
  flag: Option<&MinimumDependencyAgeArg>,
  configured: Option<&MinimumDependencyAgeArg>,
  environment: &impl Environment,
) -> Option<DependencyAgeCutoff> {
  let now = environment.sys_time_now();
  match (flag, configured) {
    (Some(flag), _) => DependencyAgeCutoff::new(flag.age(), format!("--minimum-dependency-age {}", flag.text()), now),
    (None, Some(configured)) => DependencyAgeCutoff::new(configured.age(), format!("minimumDependencyAge {}", configured.text()), now),
    (None, None) => None,
  }
}

/// No published version of a package is old enough to satisfy the configured
/// minimum dependency age. Distinguished from other resolution failures so
/// `dprint config update` can leave the plugin where it is instead of
/// reporting an error.
#[derive(Debug, thiserror::Error)]
#[error("No version of {name} at or below {latest} is old enough for the minimum dependency age ({configured}).")]
pub struct MinimumDependencyAgeError {
  pub name: String,
  pub latest: String,
  pub configured: String,
}

async fn fetch_packument(name: &str, environment: &impl Environment) -> Result<serde_json::Value> {
  let packument_url_str = get_packument_url(name);
  let packument_url = url::Url::parse(&packument_url_str).with_context(|| format!("Failed to parse npm packument URL: {}", packument_url_str))?;
  log_debug!(environment, "Fetching npm packument: {}", packument_url);
  let (_, packument_file) = environment
    .download_file_err_404(&packument_url)
    .await
    .with_context(|| format!("Failed to fetch npm packument for {}", name))?;
  serde_json::from_slice(&packument_file.content).with_context(|| format!("Failed to parse npm packument for {}", name))
}

/// Picks the version of a package to resolve to, which is `dist-tags.latest`
/// unless a minimum dependency age is configured and that version is younger
/// than it. In that case the newest version published before the cutoff is
/// used instead — the newest release is the one a supply chain attack lands
/// in, so holding off on it for a few days is what the option buys.
///
/// Only versions at or below `dist-tags.latest` are considered, and
/// prereleases are passed over unless the latest tag is itself one — so a
/// package publishing a `next` prerelease can't have it selected by a walk
/// back the user didn't ask for, while a package with nothing but prereleases
/// still resolves.
///
/// A version the registry reports no publish date for can't be shown to be
/// too new, so it stays eligible and the caller is warned. Failing against a
/// registry that doesn't serve `time` would be worse than saying so.
fn select_version_from_packument(packument: &serde_json::Value, name: &str, cutoff: &DependencyAgeCutoff, environment: &impl Environment) -> Result<String> {
  let latest = latest_version_from_packument(packument, name)?;
  let times = packument.get("time").and_then(|time| time.as_object());
  let Some(times) = times else {
    log_warn!(
      environment,
      "Could not check the age of {} — its registry doesn't report publish times. Using {}.",
      name,
      latest
    );
    return Ok(latest);
  };
  match published_time(times, &latest) {
    Some(published) if !cutoff.is_too_new(published) => return Ok(latest),
    None => {
      log_warn!(
        environment,
        "Could not check the age of {}@{} — its registry doesn't report when it was published. Using it anyway.",
        name,
        latest
      );
      return Ok(latest);
    }
    Some(_) => {} // too new — fall through and walk back
  }

  // a latest tag that isn't a version leaves nothing to measure the rest of
  // the package against, so the walk back is refused rather than run without
  // its ceiling — which would let it reach past the release the tag names
  let Ok(latest_version) = Version::parse_from_npm(&latest) else {
    return Err(too_new_error(name, &latest, cutoff));
  };
  // a package whose latest tag is itself a prerelease has nothing but
  // prereleases to walk back through, so they're only passed over when there's
  // a stable line to walk back along
  let allow_prerelease = !latest_version.pre.is_empty();
  let mut best: Option<(Version, &str)> = None;
  let mut best_is_undated = false;
  for version_text in packument.get("versions").and_then(|v| v.as_object()).into_iter().flatten().map(|(key, _)| key) {
    let Ok(version) = Version::parse_from_npm(version_text) else {
      continue;
    };
    if !allow_prerelease && !version.pre.is_empty() {
      continue;
    }
    if version > latest_version {
      continue;
    }
    // a version the registry reports no date for can't be shown to be too new,
    // so it stays in the running (matching what happens when the latest version
    // has no date) and the caller is warned about the one that gets picked
    let published = published_time(times, version_text);
    if published.is_some_and(|published| cutoff.is_too_new(published)) {
      continue;
    }
    if best.as_ref().is_none_or(|(best, _)| version > *best) {
      best_is_undated = published.is_none();
      best = Some((version, version_text));
    }
  }

  match best {
    Some((_, version)) => {
      if best_is_undated {
        log_warn!(
          environment,
          "Could not check the age of {}@{} — its registry doesn't report when it was published. Using it anyway.",
          name,
          version
        );
      }
      log_stderr_info!(
        environment,
        "Using {} {} instead of {}, which is newer than the minimum dependency age allows ({}). Pass --minimum-dependency-age=0 to use the latest version.",
        name,
        version,
        latest,
        cutoff.description(),
      );
      Ok(version.to_string())
    }
    None => Err(too_new_error(name, &latest, cutoff)),
  }
}

/// The error for a package with nothing left to select once the minimum
/// dependency age has ruled its newest releases out.
fn too_new_error(name: &str, latest: &str, cutoff: &DependencyAgeCutoff) -> Error {
  MinimumDependencyAgeError {
    name: name.to_string(),
    latest: latest.to_string(),
    configured: cutoff.description().to_string(),
  }
  .into()
}

/// When a packument says a version was published, or `None` when it reports no
/// date for it (or one this dprint can't read).
fn published_time(times: &serde_json::Map<String, serde_json::Value>, version: &str) -> Option<SystemTime> {
  parse_rfc3339(times.get(version)?.as_str()?)
}

/// Reads `dist-tags.latest` from a packument.
fn latest_version_from_packument(packument: &serde_json::Value, name: &str) -> Result<String> {
  packument
    .get("dist-tags")
    .and_then(|d| d.get("latest"))
    .and_then(|v| v.as_str())
    .map(|s| s.to_string())
    .ok_or_else(|| anyhow::anyhow!("Missing dist-tags.latest for {}", name))
}

/// Picks the plugin file a package ships at its root, preferring `plugin.wasm`
/// over `plugin.json` if (unusually) both are present. Returns `None` if
/// neither exists. Run against the extracted package directory for a pathless
/// `dprint add`.
fn detect_plugin_path_in_dir(extract_dir: &Path, environment: &impl Environment) -> Option<String> {
  if environment.path_exists(extract_dir.join("plugin.wasm")) {
    Some("plugin.wasm".to_string())
  } else if environment.path_exists(extract_dir.join("plugin.json")) {
    Some("plugin.json".to_string())
  } else {
    None
  }
}

/// Maps a plugin file path to its kind by extension (`.json` → process, else
/// wasm), matching `NpmSpecifier::plugin_kind`.
fn plugin_kind_from_path(path: &str) -> PluginKind {
  if path.rsplit_once('.').is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("json")) {
    PluginKind::Process
  } else {
    PluginKind::Wasm
  }
}

/// Inputs to [`resolve_npm_from_registry`].
pub struct ResolveNpmRegistryOptions<'a> {
  /// The package to resolve. Must carry a concrete version.
  pub specifier: &'a NpmSpecifier,
  /// Expected checksum to verify the tarball against. Ignored when
  /// `establish_checksum` is set (we're computing it, not verifying).
  pub checksum: Option<&'a str>,
  /// Pathless `dprint add`: ignore `specifier.path` and detect the plugin file
  /// (`plugin.wasm` / `plugin.json`) from the extracted package instead.
  pub detect_path: bool,
  /// `dprint add` mode: compute the checksum from the download rather than
  /// requiring one for process plugins. `dprint fmt` leaves this false so a
  /// process plugin without a checksum is still rejected.
  pub establish_checksum: bool,
}

/// Resolves a versioned npm plugin. Wasm files without a tarball checksum use
/// the CDN directly. Other references download, verify and extract the
/// tarball, then resolve the per-platform binary for process plugins.
pub async fn resolve_npm_from_registry(options: ResolveNpmRegistryOptions<'_>, environment: &impl Environment) -> Result<NpmResolvedPlugin> {
  let ResolveNpmRegistryOptions {
    specifier,
    checksum,
    detect_path,
    establish_checksum,
  } = options;
  let version = specifier
    .version
    .as_deref()
    .ok_or_else(|| anyhow::anyhow!("Cannot resolve npm plugin without a version from the registry"))?;
  // A checksum on an npm reference covers the tarball.
  if !detect_path && !establish_checksum && checksum.is_none() && specifier.plugin_kind() == PluginKind::Wasm {
    let mut url = url::Url::parse(NPM_CDN_URL)?;
    {
      let mut path = url.path_segments_mut().expect("CDN URL has a path");
      path.pop_if_empty();
      let package = format!("{}@{}", specifier.name, version);
      path.extend(package.split('/'));
      path.extend(specifier.path.split('/'));
    }
    let (resolved_url, file) = environment.download_file(&url).await?;
    if let Some(file) = file {
      return Ok(NpmResolvedPlugin {
        plugin_bytes: file.content,
        plugin_kind: PluginKind::Wasm,
        local_path: PathSource::new_remote(resolved_url.into_owned()),
        pre_resolved_tarball: None,
        resolved_path: specifier.path.clone(),
        tarball_checksum: None,
      });
    }
    // A version missing from the CDN may already be available on npm.
  }

  let tarball_bytes = download_npm_tarball(&specifier.name, version, environment).await?;

  let tarball_sha256 = get_sha256_checksum(&tarball_bytes);

  // verify the checksum before any extraction work — unless we're establishing
  // it (add mode), where there's nothing to verify against yet.
  if !establish_checksum {
    if let Some(checksum) = checksum {
      if tarball_sha256 != checksum {
        bail!(
          "Invalid checksum for npm package {}. Check the plugin's release notes for the expected checksum.\n\nActual: {}\nExpected: {}",
          specifier.display(),
          tarball_sha256,
          checksum,
        );
      }
    } else if specifier.plugin_kind() != PluginKind::Wasm {
      bail!(
        concat!(
          "The npm plugin must have a checksum specified for security reasons ",
          "since it is not a Wasm plugin. Check the plugin's release notes for what ",
          "the checksum is or if you trust the source, you may specify: {}@{}"
        ),
        specifier.display(),
        tarball_sha256,
      );
    }
  }

  // extract and read the plugin file in a blocking task since
  // tarball decompression and file I/O can be slow
  let extract_dir = get_npm_extract_dir(&specifier.name, version, environment);
  let requested_path = specifier.path.clone();
  let specifier_clone = specifier.clone();
  let environment_clone = environment.clone();
  let (plugin_bytes, local_path, resolved_path) = kprint_async_runtime::spawn_blocking(move || -> Result<_> {
    let environment = environment_clone;
    extract_tarball_to_dir(&tarball_bytes, &extract_dir, &environment)?;

    // for a pathless add, pick whichever plugin file the package actually ships.
    let resolved_path = if detect_path {
      detect_plugin_path_in_dir(&extract_dir, &environment)
        .ok_or_else(|| anyhow::anyhow!("Could not find a plugin.wasm or plugin.json in npm package {}", specifier_clone.name))?
    } else {
      requested_path
    };

    let plugin_file_path = extract_dir.join(&resolved_path);
    if !environment.path_exists(&plugin_file_path) {
      bail!(missing_plugin_file_message(
        &specifier_clone,
        &format!("npm package {}", specifier_clone.name),
        &extract_dir,
        &environment,
      ));
    }
    let plugin_bytes = environment
      .read_file_bytes(&plugin_file_path)
      .with_context(|| format!("Failed to read {}", plugin_file_path.display()))?;
    let canonical = environment.canonicalize(&plugin_file_path)?;
    Ok((plugin_bytes, PathSource::new_local(canonical), resolved_path))
  })
  .await??;

  let plugin_kind = plugin_kind_from_path(&resolved_path);

  // process plugins shipped via the npm registry mustn't silently fetch their
  // platform binary over http(s) at format time. For npm references in
  // plugin.json, we fetch the per-platform tarball from the registry and
  // verify its checksum (same flow as the node_modules path). File/relative
  // references resolve against the extract dir via the standard setup flow.
  let pre_resolved_tarball = if plugin_kind == PluginKind::Process {
    try_resolve_process_plugin_per_platform_tarball(&plugin_bytes, environment).await?
  } else {
    None
  };

  // record the tarball's checksum so a later `dprint add` of the same version
  // can reuse it without re-downloading the tarball. Best-effort.
  write_npm_tarball_meta(&specifier.name, version, &tarball_sha256, environment);

  Ok(NpmResolvedPlugin {
    plugin_bytes,
    plugin_kind,
    local_path,
    pre_resolved_tarball,
    resolved_path,
    tarball_checksum: Some(tarball_sha256),
  })
}

/// Resolves an npm plugin from node_modules (unversioned specifier). For
/// process plugins, the per-platform binary referenced by plugin.json is
/// fetched from the npm registry (not node_modules), since the checksum
/// inside plugin.json covers the per-platform tarball — see
/// `try_resolve_process_plugin_per_platform_binary`.
pub async fn resolve_npm_from_node_modules(specifier: &NpmSpecifier, config_dir: &Path, environment: &impl Environment) -> Result<NpmResolvedPlugin> {
  let package_dir = match find_package_in_node_modules(&specifier.name, config_dir, environment) {
    Some(dir) => dir,
    None => bail!(node_modules_missing_message(specifier, environment).await),
  };
  let plugin_path = package_dir.join(&specifier.path);
  if !environment.path_exists(&plugin_path) {
    bail!(missing_plugin_file_message(
      specifier,
      &package_dir.display().to_string(),
      &package_dir,
      environment,
    ));
  }
  let canonical = environment.canonicalize(&plugin_path)?;
  let local_path = PathSource::new_local(canonical.clone());
  let plugin_bytes = environment
    .read_file_bytes(canonical.as_ref())
    .with_context(|| format!("Failed to read {}", canonical.display()))?;

  let pre_resolved_tarball = if specifier.plugin_kind() == PluginKind::Process {
    try_resolve_process_plugin_per_platform_tarball(&plugin_bytes, environment).await?
  } else {
    None
  };

  Ok(NpmResolvedPlugin {
    plugin_bytes,
    plugin_kind: specifier.plugin_kind(),
    local_path,
    pre_resolved_tarball,
    resolved_path: specifier.path.clone(),
    tarball_checksum: None,
  })
}

/// Locates the canonical local path an unversioned npm specifier resolves to,
/// without reading the plugin file or doing process-plugin dep resolution.
/// Sync, so the error here is the bare "not installed" hint — the async
/// resolve path enriches the message with a versioned suggestion via
/// [`node_modules_missing_message`].
pub fn find_npm_plugin_local_path(specifier: &NpmSpecifier, config_dir: &Path, environment: &impl Environment) -> Result<PathSource> {
  let package_dir = find_package_in_node_modules(&specifier.name, config_dir, environment).ok_or_else(|| {
    anyhow::anyhow!(
      "Could not find {} in node_modules. Make sure the package is installed (npm install {}).",
      specifier.name,
      specifier.name,
    )
  })?;
  let plugin_path = package_dir.join(&specifier.path);

  if !environment.path_exists(&plugin_path) {
    bail!(missing_plugin_file_message(
      specifier,
      &package_dir.display().to_string(),
      &package_dir,
      environment,
    ));
  }

  let canonical = environment.canonicalize(&plugin_path)?;
  Ok(PathSource::new_local(canonical))
}

/// Builds the error message for a missing plugin file inside an npm package.
/// When the requested file is `plugin.wasm` (or vice-versa) and the other
/// recognized plugin file is actually present, suggest the corrected
/// specifier so users hit the helpful error instead of "Is the package a
/// dprint plugin?".
fn missing_plugin_file_message(specifier: &NpmSpecifier, package_display: &str, package_dir: &Path, environment: &impl Environment) -> String {
  if let Some(alternate) = alternate_plugin_filename(&specifier.path, package_dir, environment) {
    let suggestion = npm_specifier_with_path(specifier, alternate);
    return format!(
      "Could not find {} in {}. The package contains {} instead — reference it as `{}`.",
      specifier.path, package_display, alternate, suggestion,
    );
  }
  format!("Could not find {} in {}. Is the package a dprint plugin?", specifier.path, package_display)
}

/// If the requested path is one recognized plugin filename and the *other*
/// recognized plugin filename actually exists in the package directory,
/// return that other filename.
fn alternate_plugin_filename(requested: &str, package_dir: &Path, environment: &impl Environment) -> Option<&'static str> {
  let candidate = if requested.eq_ignore_ascii_case("plugin.wasm") {
    "plugin.json"
  } else if requested.eq_ignore_ascii_case("plugin.json") {
    "plugin.wasm"
  } else {
    return None;
  };
  if environment.path_exists(package_dir.join(candidate)) {
    Some(candidate)
  } else {
    None
  }
}

fn npm_specifier_with_path(specifier: &NpmSpecifier, path: &str) -> String {
  match &specifier.version {
    Some(version) => format!("npm:{}@{}/{}", specifier.name, version, path),
    None => format!("npm:{}/{}", specifier.name, path),
  }
}

/// Reads a process plugin manifest (plugin.json) and, if the platform-specific
/// reference is an `npm:` specifier, fetches the per-platform package's
/// tarball from the npm registry and verifies its SHA-256 against the
/// plugin.json checksum. The full tarball bytes are handed back to
/// `setup_process_plugin`, which unpacks them into the plugin cache
/// directory — extracting the whole package (not just the named binary)
/// so the executable can sit alongside any DLLs / data files it ships.
///
/// Returns `None` for in-package relative references so the caller falls back
/// to the standard flow (resolved against plugin.json's directory). Network
/// (`http(s)://`), `file://`, absolute, home (`~/`), and parent-escaping
/// (`..`) references are rejected so an npm-installed plugin can't silently
/// fetch from the network or reach a file outside its own package — see
/// [`bail_if_disallowed_reference`].
async fn try_resolve_process_plugin_per_platform_tarball(
  plugin_json_bytes: &[u8],
  environment: &impl Environment,
) -> Result<Option<PreResolvedProcessPluginTarball>> {
  use crate::plugins::implementations::get_process_plugin_os_path;
  use crate::plugins::implementations::parse_process_plugin_file;

  let plugin_file = parse_process_plugin_file(plugin_json_bytes).context("Failed to parse process plugin manifest (plugin.json)")?;
  let os_path = get_process_plugin_os_path(&plugin_file, environment)?;

  if !os_path.reference.starts_with("npm:") {
    bail_if_disallowed_reference(&plugin_file.name, &os_path.reference)?;
    return Ok(None);
  }

  let parsed = crate::utils::parse_npm_specifier(&os_path.reference)?;
  let version = parsed
    .specifier
    .version
    .as_deref()
    .ok_or_else(|| anyhow::anyhow!("npm reference in plugin '{}' must include a version: {}", plugin_file.name, os_path.reference,))?;

  let tarball_bytes = fetch_and_verify_npm_tarball(&parsed.specifier.name, version, &os_path.checksum, environment)
    .await
    .with_context(|| format!("Resolving npm dependency for process plugin '{}'", plugin_file.name))?;

  Ok(Some(PreResolvedProcessPluginTarball {
    name: plugin_file.name,
    version: plugin_file.version,
    tarball_bytes,
    executable_sub_path: parsed.specifier.path,
  }))
}

/// Fetches `name@version` from the npm registry and verifies its SHA-256
/// against `expected_checksum`. Returns the tarball bytes — the caller decides
/// where (if anywhere) to extract them. Always re-fetches and re-verifies
/// on every call so a registry that silently swaps the tarball's contents
/// is detected immediately.
async fn fetch_and_verify_npm_tarball(name: &str, version: &str, expected_checksum: &str, environment: &impl Environment) -> Result<Vec<u8>> {
  let tarball_bytes = download_npm_tarball(name, version, environment).await?;
  if let Err(err) = verify_sha256_checksum(&tarball_bytes, expected_checksum) {
    bail!(
      "Invalid checksum for npm package {}@{}. The tarball's contents don't match the expected SHA-256.\n\n{:#}",
      name,
      version,
      err,
    );
  }

  Ok(tarball_bytes)
}

/// An npm-installed process plugin must ship its per-platform binary inside
/// the package and reference it by a relative path (or fetch it from another
/// npm package via an `npm:` specifier, handled before this is called). Reject
/// every other reference: a network URL would let the plugin silently fetch an
/// unverified-at-config-time binary, and a `file://` / absolute / home /
/// parent-escaping path would point outside the package at an arbitrary file
/// already on the machine.
fn bail_if_disallowed_reference(plugin_name: &str, reference: &str) -> Result<()> {
  if is_network_reference(reference) {
    bail!(
      concat!(
        "Process plugin '{}' was installed via npm but its plugin.json references the platform ",
        "binary over the network ({}). Network references aren't allowed for npm-installed plugins; ",
        "the plugin author needs to ship the binary inside the npm package or as a separate npm package.",
      ),
      plugin_name,
      reference,
    );
  }
  if escapes_package_dir(reference) {
    bail!(
      concat!(
        "Process plugin '{}' was installed via npm but its plugin.json references the platform ",
        "binary by a path outside the package ({}). npm-installed plugins may only reference a binary ",
        "shipped inside the package (a relative path) or another npm package (an npm: specifier); ",
        "the plugin author needs to fix the reference.",
      ),
      plugin_name,
      reference,
    );
  }
  Ok(())
}

/// True for an `http(s)://` reference (an absolute URL whose scheme is http or
/// https). Relative paths don't parse as a base URL, so they aren't matched.
fn is_network_reference(reference: &str) -> bool {
  url::Url::parse(reference)
    .ok()
    .filter(|u| !u.cannot_be_a_base())
    .map(|u| matches!(u.scheme(), "http" | "https"))
    .unwrap_or(false)
}

/// True for references that resolve outside the package directory: `file://`
/// URLs, absolute paths (POSIX `/…`, UNC / rooted `\…`, Windows `C:\…` /
/// `C:/…`), home-directory (`~/…`) paths, and relative paths containing a
/// `..` component. Plain in-package relative paths are not matched.
fn escapes_package_dir(reference: &str) -> bool {
  if reference == "~" || reference.starts_with("~/") {
    return true;
  }
  if is_absolute_reference(reference) {
    return true;
  }
  if reference.split(['/', '\\']).any(|segment| segment == "..") {
    return true;
  }
  url::Url::parse(reference)
    .ok()
    .filter(|u| !u.cannot_be_a_base())
    .map(|u| u.scheme() == "file")
    .unwrap_or(false)
}

fn is_absolute_reference(reference: &str) -> bool {
  let bytes = reference.as_bytes();
  // POSIX absolute, or UNC / drive-relative-rooted on Windows
  if matches!(bytes.first(), Some(b'/' | b'\\')) {
    return true;
  }
  // Windows drive path: `C:\…` or `C:/…`
  let chars: Vec<char> = reference.chars().take(3).collect();
  matches!(chars.first(), Some(c) if c.is_ascii_alphabetic()) && matches!(chars.get(1), Some(':')) && matches!(chars.get(2), Some('/' | '\\'))
}

/// Returns the directory where an npm package tarball should be extracted.
pub(super) fn get_npm_extract_dir(package_name: &str, version: &str, environment: &impl Environment) -> PathBuf {
  // use a sanitized name for the directory (replace / with __)
  let dir_name = format!("{}@{}", package_name.replace('/', "__"), version);
  environment.get_cache_dir().join("npm").join(NPM_REGISTRY_HOST).join(dir_name)
}

/// The tarball's SHA-256, cached so a later `dprint add` of the same
/// `name@version` can reuse it without re-downloading the tarball. The checksum
/// is a package-version property — the same regardless of which plugin file in
/// the package an entry points at — so only it is cached here; the plugin kind
/// is always derived from the specifier path or the extracted files. If a
/// registry ever republishes the same version with different bytes the cached
/// checksum goes stale, but that fails closed: the next `dprint fmt`
/// re-downloads and errors on the mismatch (and `clear-cache` fixes it).
struct NpmTarballMeta {
  tarball_sha256: String,
}

/// Path of the sidecar file caching a tarball's checksum, kept next to (not
/// inside) the `name@version` extract directory so it doesn't mix with the
/// package's own files. Wiped by `dprint clear-cache` along with the rest of
/// the npm cache.
fn npm_tarball_meta_path(package_name: &str, version: &str, environment: &impl Environment) -> PathBuf {
  let file_name = format!("{}@{}.meta.json", package_name.replace('/', "__"), version);
  environment.get_cache_dir().join("npm").join(NPM_REGISTRY_HOST).join(file_name)
}

/// Reads the cached tarball sidecar, or `None` if it's missing or unreadable.
fn read_npm_tarball_meta(package_name: &str, version: &str, environment: &impl Environment) -> Option<NpmTarballMeta> {
  let path = npm_tarball_meta_path(package_name, version, environment);
  let text = environment.read_file(&path).ok()?;
  let value: serde_json::Value = serde_json::from_str(&text).ok()?;
  let tarball_sha256 = value.get("tarballChecksum")?.as_str()?.to_string();
  Some(NpmTarballMeta { tarball_sha256 })
}

/// Writes the tarball sidecar. Best-effort — a failure just means the next
/// `dprint add` re-downloads to recompute the checksum.
fn write_npm_tarball_meta(package_name: &str, version: &str, tarball_sha256: &str, environment: &impl Environment) {
  let json = serde_json::json!({ "tarballChecksum": tarball_sha256 });
  let path = npm_tarball_meta_path(package_name, version, environment);
  let _ = environment.write_file(&path, &json.to_string());
}

/// Extracts an npm tarball to a directory on disk.
///
/// Idempotent and concurrency-safe:
/// - If `dest_dir` already exists, returns immediately. `name@version` is
///   immutable on npm, and `dest_dir` only appears after a complete extract
///   (we populate a temp dir first, then rename), so existence implies a
///   prior successful extract.
/// - Each call uses a uniquely-named temp dir, so two concurrent extracts
///   (different processes, or otherwise outside the in-process fs lock pool)
///   never trample each other's intermediate state.
/// - If the final rename fails because a racing extract finished first, we
///   discard our copy and use the winner's `dest_dir`.
///
/// Strips the first path component (usually `package/`) from each entry.
fn extract_tarball_to_dir(tarball_bytes: &[u8], dest_dir: &Path, environment: &impl Environment) -> Result<()> {
  use crate::utils::fs::get_atomic_path;

  if environment.path_exists(dest_dir) {
    return Ok(());
  }

  let temp_dir = get_atomic_path(environment, dest_dir);
  environment.mk_dir_all(&temp_dir)?;

  if let Err(err) = extract_tarball_to_dir_inner(tarball_bytes, &temp_dir, environment) {
    environment.try_remove_dir_all(&temp_dir);
    return Err(err);
  }

  match environment.rename(&temp_dir, dest_dir) {
    Ok(()) => Ok(()),
    Err(err) => {
      if environment.path_exists(dest_dir) {
        // another concurrent extract finished first — discard our copy and
        // use theirs (immutable name@version means contents are equivalent)
        environment.try_remove_dir_all(&temp_dir);
        Ok(())
      } else {
        environment.try_remove_dir_all(&temp_dir);
        Err(err.into())
      }
    }
  }
}

/// Extracts an npm tarball into `dest_dir`, replacing any existing contents.
/// Same wrapper-stripping / path-traversal / permission-preserving rules as
/// [`extract_tarball_to_dir`], but for caches whose contents are *not*
/// content-addressable (e.g. the per-plugin cache, which gets rewritten
/// whenever the source plugin.json changes). The extract goes through a
/// sibling temp dir so a crash mid-extract can't leave the destination
/// half-populated; the caller is responsible for serializing extracts
/// against the same `dest_dir` via fs locks.
pub(in crate::plugins) fn extract_tarball_replacing(tarball_bytes: &[u8], dest_dir: &Path, environment: &impl Environment) -> Result<()> {
  use crate::utils::fs::get_atomic_path;

  let temp_dir = get_atomic_path(environment, dest_dir);
  environment.mk_dir_all(&temp_dir)?;

  if let Err(err) = extract_tarball_to_dir_inner(tarball_bytes, &temp_dir, environment) {
    environment.try_remove_dir_all(&temp_dir);
    return Err(err);
  }

  // remove any existing directory before moving the staged extract into place.
  // surface a removal failure directly — otherwise the rename below fails with
  // a confusing "directory not empty" error that hides the real cause.
  if let Err(err) = environment.remove_dir_all(dest_dir) {
    environment.try_remove_dir_all(&temp_dir);
    return Err(err.into());
  }
  if let Err(err) = environment.rename(&temp_dir, dest_dir) {
    environment.try_remove_dir_all(&temp_dir);
    return Err(err.into());
  }
  Ok(())
}

fn extract_tarball_to_dir_inner(tarball_bytes: &[u8], output_dir: &Path, environment: &impl Environment) -> Result<()> {
  let decoder = GzDecoder::new(tarball_bytes);
  let mut archive = Archive::new(decoder);
  // npm tarballs wrap every entry under a single top-level directory (usually
  // "package/"). Lock in that wrapper from the first entry and reject any
  // entry that doesn't share it, otherwise files outside the wrapper would
  // be silently dropped.
  let mut wrapper: Option<std::ffi::OsString> = None;
  let mut files_written: usize = 0;
  let mut created_dirs = std::collections::HashSet::from([output_dir.to_path_buf()]);

  for entry in archive.entries().context("Failed to read npm tarball entries")? {
    let mut entry = entry.context("Failed to read npm tarball entry")?;
    let entry_type = entry.header().entry_type();

    // skip symlinks, hardlinks, and other special entries
    match entry_type {
      tar::EntryType::Regular | tar::EntryType::Directory => {}
      tar::EntryType::Symlink | tar::EntryType::Link => {
        // npm doesn't support symlinks/hardlinks in packages
        continue;
      }
      tar::EntryType::XGlobalHeader => continue,
      _ => continue,
    }

    let path = entry.path().context("Failed to get entry path")?.to_path_buf();

    // GNU tar prefixes entries with `./` by default; skip those leading
    // `CurDir`s so the wrapper-detection sees the real first directory.
    let mut components = path.components().peekable();
    while let Some(std::path::Component::CurDir) = components.peek() {
      components.next();
    }
    let Some(first) = components.next() else {
      continue;
    };
    // require a normal directory component as the wrapper. Absolute paths
    // (RootDir / Prefix) and `..` (ParentDir) at the top of an entry would
    // otherwise pin `wrapper` to a value we'd silently apply to subsequent
    // entries, mis-shaping the extract.
    let std::path::Component::Normal(first_name) = first else {
      bail!(
        "Refusing to extract npm tarball entry with non-relative top-level component: {}",
        path.display(),
      );
    };
    let first_os = first_name.to_os_string();
    match &wrapper {
      None => wrapper = Some(first_os),
      Some(existing) if existing == &first_os => {}
      Some(existing) => {
        bail!(
          "Inconsistent npm tarball: expected all entries under '{}/' but found '{}'",
          existing.to_string_lossy(),
          path.display(),
        );
      }
    }

    let relative: PathBuf = components.collect();
    if relative.as_os_str().is_empty() {
      continue;
    }

    let dest_path = output_dir.join(&relative);

    // path traversal check: ensure the resolved path stays within the output directory
    let normalized = normalize_path(&dest_path);
    if !normalized.starts_with(output_dir) {
      bail!("Refusing to extract tarball entry outside output directory: {}", path.display());
    }

    if entry_type == tar::EntryType::Directory {
      if created_dirs.insert(dest_path.clone()) {
        environment.mk_dir_all(&dest_path)?;
      }
      continue;
    }

    // regular file
    if let Some(parent) = dest_path.parent()
      && created_dirs.insert(parent.to_path_buf())
    {
      environment.mk_dir_all(parent)?;
    }

    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut entry, &mut bytes)?;
    environment.write_file_bytes(&dest_path, &bytes)?;
    files_written += 1;

    // preserve executable permissions on unix
    #[cfg(unix)]
    if let Ok(mode) = entry.header().mode()
      && mode != 0o644
    {
      use sys_traits::FsSetPermissions;
      environment
        .fs_set_permissions(&dest_path, mode)
        .with_context(|| format!("Failed to set permissions on {}", dest_path.display()))?;
    }
  }

  if files_written == 0 {
    bail!("npm tarball contained no extractable files (expected at least one file under a wrapper directory)");
  }

  Ok(())
}

/// Normalizes a path by resolving `.` and `..` components without accessing the filesystem.
fn normalize_path(path: &Path) -> PathBuf {
  let mut result = PathBuf::new();
  for component in path.components() {
    match component {
      std::path::Component::ParentDir => {
        result.pop();
      }
      std::path::Component::CurDir => {}
      other => result.push(other),
    }
  }
  result
}

fn get_packument_url(package_name: &str) -> String {
  format!("https://{}/{}", NPM_REGISTRY_HOST, package_name)
}

/// Walks up from `start_dir` looking for `node_modules/{package_name}/`.
/// Returns `None` if not installed anywhere along the ancestor chain.
fn find_package_in_node_modules(package_name: &str, start_dir: &Path, environment: &impl Environment) -> Option<std::path::PathBuf> {
  for dir in start_dir.ancestors() {
    let candidate = dir.join("node_modules").join(package_name);
    if environment.path_exists(&candidate) {
      return Some(candidate);
    }
  }
  None
}

/// Builds the user-facing "package not in node_modules" error. When the
/// caller has async context (the user-facing resolve path), it also
/// suggests a concrete `npm:<name>@<version>` specifier built from the
/// package's latest version; when that lookup fails we fall back to
/// the bare `npm install` hint. We deliberately don't append the tarball
/// checksum here — for non-wasm plugins the next resolve step bails with a
/// "must have a checksum" error that quotes the exact value to use.
async fn node_modules_missing_message(specifier: &NpmSpecifier, environment: &impl Environment) -> String {
  match fetch_latest_version(&specifier.name, environment).await.ok() {
    Some(version) => {
      // hide the default `plugin.wasm` path so the suggestion matches what
      // a user would actually type. Non-default paths (e.g. `plugin.json`)
      // are kept verbatim.
      let suggestion = if specifier.path == "plugin.wasm" {
        format!("npm:{}@{}", specifier.name, version)
      } else {
        format!("npm:{}@{}/{}", specifier.name, version, specifier.path)
      };
      format!(
        concat!(
          "Could not find {} in node_modules.\n",
          "\n",
          "1. Make sure the package is installed (ex. npm install {})\n",
          "2. OR specify a version (ex. {})",
        ),
        specifier.name, specifier.name, suggestion,
      )
    }
    None => format!(
      "Could not find {} in node_modules. Make sure the package is installed (npm install {}).",
      specifier.name, specifier.name,
    ),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use kprint_platform::environment::*;

  #[test]
  fn bail_if_disallowed_reference_only_allows_in_package_relative_paths() {
    // relative, in-package references are the supported form and must pass
    for ok in ["bin.zip", "./bin.zip", "sub/bin", "sub/dir/foo.exe"] {
      assert!(bail_if_disallowed_reference("p", ok).is_ok(), "expected {ok} to be allowed");
    }

    // network, file://, absolute, home, and parent-escaping refs are rejected
    let network = ["http://example.com/bin.zip", "https://example.com/bin.zip"];
    for r in network {
      let err = bail_if_disallowed_reference("p", r).unwrap_err().to_string();
      assert!(err.contains("Network references aren't allowed"), "got: {err}");
    }
    let outside = [
      "file:///etc/passwd",
      "/etc/passwd",
      "\\\\server\\share\\bin",
      "C:\\Windows\\bin.exe",
      "C:/Windows/bin.exe",
      "~/bin.zip",
      "../escape/bin",
      "sub/../../escape",
    ];
    for r in outside {
      let err = bail_if_disallowed_reference("p", r).unwrap_err().to_string();
      assert!(err.contains("outside the package"), "expected {r} rejected, got: {err}");
    }
  }

  /// 2024-05-10T00:00:00Z — the "now" the minimum dependency age tests run at.
  const NOW: u64 = 1715299200;

  /// A packument for a wasm plugin package with a publish time per version.
  /// A version given no time stands for one the registry reports no date for.
  fn packument_with_times(latest: &str, versions: &[(&str, Option<&str>)]) -> serde_json::Value {
    let mut version_map = serde_json::Map::new();
    let mut time_map = serde_json::Map::new();
    for (version, time) in versions {
      version_map.insert(
        version.to_string(),
        serde_json::json!({ "dist": { "tarball": format!("https://registry.npmjs.org/foo/-/foo-{}.tgz", version) } }),
      );
      if let Some(time) = time {
        time_map.insert(version.to_string(), serde_json::json!(time));
      }
    }
    serde_json::json!({
      "dist-tags": { "latest": latest },
      "versions": version_map,
      "time": time_map,
    })
  }

  /// Resolves the version `dprint add` would pin for `foo`, at [`NOW`] with the
  /// given age. Returns the stderr the resolution logged alongside it.
  async fn resolve_version_with_age(packument: serde_json::Value, age: &str) -> (Result<String>, Vec<String>) {
    use crate::environment::TestEnvironment;
    use std::str::FromStr;
    let environment = TestEnvironment::new();
    environment.set_fs_time(NOW);
    environment.add_remote_file_bytes("https://registry.npmjs.org/foo", packument.to_string().into_bytes());
    let age = MinimumDependencyAgeArg::from_str(age).unwrap();
    let cutoff = resolve_dependency_age_cutoff(Some(&age), None, &environment);
    let result = resolve_npm_latest_version("foo", cutoff.as_ref(), &environment).await;
    let logged = environment.take_stderr_messages();
    (result, logged)
  }

  #[tokio::test]
  async fn minimum_dependency_age_keeps_latest_when_it_is_old_enough() {
    let packument = packument_with_times("1.1.0", &[("1.0.0", Some("2024-01-01T00:00:00Z")), ("1.1.0", Some("2024-05-01T00:00:00Z"))]);
    let (version, logged) = resolve_version_with_age(packument, "P3D").await;
    assert_eq!(version.unwrap(), "1.1.0");
    assert!(logged.is_empty(), "expected nothing logged, got {:?}", logged);
  }

  #[tokio::test]
  async fn minimum_dependency_age_walks_back_to_the_newest_version_old_enough() {
    let packument = packument_with_times(
      "1.1.0",
      &[
        ("1.0.0", Some("2024-01-01T00:00:00Z")),
        ("1.0.1", Some("2024-02-01T00:00:00Z")),
        // published the day before `NOW`, so a three day minimum rules it out
        ("1.1.0", Some("2024-05-09T00:00:00Z")),
      ],
    );
    let (version, logged) = resolve_version_with_age(packument, "P3D").await;
    assert_eq!(version.unwrap(), "1.0.1");
    assert_eq!(
      logged,
      vec!["Using foo 1.0.1 instead of 1.1.0, which is newer than the minimum dependency age allows (--minimum-dependency-age P3D). Pass --minimum-dependency-age=0 to use the latest version.".to_string()]
    );
  }

  #[tokio::test]
  async fn minimum_dependency_age_never_walks_back_to_a_prerelease_or_past_latest() {
    let packument = packument_with_times(
      "1.1.0",
      &[
        ("0.9.0", Some("2024-01-01T00:00:00Z")),
        // a prerelease below latest, a `next` tagged prerelease and a version
        // above latest are all old enough, but none is what the user asked for
        // by adding the package
        ("1.0.0-beta.1", Some("2024-01-02T00:00:00Z")),
        ("2.0.0-rc.1", Some("2024-01-02T00:00:00Z")),
        ("2.0.0", Some("2024-01-03T00:00:00Z")),
        ("1.1.0", Some("2024-05-09T00:00:00Z")),
      ],
    );
    let (version, _) = resolve_version_with_age(packument, "P3D").await;
    assert_eq!(version.unwrap(), "0.9.0");
  }

  #[tokio::test]
  async fn minimum_dependency_age_keeps_a_version_with_no_publish_date_in_the_running() {
    // an undated version can't be shown to be too new, so it stays eligible
    // and the caller is told the age couldn't be checked for it
    let packument = packument_with_times(
      "1.1.0",
      &[
        ("1.0.0", Some("2024-01-01T00:00:00Z")),
        ("1.0.5", None),
        ("1.1.0", Some("2024-05-09T00:00:00Z")),
      ],
    );
    let (version, logged) = resolve_version_with_age(packument, "P3D").await;
    assert_eq!(version.unwrap(), "1.0.5");
    assert_eq!(
      logged,
      vec![
        "Could not check the age of foo@1.0.5 — its registry doesn't report when it was published. Using it anyway.".to_string(),
        "Using foo 1.0.5 instead of 1.1.0, which is newer than the minimum dependency age allows (--minimum-dependency-age P3D). Pass --minimum-dependency-age=0 to use the latest version.".to_string(),
      ]
    );
  }

  #[tokio::test]
  async fn minimum_dependency_age_walks_back_through_prereleases_when_latest_is_one() {
    // a package whose latest tag is a prerelease has nothing but prereleases
    // to walk back through, so skipping them would leave it unresolvable
    let packument = packument_with_times(
      "1.0.0-rc.2",
      &[("1.0.0-rc.1", Some("2024-01-01T00:00:00Z")), ("1.0.0-rc.2", Some("2024-05-09T00:00:00Z"))],
    );
    let (version, _) = resolve_version_with_age(packument, "P3D").await;
    assert_eq!(version.unwrap(), "1.0.0-rc.1");
  }

  #[tokio::test]
  async fn minimum_dependency_age_refuses_to_walk_back_without_a_ceiling() {
    // a latest tag that isn't a version leaves nothing to bound the walk back,
    // which would otherwise reach past the release the tag names
    let mut packument = packument_with_times("1.0.0", &[("1.0.0", Some("2024-05-09T00:00:00Z")), ("2.0.0", Some("2024-01-01T00:00:00Z"))]);
    packument["dist-tags"]["latest"] = serde_json::json!("not-a-version");
    packument["time"]["not-a-version"] = serde_json::json!("2024-05-09T00:00:00Z");
    let (result, _) = resolve_version_with_age(packument, "P3D").await;
    assert!(result.unwrap_err().downcast_ref::<MinimumDependencyAgeError>().is_some());
  }

  #[tokio::test]
  async fn minimum_dependency_age_errors_when_nothing_is_old_enough() {
    let packument = packument_with_times("1.1.0", &[("1.0.0", Some("2024-05-09T00:00:00Z")), ("1.1.0", Some("2024-05-09T12:00:00Z"))]);
    let (result, _) = resolve_version_with_age(packument, "P3D").await;
    let err = result.unwrap_err();
    assert_eq!(
      err.to_string(),
      "No version of foo at or below 1.1.0 is old enough for the minimum dependency age (--minimum-dependency-age P3D)."
    );
    assert!(err.downcast_ref::<MinimumDependencyAgeError>().is_some(), "expected the age error type");
  }

  #[tokio::test]
  async fn minimum_dependency_age_uses_latest_when_the_registry_reports_no_times() {
    let packument = serde_json::json!({
      "dist-tags": { "latest": "1.1.0" },
      "versions": { "1.1.0": { "dist": { "tarball": "https://registry.npmjs.org/foo/-/foo-1.1.0.tgz" } } }
    });
    let (version, logged) = resolve_version_with_age(packument, "P3D").await;
    assert_eq!(version.unwrap(), "1.1.0");
    assert_eq!(
      logged,
      vec!["Could not check the age of foo — its registry doesn't report publish times. Using 1.1.0.".to_string()]
    );
  }

  #[tokio::test]
  async fn minimum_dependency_age_uses_latest_when_it_has_no_publish_date() {
    let packument = packument_with_times("1.1.0", &[("1.0.0", Some("2024-01-01T00:00:00Z")), ("1.1.0", None)]);
    let (version, logged) = resolve_version_with_age(packument, "P3D").await;
    assert_eq!(version.unwrap(), "1.1.0");
    assert_eq!(
      logged,
      vec!["Could not check the age of foo@1.1.0 — its registry doesn't report when it was published. Using it anyway.".to_string()]
    );
  }

  #[test]
  fn resolve_dependency_age_cutoff_defaults_to_no_minimum() {
    use crate::environment::TestEnvironment;
    let environment = TestEnvironment::new();
    environment.set_fs_time(NOW);
    assert_eq!(resolve_dependency_age_cutoff(None, None, &environment), None);
  }

  #[test]
  fn resolve_dependency_age_cutoff_describes_the_value_as_it_was_written() {
    use crate::environment::TestEnvironment;
    use std::str::FromStr;
    let environment = TestEnvironment::new();
    environment.set_fs_time(NOW);
    let describe = |text: &str| {
      let age = MinimumDependencyAgeArg::from_str(text).unwrap();
      resolve_dependency_age_cutoff(Some(&age), None, &environment).unwrap().description().to_string()
    };
    assert_eq!(describe("PT36H"), "--minimum-dependency-age PT36H");
    // the value is quoted back as the user typed it rather than canonicalized
    assert_eq!(describe("90"), "--minimum-dependency-age 90");
    assert_eq!(describe("2024-05-05"), "--minimum-dependency-age 2024-05-05");
  }

  #[test]
  fn resolve_dependency_age_cutoff_prefers_the_flag_over_the_config_file() {
    use crate::environment::TestEnvironment;
    use std::str::FromStr;
    let environment = TestEnvironment::new();
    environment.set_fs_time(NOW);
    let configured = MinimumDependencyAgeArg::from_str("P3D").unwrap();
    let cutoff = resolve_dependency_age_cutoff(None, Some(&configured), &environment).unwrap();
    assert_eq!(cutoff.description(), "minimumDependencyAge P3D");
    let flag = MinimumDependencyAgeArg::from_str("P1D").unwrap();
    let cutoff = resolve_dependency_age_cutoff(Some(&flag), Some(&configured), &environment).unwrap();
    assert_eq!(cutoff.description(), "--minimum-dependency-age P1D");
    let off = MinimumDependencyAgeArg::from_str("0").unwrap();
    assert_eq!(resolve_dependency_age_cutoff(Some(&off), Some(&configured), &environment), None);
  }

  #[tokio::test]
  async fn minimum_dependency_age_accepts_an_absolute_cutoff_date() {
    let packument = packument_with_times("1.1.0", &[("1.0.0", Some("2024-01-01T00:00:00Z")), ("1.1.0", Some("2024-05-09T00:00:00Z"))]);
    // 1.1.0 landed after the cutoff date, so the release before it is used
    let (version, logged) = resolve_version_with_age(packument, "2024-05-05").await;
    assert_eq!(version.unwrap(), "1.0.0");
    assert_eq!(
      logged,
      vec!["Using foo 1.0.0 instead of 1.1.0, which is newer than the minimum dependency age allows (--minimum-dependency-age 2024-05-05). Pass --minimum-dependency-age=0 to use the latest version.".to_string()]
    );
  }

  #[test]
  fn test_get_packument_url_scoped() {
    assert_eq!(get_packument_url("@dprint/typescript"), "https://registry.npmjs.org/@dprint/typescript");
  }

  #[test]
  fn test_get_packument_url_unscoped() {
    assert_eq!(get_packument_url("dprint-plugin-foo"), "https://registry.npmjs.org/dprint-plugin-foo");
  }

  #[tokio::test]
  async fn fetch_npm_latest_info_wasm_skips_tarball_download() {
    use crate::environment::TestEnvironment;
    let environment = TestEnvironment::new();
    environment.add_npm_latest_version("foo", "1.2.3");
    // intentionally NOT adding the tarball — wasm should not fetch it

    let specifier = NpmSpecifier {
      name: "foo".to_string(),
      version: Some("1.0.0".to_string()),
      path: "plugin.wasm".to_string(),
    };
    let info = fetch_npm_latest_info(
      FetchNpmLatestInfo {
        specifier: &specifier,
        minimum_dependency_age: None,
        want_tarball_sha: false,
      },
      &environment,
    )
    .await
    .unwrap();
    assert_eq!(info.version, "1.2.3");
    assert!(info.tarball_sha256.is_none());
  }

  #[tokio::test]
  async fn fetch_npm_latest_info_wasm_with_want_tarball_sha_fetches_tarball() {
    // wasm plugin updates that previously carried a checksum must get a fresh
    // tarball sha rather than reusing the stale one from the old specifier.
    use crate::environment::TestEnvironment;
    let environment = TestEnvironment::new();
    environment.add_npm_latest_version("foo", "1.2.3");
    let tarball_bytes = vec![1u8, 2, 3, 4];
    let expected = get_sha256_checksum(&tarball_bytes);
    environment.add_remote_file_bytes("https://registry.npmjs.org/foo/-/foo-1.2.3.tgz", tarball_bytes);

    let specifier = NpmSpecifier {
      name: "foo".to_string(),
      version: Some("1.0.0".to_string()),
      path: "plugin.wasm".to_string(),
    };
    let info = fetch_npm_latest_info(
      FetchNpmLatestInfo {
        specifier: &specifier,
        minimum_dependency_age: None,
        want_tarball_sha: true,
      },
      &environment,
    )
    .await
    .unwrap();
    assert_eq!(info.version, "1.2.3");
    assert_eq!(info.tarball_sha256.as_deref(), Some(expected.as_str()));
  }

  #[tokio::test]
  async fn fetch_npm_latest_info_process_downloads_tarball_for_checksum() {
    use crate::environment::TestEnvironment;
    let environment = TestEnvironment::new();
    environment.add_npm_latest_version("foo", "2.0.0");
    let tarball_bytes = vec![0u8, 1, 2, 3, 4, 5];
    let expected_checksum = get_sha256_checksum(&tarball_bytes);
    environment.add_remote_file_bytes("https://registry.npmjs.org/foo/-/foo-2.0.0.tgz", tarball_bytes);

    let specifier = NpmSpecifier {
      name: "foo".to_string(),
      version: Some("1.0.0".to_string()),
      path: "plugin.json".to_string(),
    };
    let info = fetch_npm_latest_info(
      FetchNpmLatestInfo {
        specifier: &specifier,
        minimum_dependency_age: None,
        want_tarball_sha: false,
      },
      &environment,
    )
    .await
    .unwrap();
    assert_eq!(info.version, "2.0.0");
    assert_eq!(info.tarball_sha256.as_deref(), Some(expected_checksum.as_str()));
  }

  #[tokio::test]
  async fn resolve_npm_latest_version_errors_when_the_package_has_no_version() {
    use crate::environment::TestEnvironment;
    let environment = TestEnvironment::new();
    environment.add_remote_file_bytes(
      &crate::test_helpers::npm_latest_version_url("foo"),
      br#"{ "type": "npm", "name": "foo", "version": null }"#.to_vec(),
    );
    let err = resolve_npm_latest_version("foo", None, &environment).await.unwrap_err();
    assert_eq!(err.to_string(), "foo has no published version.");
  }

  #[test]
  fn detect_plugin_path_in_dir_prefers_wasm_then_json() {
    use crate::environment::TestEnvironment;
    let environment = TestEnvironment::new();
    environment.mk_dir_all("/pkg").unwrap();
    assert_eq!(detect_plugin_path_in_dir(std::path::Path::new("/pkg"), &environment), None);
    environment.write_file("/pkg/plugin.json", "{}").unwrap();
    assert_eq!(
      detect_plugin_path_in_dir(std::path::Path::new("/pkg"), &environment).as_deref(),
      Some("plugin.json")
    );
    // wasm wins if a package unusually ships both
    environment.write_file("/pkg/plugin.wasm", "\0asm").unwrap();
    assert_eq!(
      detect_plugin_path_in_dir(std::path::Path::new("/pkg"), &environment).as_deref(),
      Some("plugin.wasm")
    );
  }

  #[test]
  fn plugin_kind_from_path_by_extension() {
    assert_eq!(plugin_kind_from_path("plugin.wasm"), PluginKind::Wasm);
    assert_eq!(plugin_kind_from_path("plugin.json"), PluginKind::Process);
    assert_eq!(plugin_kind_from_path("sub/plugin.JSON"), PluginKind::Process);
    assert_eq!(plugin_kind_from_path("foo"), PluginKind::Wasm);
  }

  #[tokio::test]
  async fn resolve_npm_from_registry_add_mode_detects_path_checksums_and_writes_sidecar() {
    use crate::environment::TestEnvironment;
    use crate::test_helpers::create_test_npm_tarball;

    let environment = TestEnvironment::new();
    let tarball = create_test_npm_tarball(&[("package/plugin.wasm", b"\0asm")]);
    let expected = get_sha256_checksum(&tarball);
    environment.add_remote_file_bytes("https://registry.npmjs.org/foo/-/foo-1.0.0.tgz", tarball);
    // a defaulted path + add mode: detect the file, compute the checksum (no
    // checksum required to verify), and record the sidecar.
    let specifier = NpmSpecifier {
      name: "foo".to_string(),
      version: Some("1.0.0".to_string()),
      path: "plugin.wasm".to_string(),
    };
    let resolved = resolve_npm_from_registry(
      ResolveNpmRegistryOptions {
        specifier: &specifier,
        checksum: None,
        detect_path: true,
        establish_checksum: true,
      },
      &environment,
    )
    .await
    .unwrap();
    assert_eq!(resolved.resolved_path, "plugin.wasm");
    assert_eq!(resolved.plugin_kind, PluginKind::Wasm);
    assert_eq!(resolved.tarball_checksum.as_deref(), Some(expected.as_str()));

    // the setup wrote the checksum sidecar, so a later add can reuse it...
    assert_eq!(read_npm_tarball_checksum("foo", "1.0.0", &environment).as_deref(), Some(expected.as_str()));
    // ...and re-detect the plugin file from the already-extracted package
    // (no package-level cached "kind").
    assert_eq!(
      detect_extracted_npm_plugin("foo", "1.0.0", &environment),
      Some(("plugin.wasm".to_string(), PluginKind::Wasm))
    );
  }

  #[test]
  fn detect_npm_plugin_kind_in_node_modules_reads_installed_package() {
    use crate::environment::TestEnvironment;
    let environment = TestEnvironment::new();
    environment.mk_dir_all("/repo/node_modules/proc").unwrap();
    environment.mk_dir_all("/repo/node_modules/wasmy").unwrap();
    environment.write_file("/repo/node_modules/proc/plugin.json", "{}").unwrap();
    environment.write_file("/repo/node_modules/wasmy/plugin.wasm", "\0asm").unwrap();

    assert_eq!(
      detect_npm_plugin_kind_in_node_modules("proc", std::path::Path::new("/repo"), &environment),
      Some(PluginKind::Process)
    );
    assert_eq!(
      detect_npm_plugin_kind_in_node_modules("wasmy", std::path::Path::new("/repo"), &environment),
      Some(PluginKind::Wasm)
    );
    assert_eq!(
      detect_npm_plugin_kind_in_node_modules("missing", std::path::Path::new("/repo"), &environment),
      None
    );
  }

  #[test]
  fn test_extract_tarball_to_dir() {
    use crate::environment::RealEnvironment;
    RealEnvironment::run_test_with_real_env(|env| {
      Box::pin(async move {
        let tarball = create_test_tarball(&[("package/plugin.wasm", b"wasm-bytes"), ("package/extra/data.bin", b"extra-data")]);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("extracted");

        extract_tarball_to_dir(&tarball, &dest, &env).unwrap();

        assert_eq!(env.read_file_bytes(dest.join("plugin.wasm")).unwrap(), b"wasm-bytes");
        assert_eq!(env.read_file_bytes(dest.join("extra").join("data.bin")).unwrap(), b"extra-data");
        // the "package" prefix should be stripped
        assert!(!env.path_exists(dest.join("package")));
      })
    });
  }

  #[test]
  fn extract_tarball_rejects_inconsistent_wrapper() {
    use crate::environment::RealEnvironment;
    RealEnvironment::run_test_with_real_env(|env| {
      Box::pin(async move {
        let tarball = create_test_tarball(&[("package/plugin.wasm", b"wasm-bytes"), ("other/extra.bin", b"stray")]);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("extracted");

        let err = extract_tarball_to_dir(&tarball, &dest, &env).unwrap_err();
        assert!(err.to_string().contains("Inconsistent npm tarball"), "got: {}", err);
      })
    });
  }

  #[test]
  fn extract_tarball_rejects_root_only_entries() {
    use crate::environment::RealEnvironment;
    RealEnvironment::run_test_with_real_env(|env| {
      Box::pin(async move {
        // a single file with no wrapper directory — would have been silently dropped
        let tarball = create_test_tarball(&[("plugin.wasm", b"wasm-bytes")]);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("extracted");

        let err = extract_tarball_to_dir(&tarball, &dest, &env).unwrap_err();
        assert!(err.to_string().contains("no extractable files"), "got: {}", err);
      })
    });
  }

  #[test]
  fn extract_tarball_accepts_leading_curdir_components() {
    // GNU tar's default output prefixes every entry with `./`. The wrapper-
    // detection logic must look past those leading `CurDir`s rather than
    // treating them as the first directory.
    use crate::environment::RealEnvironment;
    use crate::test_helpers::create_test_npm_tarball_raw_paths;
    RealEnvironment::run_test_with_real_env(|env| {
      Box::pin(async move {
        let tarball = create_test_npm_tarball_raw_paths(&[("./package/plugin.wasm", b"wasm-bytes"), ("./package/extra.bin", b"extra")]);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("extracted");

        extract_tarball_to_dir(&tarball, &dest, &env).unwrap();
        assert_eq!(env.read_file_bytes(dest.join("plugin.wasm")).unwrap(), b"wasm-bytes");
        assert_eq!(env.read_file_bytes(dest.join("extra.bin")).unwrap(), b"extra");
      })
    });
  }

  #[test]
  fn extract_tarball_rejects_absolute_path_entries() {
    // a tarball entry whose first component is `/` would otherwise pin the
    // wrapper to "/" and write the entry's tail under output_dir, mis-shaping
    // the extract. Reject before that mistake can take effect.
    use crate::environment::RealEnvironment;
    use crate::test_helpers::create_test_npm_tarball_raw_paths;
    RealEnvironment::run_test_with_real_env(|env| {
      Box::pin(async move {
        let tarball = create_test_npm_tarball_raw_paths(&[("/etc/passwd", b"pwned")]);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("extracted");

        let err = extract_tarball_to_dir(&tarball, &dest, &env).unwrap_err();
        assert!(err.to_string().contains("non-relative top-level component"), "got: {}", err);
      })
    });
  }

  use crate::test_helpers::create_test_npm_tarball as create_test_tarball;

  #[test]
  fn extract_tarball_is_idempotent_when_dest_dir_exists() {
    // a second call must not re-extract; trusting that name@version is
    // immutable means we can fast-path-skip if dest_dir already exists.
    use crate::environment::RealEnvironment;
    RealEnvironment::run_test_with_real_env(|env| {
      Box::pin(async move {
        let tarball = create_test_tarball(&[("package/plugin.wasm", b"first-extract")]);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("extracted");

        extract_tarball_to_dir(&tarball, &dest, &env).unwrap();
        assert_eq!(env.read_file_bytes(dest.join("plugin.wasm")).unwrap(), b"first-extract");

        // a second call with different bytes must NOT overwrite — we trust
        // dest_dir's existence to mean "already extracted"
        let different = create_test_tarball(&[("package/plugin.wasm", b"second-extract")]);
        extract_tarball_to_dir(&different, &dest, &env).unwrap();
        assert_eq!(
          env.read_file_bytes(dest.join("plugin.wasm")).unwrap(),
          b"first-extract",
          "dest_dir should not be re-extracted"
        );

        // and there shouldn't be any leftover temp dirs
        let leftover: Vec<_> = env
          .dir_info(dir.path())
          .unwrap()
          .into_iter()
          .map(|entry| match entry {
            crate::environment::DirEntry::Directory(path) | crate::environment::DirEntry::File { path, .. } => path,
          })
          .filter(|path| path.file_name().unwrap().to_string_lossy().contains(".tmp"))
          .collect();
        assert!(leftover.is_empty(), "expected no .tmp leftover, got {leftover:?}");
      })
    });
  }

  #[test]
  fn extract_tarball_fast_paths_when_dest_dir_exists_from_a_prior_extract() {
    // dest_dir is already populated (e.g. from a previous extract or a
    // concurrent winner that finished before we even started). The initial
    // path_exists check must short-circuit and leave the existing contents
    // alone.
    use crate::environment::RealEnvironment;
    RealEnvironment::run_test_with_real_env(|env| {
      Box::pin(async move {
        let tarball = create_test_tarball(&[("package/plugin.wasm", b"second-attempt")]);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("extracted");

        env.mk_dir_all(&dest).unwrap();
        env.write_file_bytes(dest.join("plugin.wasm"), b"winner").unwrap();

        extract_tarball_to_dir(&tarball, &dest, &env).unwrap();
        assert_eq!(env.read_file_bytes(dest.join("plugin.wasm")).unwrap(), b"winner");

        // no temp dir orphans
        let leftover: Vec<_> = env
          .dir_info(dir.path())
          .unwrap()
          .into_iter()
          .map(|entry| match entry {
            crate::environment::DirEntry::Directory(path) | crate::environment::DirEntry::File { path, .. } => path,
          })
          .filter(|path| path.file_name().unwrap().to_string_lossy().contains(".tmp"))
          .collect();
        assert!(leftover.is_empty(), "expected no .tmp leftover, got {leftover:?}");
      })
    });
  }

  /// Test-only seam to exercise the rename-race fallback in
  /// `extract_tarball_to_dir`. Production has no business calling this — the
  /// fast-path skip on existing `dest_dir` is part of the correctness story
  /// — so it lives here under `#[cfg(test)]` rather than as a module-private
  /// function that future production code could pick up by accident.
  fn extract_tarball_skipping_existence_check<E: crate::environment::PluginEnvironment>(tarball_bytes: &[u8], dest_dir: &Path, environment: &E) -> Result<()> {
    use crate::utils::fs::get_atomic_path;
    let temp_dir = get_atomic_path(environment, dest_dir);
    environment.mk_dir_all(&temp_dir)?;
    if let Err(err) = extract_tarball_to_dir_inner(tarball_bytes, &temp_dir, environment) {
      let _ = environment.remove_dir_all(&temp_dir);
      return Err(err);
    }
    match environment.rename(&temp_dir, dest_dir) {
      Ok(()) => Ok(()),
      Err(err) => {
        if environment.path_exists(dest_dir) {
          let _ = environment.remove_dir_all(&temp_dir);
          Ok(())
        } else {
          let _ = environment.remove_dir_all(&temp_dir);
          Err(err.into())
        }
      }
    }
  }

  #[test]
  fn extract_tarball_falls_back_when_rename_loses_to_a_concurrent_extract() {
    // exercise the path where the initial path_exists check returned false
    // but dest_dir is populated by another extract before our rename runs.
    // we call the test seam directly to skip the fast-path check and pre-
    // populate dest_dir so the rename collides — the function must accept
    // the winner and clean up its own temp dir rather than erroring.
    use crate::environment::RealEnvironment;
    RealEnvironment::run_test_with_real_env(|env| {
      Box::pin(async move {
        let tarball = create_test_tarball(&[("package/plugin.wasm", b"loser")]);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("extracted");

        // a winning concurrent extract has populated dest_dir with content.
        // a non-empty dest is what makes rename of our temp dir collide.
        env.mk_dir_all(&dest).unwrap();
        env.write_file_bytes(dest.join("plugin.wasm"), b"winner").unwrap();

        extract_tarball_skipping_existence_check(&tarball, &dest, &env).unwrap();
        // winner's contents preserved
        assert_eq!(env.read_file_bytes(dest.join("plugin.wasm")).unwrap(), b"winner");

        // our temp dir was cleaned up
        let leftover: Vec<_> = env
          .dir_info(dir.path())
          .unwrap()
          .into_iter()
          .map(|entry| match entry {
            crate::environment::DirEntry::Directory(path) | crate::environment::DirEntry::File { path, .. } => path,
          })
          .filter(|path| path.file_name().unwrap().to_string_lossy().contains(".tmp"))
          .collect();
        assert!(leftover.is_empty(), "expected no .tmp leftover, got {leftover:?}");
      })
    });
  }

  /// Stages an npm registry tarball at the default registry so
  /// `try_resolve_process_plugin_per_platform_binary` can fetch it. Returns
  /// the tarball's SHA-256 so callers can plug it into plugin.json.
  fn stage_per_platform_npm_package(environment: &crate::environment::TestEnvironment, name: &str, version: &str, files: &[(&str, &[u8])]) -> String {
    let tarball = create_test_tarball(files);
    let checksum = get_sha256_checksum(&tarball);
    let packument = serde_json::json!({
      "versions": {
        version: { "dist": { "tarball": format!("https://registry.npmjs.org/{name}/-/{name}-{version}.tgz") } }
      }
    });
    environment.add_remote_file_bytes(&format!("https://registry.npmjs.org/{name}"), packument.to_string().into_bytes());
    environment.add_remote_file_bytes(&format!("https://registry.npmjs.org/{name}/-/{name}-{version}.tgz"), tarball);
    checksum
  }

  #[tokio::test]
  async fn resolve_npm_from_node_modules_process_plugin_aarch64_falls_back_to_x86_64() {
    // a process plugin manifest that only ships linux-x86_64 should still
    // resolve on linux/aarch64 — matches the HTTPS path's behavior.
    use crate::environment::TestEnvironment;
    let environment = TestEnvironment::new();
    environment.set_os("linux");
    environment.set_cpu_arch("aarch64");

    // the per-platform package ships the executable inside its tarball.
    // The checksum in plugin.json is the tarball's SHA-256, not the binary's.
    let tarball_checksum = stage_per_platform_npm_package(&environment, "foo-linux-x86_64", "1.0.0", &[("package/foo", b"fake-binary-contents")]);

    // the manifest only lists linux-x86_64; the aarch64 host should still resolve it
    let manifest = serde_json::json!({
      "schemaVersion": 2,
      "name": "foo",
      "version": "1.0.0",
      "linux-x86_64": {
        "reference": "npm:foo-linux-x86_64@1.0.0/foo",
        "checksum": tarball_checksum,
      },
    });
    environment.mk_dir_all("/node_modules/foo").unwrap();
    environment.write_file("/node_modules/foo/plugin.json", &manifest.to_string()).unwrap();

    let specifier = NpmSpecifier {
      name: "foo".to_string(),
      version: None,
      path: "plugin.json".to_string(),
    };
    let resolved = resolve_npm_from_node_modules(&specifier, std::path::Path::new("/"), &environment)
      .await
      .unwrap();
    let tarball = resolved.pre_resolved_tarball.expect("process plugin should have a pre-resolved tarball");
    assert_eq!(tarball.name, "foo");
    assert_eq!(tarball.version, "1.0.0");
    assert_eq!(tarball.executable_sub_path, "foo");
    // tarball bytes are passed straight through (verified against the
    // checksum); we don't extract here, so we just sanity-check non-emptiness.
    assert!(!tarball.tarball_bytes.is_empty());
  }

  #[tokio::test]
  async fn resolve_npm_from_node_modules_process_plugin_rejects_unversioned_reference() {
    // plugin.json must pin a version for its per-platform npm reference —
    // we need a specific tarball to verify against the checksum.
    use crate::environment::TestEnvironment;
    let environment = TestEnvironment::new();
    environment.set_os("linux");
    environment.set_cpu_arch("x86_64");

    let manifest = serde_json::json!({
      "schemaVersion": 2,
      "name": "foo",
      "version": "1.0.0",
      "linux-x86_64": {
        "reference": "npm:foo-linux-x86_64/foo",
        "checksum": "0".repeat(64),
      },
    });
    environment.mk_dir_all("/node_modules/foo").unwrap();
    environment.write_file("/node_modules/foo/plugin.json", &manifest.to_string()).unwrap();

    let specifier = NpmSpecifier {
      name: "foo".to_string(),
      version: None,
      path: "plugin.json".to_string(),
    };
    let err = match resolve_npm_from_node_modules(&specifier, std::path::Path::new("/"), &environment).await {
      Ok(_) => panic!("expected an error"),
      Err(e) => e,
    };
    let msg = format!("{err:#}");
    assert!(msg.contains("must include a version"), "got: {msg}");
    assert!(msg.contains("npm:foo-linux-x86_64/foo"), "got: {msg}");
  }

  #[tokio::test]
  async fn resolve_npm_from_node_modules_process_plugin_rejects_tarball_checksum_mismatch() {
    // plugin.json's checksum doesn't match the fetched tarball — surface a
    // clear "invalid checksum" error so the plugin author / user can tell
    // what broke.
    use crate::environment::TestEnvironment;
    let environment = TestEnvironment::new();
    environment.set_os("linux");
    environment.set_cpu_arch("x86_64");

    let _real_checksum = stage_per_platform_npm_package(&environment, "foo-linux-x86_64", "1.0.0", &[("package/foo", b"binary")]);
    let bogus_checksum = "0".repeat(64);

    let manifest = serde_json::json!({
      "schemaVersion": 2,
      "name": "foo",
      "version": "1.0.0",
      "linux-x86_64": {
        "reference": "npm:foo-linux-x86_64@1.0.0/foo",
        "checksum": bogus_checksum,
      },
    });
    environment.mk_dir_all("/node_modules/foo").unwrap();
    environment.write_file("/node_modules/foo/plugin.json", &manifest.to_string()).unwrap();

    let specifier = NpmSpecifier {
      name: "foo".to_string(),
      version: None,
      path: "plugin.json".to_string(),
    };
    let err = match resolve_npm_from_node_modules(&specifier, std::path::Path::new("/"), &environment).await {
      Ok(_) => panic!("expected an error"),
      Err(e) => e,
    };
    let msg = format!("{err:#}");
    assert!(msg.contains("Invalid checksum"), "got: {msg}");
    assert!(msg.contains("foo-linux-x86_64"), "got: {msg}");
  }

  #[tokio::test]
  async fn resolve_npm_from_node_modules_process_plugin_rejects_bad_schema_version() {
    use crate::environment::TestEnvironment;
    let environment = TestEnvironment::new();
    let manifest = serde_json::json!({
      "schemaVersion": 1,
      "name": "foo",
      "version": "1.0.0",
    });
    environment.mk_dir_all("/node_modules/foo").unwrap();
    environment.write_file("/node_modules/foo/plugin.json", &manifest.to_string()).unwrap();
    let specifier = NpmSpecifier {
      name: "foo".to_string(),
      version: None,
      path: "plugin.json".to_string(),
    };
    let err = match resolve_npm_from_node_modules(&specifier, std::path::Path::new("/"), &environment).await {
      Ok(_) => panic!("expected an error"),
      Err(e) => e,
    };
    assert!(
      err.to_string().contains("schema version") || format!("{err:#}").contains("schema version"),
      "expected schema-version error, got: {err:#}"
    );
  }

  #[tokio::test]
  async fn resolve_npm_from_node_modules_process_plugin_rejects_https_reference() {
    // an npm-installed process plugin can't reference its platform binary
    // over the network — the whole point of installing via npm is to avoid
    // surprise HTTP fetches at format time.
    use crate::environment::TestEnvironment;
    let environment = TestEnvironment::new();
    environment.set_os("linux");
    environment.set_cpu_arch("x86_64");
    let manifest = serde_json::json!({
      "schemaVersion": 2,
      "name": "foo",
      "version": "1.0.0",
      "linux-x86_64": {
        "reference": "https://example.com/foo-linux-x86_64.zip",
        "checksum": "deadbeef",
      },
    });
    environment.mk_dir_all("/node_modules/foo").unwrap();
    environment.write_file("/node_modules/foo/plugin.json", &manifest.to_string()).unwrap();
    let specifier = NpmSpecifier {
      name: "foo".to_string(),
      version: None,
      path: "plugin.json".to_string(),
    };
    let err = match resolve_npm_from_node_modules(&specifier, std::path::Path::new("/"), &environment).await {
      Ok(_) => panic!("expected an error"),
      Err(e) => e,
    };
    let msg = format!("{err:#}");
    assert!(msg.contains("Network references aren't allowed"), "got: {msg}");
    assert!(msg.contains("https://example.com/foo-linux-x86_64.zip"), "got: {msg}");
  }

  #[tokio::test]
  async fn resolve_npm_from_registry_suggests_plugin_json_when_wasm_missing() {
    // user wrote `npm:@dprint/exec@0.6.2` (defaulting to plugin.wasm) but the
    // package ships a plugin.json instead. The error should tell them to
    // reference plugin.json, not just "Is the package a dprint plugin?".
    use crate::environment::TestEnvironment;
    let environment = TestEnvironment::new();
    let tarball = create_test_tarball(&[("package/plugin.json", br#"{"schemaVersion":2,"name":"exec","version":"0.6.2"}"#)]);
    let tarball_checksum = get_sha256_checksum(&tarball);
    environment.add_remote_file_bytes("https://registry.npmjs.org/@dprint/exec/-/exec-0.6.2.tgz", tarball);

    let specifier = NpmSpecifier {
      name: "@dprint/exec".to_string(),
      version: Some("0.6.2".to_string()),
      path: "plugin.wasm".to_string(),
    };
    let err = match resolve_npm_from_registry(
      ResolveNpmRegistryOptions {
        specifier: &specifier,
        checksum: Some(&tarball_checksum),
        detect_path: false,
        establish_checksum: false,
      },
      &environment,
    )
    .await
    {
      Ok(_) => panic!("expected an error"),
      Err(e) => e,
    };
    let msg = format!("{err:#}");
    assert!(msg.contains("plugin.json instead"), "got: {msg}");
    assert!(msg.contains("npm:@dprint/exec@0.6.2/plugin.json"), "got: {msg}");
  }

  #[test]
  fn find_npm_plugin_local_path_suggests_plugin_json_when_wasm_missing() {
    use crate::environment::TestEnvironment;
    let environment = TestEnvironment::new();
    environment.mk_dir_all("/node_modules/@dprint/exec").unwrap();
    environment
      .write_file(
        "/node_modules/@dprint/exec/plugin.json",
        r#"{"schemaVersion":2,"name":"exec","version":"0.6.2"}"#,
      )
      .unwrap();

    let specifier = NpmSpecifier {
      name: "@dprint/exec".to_string(),
      version: None,
      path: "plugin.wasm".to_string(),
    };
    let err = find_npm_plugin_local_path(&specifier, std::path::Path::new("/"), &environment).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("plugin.json instead"), "got: {msg}");
    // unversioned suggestion should not pin a version
    assert!(msg.contains("npm:@dprint/exec/plugin.json"), "got: {msg}");
  }

  #[test]
  fn find_npm_plugin_local_path_no_alternate_when_neither_present() {
    // package directory exists but contains nothing useful — falls back to
    // the generic "Is the package a dprint plugin?" message.
    use crate::environment::TestEnvironment;
    let environment = TestEnvironment::new();
    environment.mk_dir_all("/node_modules/foo").unwrap();
    environment.write_file("/node_modules/foo/README.md", "hi").unwrap();

    let specifier = NpmSpecifier {
      name: "foo".to_string(),
      version: None,
      path: "plugin.wasm".to_string(),
    };
    let err = find_npm_plugin_local_path(&specifier, std::path::Path::new("/"), &environment).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("Is the package a dprint plugin?"), "got: {msg}");
    assert!(!msg.contains("instead"), "got: {msg}");
  }

  #[tokio::test]
  async fn resolve_npm_from_node_modules_missing_package_suggests_versioned_specifier_wasm() {
    // user wrote `npm:@dprint/typescript` but hasn't run `npm install`. The
    // error should not just say "npm install" — it should also offer a
    // ready-to-paste `npm:<name>@<version>` alternative. For wasm plugins we
    // skip the checksum (it's optional and would be noise).
    use crate::environment::TestEnvironment;
    let environment = TestEnvironment::new();
    environment.add_npm_latest_version("@dprint/typescript", "0.99.0");

    let specifier = NpmSpecifier {
      name: "@dprint/typescript".to_string(),
      version: None,
      path: "plugin.wasm".to_string(),
    };
    let err = match resolve_npm_from_node_modules(&specifier, std::path::Path::new("/"), &environment).await {
      Ok(_) => panic!("expected an error"),
      Err(e) => e,
    };
    let msg = format!("{err:#}");
    assert!(msg.contains("Could not find @dprint/typescript in node_modules"), "got: {msg}");
    assert!(msg.contains("npm install @dprint/typescript"), "got: {msg}");
    // suggestion includes the resolved latest version, no checksum for wasm
    assert!(msg.contains("npm:@dprint/typescript@0.99.0"), "got: {msg}");
    assert!(!msg.contains("npm:@dprint/typescript@0.99.0@"), "wasm shouldn't carry a checksum: {msg}");
  }

  #[tokio::test]
  async fn resolve_npm_from_node_modules_missing_package_suggests_versioned_specifier_process() {
    // for process plugins (path is plugin.json), keep the path component in
    // the suggestion. The checksum requirement is surfaced by the next
    // resolve step (which quotes the exact SHA to use), so we don't bake it
    // into this message.
    use crate::environment::TestEnvironment;
    let environment = TestEnvironment::new();
    environment.add_npm_latest_version("@dprint/exec", "0.6.2");
    // intentionally no tarball mock — confirms we don't fetch it for the suggestion

    let specifier = NpmSpecifier {
      name: "@dprint/exec".to_string(),
      version: None,
      path: "plugin.json".to_string(),
    };
    let err = match resolve_npm_from_node_modules(&specifier, std::path::Path::new("/"), &environment).await {
      Ok(_) => panic!("expected an error"),
      Err(e) => e,
    };
    let msg = format!("{err:#}");
    assert!(msg.contains("Could not find @dprint/exec in node_modules"), "got: {msg}");
    assert!(msg.contains("npm:@dprint/exec@0.6.2/plugin.json"), "got: {msg}");
    // no checksum appended — the next step explains that requirement
    assert!(!msg.contains("npm:@dprint/exec@0.6.2/plugin.json@"), "got: {msg}");
  }

  #[tokio::test]
  async fn resolve_npm_from_node_modules_missing_package_falls_back_when_registry_unreachable() {
    // the registry is unreachable (no mocks set up) — the error degrades
    // gracefully to the bare "npm install" hint rather than swallowing
    // the original miss with a confusing network error.
    use crate::environment::TestEnvironment;
    let environment = TestEnvironment::new();

    let specifier = NpmSpecifier {
      name: "@dprint/typescript".to_string(),
      version: None,
      path: "plugin.wasm".to_string(),
    };
    let err = match resolve_npm_from_node_modules(&specifier, std::path::Path::new("/"), &environment).await {
      Ok(_) => panic!("expected an error"),
      Err(e) => e,
    };
    let msg = format!("{err:#}");
    assert!(msg.contains("Could not find @dprint/typescript in node_modules"), "got: {msg}");
    assert!(msg.contains("npm install @dprint/typescript"), "got: {msg}");
    // no version suggestion when we couldn't reach the registry
    assert!(!msg.contains("OR specify a version (ex."), "got: {msg}");
  }

  #[tokio::test]
  async fn resolve_npm_from_node_modules_process_plugin_rejects_non_process_kind() {
    use crate::environment::TestEnvironment;
    let environment = TestEnvironment::new();
    let manifest = serde_json::json!({
      "schemaVersion": 2,
      "kind": "other",
      "name": "foo",
      "version": "1.0.0",
    });
    environment.mk_dir_all("/node_modules/foo").unwrap();
    environment.write_file("/node_modules/foo/plugin.json", &manifest.to_string()).unwrap();
    let specifier = NpmSpecifier {
      name: "foo".to_string(),
      version: None,
      path: "plugin.json".to_string(),
    };
    let err = match resolve_npm_from_node_modules(&specifier, std::path::Path::new("/"), &environment).await {
      Ok(_) => panic!("expected an error"),
      Err(e) => e,
    };
    assert!(
      format!("{err:#}").contains("Unsupported plugin kind: other"),
      "expected unsupported-kind error, got: {err:#}"
    );
  }

  #[cfg(unix)]
  #[test]
  fn extract_tarball_preserves_exec_bits_via_env_set_permissions() {
    use crate::environment::RealEnvironment;
    use crate::test_helpers::create_test_npm_tarball_with_modes;
    use sys_traits::FsMetadata;
    use sys_traits::FsMetadataValue;
    RealEnvironment::run_test_with_real_env(|env| {
      Box::pin(async move {
        let tarball = create_test_npm_tarball_with_modes(&[("package/plugin.wasm", b"wasm", 0o644), ("package/scripts/run.sh", b"#!/bin/sh\n", 0o755)]);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("extracted");

        extract_tarball_to_dir(&tarball, &dest, &env).unwrap();

        let exec_mode = env.fs_metadata(dest.join("scripts").join("run.sh")).unwrap().mode().unwrap() & 0o777;
        assert_eq!(exec_mode, 0o755, "expected exec bits preserved");
        let plain_mode = env.fs_metadata(dest.join("plugin.wasm")).unwrap().mode().unwrap() & 0o777;
        // 0o644 is the default — set_permissions is skipped for it
        assert_eq!(plain_mode, 0o644);
      })
    });
  }
}
