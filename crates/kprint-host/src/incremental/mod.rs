mod incremental_file;

pub use incremental_file::FileMetadata;
pub use incremental_file::IncrementalFile;

use crate::configuration::ResolvedConfig;
use crate::environment::HostEnvironment as Environment;
use crate::resolution::PluginsScope;
use crate::utils::get_bytes_hash;

/// Bumped when the incremental file's format changes.
const INCREMENTAL_FILE_VERSION: usize = 1;

/// Upstream dprint keeps its incremental files directly in `incremental`.
pub fn incremental_dir(environment: &impl Environment) -> crate::environment::CanonicalizedPathBuf {
  environment
    .get_cache_dir()
    .join_panic_relative("incremental")
    .join_panic_relative(format!("v{INCREMENTAL_FILE_VERSION}"))
}

pub struct GetIncrementalFileOptions {
  pub incremental_cli_arg: Option<bool>,
  /// Whether the cli arguments limit the run to some of the files the
  /// configuration would otherwise cover (ex. file paths were specified).
  pub is_partial_run: bool,
}

pub fn get_incremental_file<TEnvironment: Environment>(
  options: GetIncrementalFileOptions,
  config: &ResolvedConfig,
  scope: &PluginsScope<TEnvironment>,
  environment: &TEnvironment,
) -> Option<IncrementalFile<TEnvironment>> {
  let incremental_cli_arg = options.incremental_cli_arg;
  if let Some(incremental_arg) = incremental_cli_arg.or(config.execution.incremental)
    && !incremental_arg
  {
    return None;
  }

  // the incremental file is stored in the cache with a key based on the root directory
  let incremental_dir = incremental_dir(environment);
  if environment.mk_dir_all(&incremental_dir).is_err() {
    return None;
  }

  let base_path = config.origin.base_path.clone();
  let file_path = incremental_dir.join_panic_relative(get_bytes_hash(base_path.to_string_lossy().as_bytes()).to_string());
  // running from a sub directory of the config only traverses that directory
  // (see paths.rs), so that's also a partial run
  let cwd = environment.cwd();
  let is_in_sub_dir = cwd != base_path && cwd.starts_with(&base_path);
  Some(IncrementalFile::new(
    file_path,
    scope.plugins_hash(),
    options.is_partial_run || is_in_sub_dir,
    environment.clone(),
  ))
}
