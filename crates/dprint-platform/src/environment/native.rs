use anyhow::Result;
use anyhow::bail;
use once_cell::sync::Lazy;
use once_cell::sync::OnceCell;
use parking_lot::Mutex;
use std::borrow::Cow;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::io::Read;
use std::io::Write;
use std::num::NonZeroUsize;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::SystemTime;
use sys_traits::BaseEnvVar;
use sys_traits::BaseFsCreateDir;
use sys_traits::BaseFsMetadata;
use sys_traits::BaseFsOpen;
use sys_traits::BaseFsRead;
use sys_traits::BaseFsRemoveFile;
use sys_traits::BaseFsRename;
use sys_traits::BaseFsSetPermissions;
use sys_traits::CreateDirOptions;
use sys_traits::SystemRandom;
use sys_traits::SystemTimeNow;
use sys_traits::ThreadSleep;
use sys_traits::impls::RealSys;
use sysinfo::System;
use url::Url;

use dprint_async_runtime::async_trait;

use super::CanonicalizedPathBuf;
use super::DirEntry;
use super::DownloadedFile;
use super::FilePermissions;
use super::PathKind;
use super::UrlDownloader;
use crate::compiler::CompilationResult;
use crate::environment::*;
use crate::utils::LogLevel;
use crate::utils::MultiSelectItem;
use crate::utils::ProgressReporter;
use crate::utils::ShowConfirmStrategy;

// cache the cwd because it's much faster than looking it up each time
static CACHED_CWD: OnceCell<CanonicalizedPathBuf> = OnceCell::new();
// cache the global gitignore path because resolving it spawns a git subprocess
// and the path is stable for the process (used by the lsp and editor-service,
// which rebuild their file matchers on every config change)
static CACHED_GLOBAL_GITIGNORE_PATH: OnceCell<Option<PathBuf>> = OnceCell::new();

/// Native operating-system capabilities with frontend services supplied by the caller.
/// Configuration and discovery can use `HeadlessServices` without a terminal,
/// network client, or Wasm compiler.
#[derive(Clone)]
pub struct NativeEnvironment<S: NativeServices> {
  services: S,
  version: String,
  system: Arc<Mutex<System>>,
}

impl<S: NativeServices> NativeEnvironment<S> {
  pub fn new(services: S, version: impl Into<String>) -> Result<Self> {
    if let Err(err) = (*CACHE_DIR).as_ref() {
      bail!("Error creating cache directory: {:#}", err);
    }
    Ok(Self {
      services,
      version: version.into(),
      system: Default::default(),
    })
  }

  /// Expands a leading `~` to the user's home directory, mirroring how git
  /// expands `core.excludesFile`.
  fn expand_user_path(&self, value: &str) -> Option<PathBuf> {
    if value == "~" {
      Some(self.get_home_dir()?.into_path_buf())
    } else if let Some(rest) = value.strip_prefix("~/") {
      Some(self.get_home_dir()?.join(rest))
    } else {
      Some(PathBuf::from(value))
    }
  }

  /// Resolves git's global excludes file path. Prefer the cached
  /// `global_gitignore_path` over calling this directly, as this spawns a git
  /// subprocess.
  fn resolve_global_gitignore_path(&self) -> Option<PathBuf> {
    // prefer the path configured via `git config core.excludesFile`
    let configured = Command::new("git")
      .arg("config")
      .arg("--get")
      .arg("core.excludesFile")
      .output()
      .ok()
      .filter(|output| output.status.success())
      .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
      .filter(|value| !value.is_empty());

    match configured {
      Some(value) => self.expand_user_path(&value),
      None => {
        // git's default location when `core.excludesFile` is unset
        let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
          Some(xdg) => PathBuf::from(xdg),
          None => self.get_home_dir()?.join(".config"),
        };
        Some(base.join("git").join("ignore"))
      }
    }
  }
}

impl<S: NativeServices> std::fmt::Debug for NativeEnvironment<S> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("RealEnvironment").finish()
  }
}

impl<S: NativeServices> BaseFsCreateDir for NativeEnvironment<S> {
  fn base_fs_create_dir(&self, path: &Path, options: &CreateDirOptions) -> io::Result<()> {
    RealSys.base_fs_create_dir(path, options)
  }
}

impl<S: NativeServices> BaseEnvVar for NativeEnvironment<S> {
  fn base_env_var_os(&self, key: &OsStr) -> Option<OsString> {
    RealSys.base_env_var_os(key)
  }
}

impl<S: NativeServices> BaseFsMetadata for NativeEnvironment<S> {
  type Metadata = sys_traits::impls::RealFsMetadata;

  fn base_fs_metadata(&self, path: &Path) -> io::Result<Self::Metadata> {
    RealSys.base_fs_metadata(path)
  }

  fn base_fs_symlink_metadata(&self, path: &Path) -> io::Result<Self::Metadata> {
    RealSys.base_fs_symlink_metadata(path)
  }
}

impl<S: NativeServices> BaseFsOpen for NativeEnvironment<S> {
  type File = sys_traits::impls::RealFsFile;

  fn base_fs_open(&self, path: &Path, options: &sys_traits::OpenOptions) -> io::Result<Self::File> {
    RealSys.base_fs_open(path, options)
  }
}

impl<S: NativeServices> BaseFsRead for NativeEnvironment<S> {
  fn base_fs_read(&self, path: &Path) -> io::Result<Cow<'static, [u8]>> {
    RealSys.base_fs_read(path)
  }
}

impl<S: NativeServices> BaseFsRemoveFile for NativeEnvironment<S> {
  fn base_fs_remove_file(&self, path: &Path) -> io::Result<()> {
    RealSys.base_fs_remove_file(path)
  }
}

impl<S: NativeServices> BaseFsRename for NativeEnvironment<S> {
  fn base_fs_rename(&self, from: &Path, to: &Path) -> io::Result<()> {
    RealSys.base_fs_rename(from, to)
  }
}

impl<S: NativeServices> BaseFsSetPermissions for NativeEnvironment<S> {
  fn base_fs_set_permissions(&self, path: &Path, mode: u32) -> io::Result<()> {
    RealSys.base_fs_set_permissions(path, mode)
  }
}

impl<S: NativeServices> ThreadSleep for NativeEnvironment<S> {
  fn thread_sleep(&self, duration: std::time::Duration) {
    std::thread::sleep(duration);
  }
}

impl<S: NativeServices> SystemRandom for NativeEnvironment<S> {
  fn sys_random(&self, buf: &mut [u8]) -> io::Result<()> {
    use rand::RngCore;
    rand::rng().fill_bytes(buf);
    Ok(())
  }
}

impl<S: NativeServices> SystemTimeNow for NativeEnvironment<S> {
  fn sys_time_now(&self) -> SystemTime {
    SystemTime::now()
  }
}

impl<S: NativeServices> crate::environment::EnvironmentVariables for NativeEnvironment<S> {
  fn env_var(&self, name: &str) -> Option<OsString> {
    std::env::var_os(name)
  }
}

impl<S: NativeServices> crate::environment::FileSystemEnvironment for NativeEnvironment<S> {
  fn atomic_write_file_bytes(&self, file_path: impl AsRef<Path>, bytes: &[u8]) -> io::Result<()> {
    crate::utils::fs::atomic_write_file_with_retries(self, file_path.as_ref(), bytes, 0o644)
  }

  fn read_file(&self, file_path: impl AsRef<Path>) -> io::Result<String> {
    let bytes = self.read_file_bytes(file_path.as_ref())?;
    String::from_utf8(bytes).map_err(|err| {
      io::Error::new(
        io::ErrorKind::InvalidData,
        format!("Error converting file to utf8 {}: {:#}", file_path.as_ref().display(), err),
      )
    })
  }

  fn read_file_bytes(&self, file_path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    log_debug!(self, "Reading file: {}", file_path.as_ref().display());
    #[allow(clippy::disallowed_methods)]
    match fs::read(&file_path) {
      Ok(bytes) => Ok(bytes),
      Err(err) => Err(io::Error::new(
        err.kind(),
        format!("Error reading file {}: {:#}", file_path.as_ref().display(), err),
      )),
    }
  }

  fn write_file_bytes(&self, file_path: impl AsRef<Path>, bytes: &[u8]) -> io::Result<()> {
    log_debug!(self, "Writing file: {}", file_path.as_ref().display());
    #[allow(clippy::disallowed_methods)]
    match fs::write(&file_path, bytes) {
      Ok(_) => Ok(()),
      Err(err) => Err(io::Error::new(
        err.kind(),
        format!("Error writing file '{}': {:#}", file_path.as_ref().display(), err),
      )),
    }
  }

  fn rename(&self, path_from: impl AsRef<Path>, path_to: impl AsRef<Path>) -> io::Result<()> {
    log_debug!(self, "Renaming {} -> {}", path_from.as_ref().display(), path_to.as_ref().display());
    #[allow(clippy::disallowed_methods)]
    fs::rename(&path_from, &path_to).map_err(|err| {
      io::Error::new(
        err.kind(),
        format!(
          "Error renaming '{}' to '{}': {:#}",
          path_from.as_ref().display(),
          path_to.as_ref().display(),
          err
        ),
      )
    })
  }

  fn remove_file(&self, file_path: impl AsRef<Path>) -> io::Result<()> {
    log_debug!(self, "Deleting file: {}", file_path.as_ref().display());
    #[allow(clippy::disallowed_methods)]
    match fs::remove_file(&file_path) {
      Ok(_) => Ok(()),
      Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
      Err(err) => Err(io::Error::new(
        err.kind(),
        format!("Error deleting file '{}': {:#}", file_path.as_ref().display(), err),
      )),
    }
  }

  fn remove_dir_all(&self, dir_path: impl AsRef<Path>) -> io::Result<()> {
    log_debug!(self, "Deleting directory: {}", dir_path.as_ref().display());
    #[allow(clippy::disallowed_methods)]
    match fs::remove_dir_all(&dir_path) {
      Ok(_) => Ok(()),
      Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
      Err(err) => Err(io::Error::new(
        err.kind(),
        format!("Error deleting directory '{}': {:#}", dir_path.as_ref().display(), err),
      )),
    }
  }

  fn dir_info(&self, dir_path: impl AsRef<Path>) -> io::Result<Vec<DirEntry>> {
    let mut entries = Vec::new();

    #[allow(clippy::disallowed_methods)]
    let dir_info = std::fs::read_dir(&dir_path)?;

    for entry in dir_info {
      let entry = entry?;
      let file_type = entry.file_type()?;
      if file_type.is_dir() {
        entries.push(DirEntry::Directory(entry.path()));
      } else if file_type.is_file() {
        entries.push(DirEntry::File {
          name: entry.file_name(),
          path: entry.path(),
        });
      }
    }

    Ok(entries)
  }

  fn scan_file_system(&self) -> Arc<dyn tree_fucker::FileSystem> {
    Arc::new(tree_fucker::std_fs::StdFileSystem::new())
  }

  fn path_exists(&self, file_path: impl AsRef<Path>) -> bool {
    log_debug!(self, "Checking path exists: {}", file_path.as_ref().display());
    #[allow(clippy::disallowed_methods)]
    file_path.as_ref().exists()
  }

  fn path_is_file(&self, file_path: impl AsRef<Path>) -> bool {
    log_debug!(self, "Checking path is file: {}", file_path.as_ref().display());
    #[allow(clippy::disallowed_methods)]
    file_path.as_ref().is_file()
  }

  fn path_kind(&self, file_path: impl AsRef<Path>) -> Option<PathKind> {
    log_debug!(self, "Statting path: {}", file_path.as_ref().display());
    #[allow(clippy::disallowed_methods)]
    let file_type = file_path.as_ref().symlink_metadata().ok()?.file_type();
    Some(if file_type.is_symlink() {
      PathKind::Symlink
    } else if file_type.is_dir() {
      PathKind::Dir
    } else {
      PathKind::File
    })
  }

  fn canonicalize(&self, path: impl AsRef<Path>) -> io::Result<CanonicalizedPathBuf> {
    canonicalize_path(path)
  }

  fn is_absolute_path(&self, path: impl AsRef<Path>) -> bool {
    path.as_ref().is_absolute()
  }

  fn file_permissions(&self, path: impl AsRef<Path>) -> io::Result<FilePermissions> {
    Ok(FilePermissions::Std(
      #[allow(clippy::disallowed_methods)]
      fs::metadata(&path)
        .map_err(|err| {
          io::Error::new(
            err.kind(),
            format!("Error getting file permissions for '{}': {:#}", path.as_ref().display(), err),
          )
        })?
        .permissions(),
    ))
  }

  fn set_file_permissions(&self, path: impl AsRef<Path>, permissions: FilePermissions) -> io::Result<()> {
    let permissions = match permissions {
      FilePermissions::Std(p) => p,
      _ => panic!("Programming error. Permissions did not contain an std permission."),
    };
    #[allow(clippy::disallowed_methods)]
    fs::set_permissions(&path, permissions).map_err(|err| {
      io::Error::new(
        err.kind(),
        format!("Error setting file permissions for '{}': {:#}", path.as_ref().display(), err),
      )
    })?;
    Ok(())
  }

  fn mk_dir_all(&self, path: impl AsRef<Path>) -> io::Result<()> {
    log_debug!(self, "Creating directory: {}", path.as_ref().display());
    #[allow(clippy::disallowed_methods)]
    match fs::create_dir_all(&path) {
      Ok(_) => Ok(()),
      Err(err) => Err(io::Error::new(
        err.kind(),
        format!("Error creating directory '{}': {:#}", path.as_ref().display(), err),
      )),
    }
  }

  fn cwd(&self) -> CanonicalizedPathBuf {
    CACHED_CWD
      .get_or_init(|| {
        #[allow(clippy::disallowed_methods)]
        self
          .canonicalize(std::env::current_dir().expect("Expected to get the current working directory."))
          .expect("expected to canonicalize the cwd")
      })
      .clone()
  }
}

impl<S: NativeServices> crate::environment::VcsEnvironment for NativeEnvironment<S> {
  fn get_staged_files(&self) -> Result<Vec<PathBuf>> {
    let output = Command::new("git")
      .arg("diff")
      .arg("--name-only")
      .arg("--relative")
      .arg("--staged")
      .arg("--diff-filter=ACMR")
      .output()?;

    Ok(String::from_utf8_lossy(&output.stdout).lines().map(PathBuf::from).collect())
  }

  fn get_dirty_files(&self) -> Result<Vec<PathBuf>> {
    // collect every file with uncommitted changes in the working directory:
    // unstaged tracked changes, staged tracked changes, and untracked files
    // that aren't gitignored. each is gathered the same way `get_staged_files`
    // gathers staged files so the behaviour (e.g. renamed files yielding their
    // new path, deletions being skipped) stays consistent.
    fn git_lines(args: &[&str]) -> Result<Vec<PathBuf>> {
      let output = Command::new("git").args(args).output()?;
      Ok(String::from_utf8_lossy(&output.stdout).lines().map(PathBuf::from).collect())
    }

    let mut files = Vec::new();
    let mut seen = HashSet::new();
    let groups = [
      // unstaged tracked changes
      git_lines(&["diff", "--name-only", "--relative", "--diff-filter=ACMR"])?,
      // staged tracked changes
      git_lines(&["diff", "--name-only", "--relative", "--staged", "--diff-filter=ACMR"])?,
      // untracked files that aren't gitignored
      git_lines(&["ls-files", "--others", "--exclude-standard"])?,
    ];
    for file in groups.into_iter().flatten() {
      if seen.insert(file.clone()) {
        files.push(file);
      }
    }
    Ok(files)
  }

  fn global_gitignore_path(&self) -> Option<PathBuf> {
    CACHED_GLOBAL_GITIGNORE_PATH.get_or_init(|| self.resolve_global_gitignore_path()).clone()
  }
}

impl<S: NativeServices> crate::environment::DirectoriesEnvironment for NativeEnvironment<S> {
  fn get_cache_dir(&self) -> CanonicalizedPathBuf {
    // ok to unwrap because this would have errored in the constructor
    (*CACHE_DIR.as_ref().unwrap()).clone()
  }
  fn get_config_dir(&self) -> Option<PathBuf> {
    dirs::config_dir()
  }
  fn get_home_dir(&self) -> Option<CanonicalizedPathBuf> {
    dirs::home_dir().map(|path| self.canonicalize(path).unwrap())
  }
}

impl<S: NativeServices> crate::environment::SystemEnvironment for NativeEnvironment<S> {
  fn is_real(&self) -> bool {
    true
  }
  fn cpu_arch(&self) -> String {
    std::env::consts::ARCH.to_string()
  }
  fn os(&self) -> String {
    let target = env!("TARGET");
    if target.contains("linux-musl") {
      "linux-musl".to_string()
    } else {
      std::env::consts::OS.to_string()
    }
  }
}

impl<S: NativeServices> crate::environment::ClockEnvironment for NativeEnvironment<S> {
  fn get_time_secs(&self) -> u64 {
    SystemTime::now().duration_since(std::time::SystemTime::UNIX_EPOCH).unwrap().as_secs()
  }
}

#[async_trait]
impl<S: NativeServices> crate::environment::ConcurrencyEnvironment for NativeEnvironment<S> {
  fn available_parallelism(&self) -> Option<NonZeroUsize> {
    std::thread::available_parallelism().ok()
  }
  async fn cpu_usage(&self) -> u8 {
    // the documentation recommends calling this twice in order
    // to get a more accurate cpu reading
    let system = self.system.clone();
    let Ok(system) = dprint_async_runtime::spawn_blocking(move || {
      {
        let mut system = system.lock();
        system.refresh_cpu_usage();
      }
      system
    })
    .await
    else {
      return 0;
    };

    // wait a duration that allows getting a more accurate cpu usage
    tokio::time::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL).await;

    dprint_async_runtime::spawn_blocking(move || {
      let mut system = system.lock();
      system.refresh_cpu_usage();
      let utilization = system.cpus().iter().map(|c| c.cpu_usage()).sum::<f32>() / system.cpus().len() as f32;
      if utilization > 101f32 {
        0 // something wrong, so just return 0 for "cannot figure out cpu usage"
      } else {
        utilization as u8
      }
    })
    .await
    .unwrap_or(0)
  }
}

impl<S: NativeServices> crate::environment::ProcessEnvironment for NativeEnvironment<S> {
  fn kill_processes_using_dir(&self, dir_path: impl AsRef<Path>) -> usize {
    let dir_path = dir_path.as_ref();
    let mut system = self.system.lock();
    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    let mut killed_pids = Vec::new();
    for process in system.processes().values() {
      let Some(exe) = process.exe() else {
        continue;
      };
      if exe.starts_with(dir_path) {
        log_debug!(self, "Killing process {} using executable: {}", process.pid(), exe.display());
        if process.kill() {
          killed_pids.push(process.pid());
        }
      }
    }

    // wait for the killed processes to actually exit so their executables are no
    // longer locked before the caller tries to delete them again. poll with a
    // timeout rather than `Process::wait`, which blocks indefinitely (e.g. on a
    // process we couldn't kill, or a zombie its real parent hasn't reaped yet).
    if !killed_pids.is_empty() {
      let mut remaining_polls = 100; // ~2s at 20ms per poll
      while remaining_polls > 0 {
        system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&killed_pids), true);
        if killed_pids.iter().all(|pid| system.process(*pid).is_none()) {
          break;
        }
        remaining_polls -= 1;
        std::thread::sleep(std::time::Duration::from_millis(20));
      }
    }

    killed_pids.len()
  }
  fn current_exe(&self) -> io::Result<PathBuf> {
    std::env::current_exe().map_err(|err| io::Error::new(err.kind(), format!("Error getting current executable: {:#}", err)))
  }
  fn run_command_get_status(&self, mut args: Vec<OsString>) -> io::Result<Option<i32>> {
    let command_name = args.remove(0);
    let command_path = which::which(command_name).map_err(|err| io::Error::new(io::ErrorKind::NotFound, err))?;
    std::process::Command::new(command_path).args(args).status().map(|s| s.code())
  }
  #[cfg(windows)]
  fn ensure_system_path(&self, directory_path: &str) -> io::Result<()> {
    use winreg::RegKey;
    use winreg::enums::*;
    log_debug!(self, "Ensuring '{}' is on the path.", directory_path);

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (env, _) = hkcu.create_subkey("Environment")?;
    let mut path: String = env.get_value("Path")?;

    // add to the path if it doesn't have this entry
    if !path.split(';').any(|p| p == directory_path) {
      if !path.is_empty() && !path.ends_with(';') {
        path.push(';')
      }
      path.push_str(directory_path);
      env.set_value("Path", &path)?;
    }
    Ok(())
  }
  #[cfg(windows)]
  fn remove_system_path(&self, directory_path: &str) -> io::Result<()> {
    use winreg::RegKey;
    use winreg::enums::*;
    log_debug!(self, "Ensuring '{}' is on the path.", directory_path);

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (env, _) = hkcu.create_subkey("Environment")?;
    let path: String = env.get_value("Path")?;
    let mut paths = path.split(';').collect::<Vec<_>>();
    let original_len = paths.len();

    paths.retain(|p| p != &directory_path);

    let was_removed = original_len != paths.len();
    if was_removed {
      env.set_value("Path", &paths.join(";"))?;
    }
    Ok(())
  }
}

impl<S: NativeServices> crate::environment::ApplicationEnvironment for NativeEnvironment<S> {
  fn cli_version(&self) -> String {
    self.version.clone()
  }
  fn is_ci(&self) -> bool {
    match std::env::var_os("CI") {
      Some(value) => {
        let value = value.to_string_lossy();
        matches!(value.as_ref(), "true" | "1")
      }
      None => false,
    }
  }
}

fn canonicalize_path(path: impl AsRef<Path>) -> io::Result<CanonicalizedPathBuf> {
  // use this to avoid //?//C:/etc... like paths on windows (UNC)
  match dunce::canonicalize(path.as_ref()) {
    Ok(result) => Ok(CanonicalizedPathBuf::new(result)),
    Err(err) => Err(io::Error::new(
      err.kind(),
      format!("Error canonicalizing path '{}': {:#}", path.as_ref().display(), err),
    )),
  }
}

const CACHE_DIR_ENV_VAR_NAME: &str = "DPRINT_CACHE_DIR";

static CACHE_DIR: Lazy<io::Result<CanonicalizedPathBuf>> = Lazy::new(|| {
  #[allow(clippy::disallowed_methods)]
  let cache_dir = get_cache_dir_internal(|var_name| std::env::var(var_name).ok())?;
  #[allow(clippy::disallowed_methods)]
  std::fs::create_dir_all(&cache_dir)?;
  canonicalize_path(cache_dir)
});

fn get_cache_dir_internal(get_env_var: impl Fn(&str) -> Option<String>) -> io::Result<PathBuf> {
  if let Some(dir_path) = get_env_var(CACHE_DIR_ENV_VAR_NAME)
    && !dir_path.trim().is_empty()
  {
    let dir_path = PathBuf::from(dir_path);
    // seems dangerous to allow a relative path as this directory may be deleted
    return if !dir_path.is_absolute() {
      Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("The {} environment variable must specify an absolute path.", CACHE_DIR_ENV_VAR_NAME),
      ))
    } else {
      Ok(dir_path)
    };
  }

  match dirs::cache_dir() {
    Some(dir) => Ok(dir.join("dprint").join("cache")),
    None => Err(io::Error::new(io::ErrorKind::NotFound, "Expected to find cache directory.")),
  }
}

#[cfg(test)]
mod test {
  use super::*;

  #[test]
  fn should_get_cache_dir_based_on_env_var() {
    let default_dir = dirs::cache_dir().unwrap().join("dprint").join("cache");
    let value = if cfg!(target_os = "windows") {
      "C:/.dprint-cache"
    } else {
      "/home/david/.dprint-cache"
    };
    assert_eq!(get_cache_dir_internal(|_| Some(value.to_string())).unwrap().to_string_lossy(), value);
    assert_eq!(get_cache_dir_internal(|_| Some("".to_string())).unwrap(), default_dir);
    assert_eq!(get_cache_dir_internal(|_| Some("  ".to_string())).unwrap(), default_dir);
    assert_eq!(get_cache_dir_internal(|_| None).unwrap(), default_dir);
  }

  #[test]
  fn should_error_when_cache_dir_env_var_relative() {
    let result = get_cache_dir_internal(|_| Some("./dir".to_string())).err();
    assert_eq!(
      result.unwrap().to_string(),
      "The DPRINT_CACHE_DIR environment variable must specify an absolute path."
    );
  }
}

impl<S: NativeServices> OutputEnvironment for NativeEnvironment<S> {
  fn __log__(&self, text: &str) {
    self.services.__log__(text)
  }
  fn __log_stderr__(&self, text: &str) {
    self.services.__log_stderr__(text)
  }
  fn log_stderr_with_context(&self, text: &str, context_name: &str) {
    self.services.log_stderr_with_context(text, context_name)
  }
  fn log_machine_readable(&self, bytes: &[u8]) {
    self.services.log_machine_readable(bytes)
  }
  fn log_action_with_progress<TResult: Send + Sync, TCreate: FnOnce(Box<dyn Fn(usize)>) -> TResult + Send + Sync>(
    &self,
    message: &str,
    action: TCreate,
    total_size: usize,
  ) -> TResult {
    self.services.log_action_with_progress(message, action, total_size)
  }
  fn log_level(&self) -> LogLevel {
    self.services.log_level()
  }
  fn progress_bars(&self) -> Option<&Arc<dyn ProgressReporter>> {
    self.services.progress_bars()
  }
}

impl<S: NativeServices> InteractionEnvironment for NativeEnvironment<S> {
  fn get_selection(&self, prompt_message: &str, item_indent_width: u16, items: &[String]) -> Result<usize> {
    self.services.get_selection(prompt_message, item_indent_width, items)
  }
  fn get_multi_selection(&self, prompt_message: &str, item_indent_width: u16, items: Vec<MultiSelectItem>) -> Result<Vec<usize>> {
    self.services.get_multi_selection(prompt_message, item_indent_width, items)
  }

  fn stdout(&self) -> Box<dyn Write + Send> {
    self.services.stdout()
  }
  fn stdin(&self) -> Box<dyn Read + Send> {
    self.services.stdin()
  }
}

#[async_trait(?Send)]
impl<S: NativeServices> UrlDownloader for NativeEnvironment<S> {
  async fn download_file_no_redirects(&self, url: &Url, auth: Option<&str>, max_len: Option<usize>) -> Result<Option<DownloadedFile>> {
    self.services.download_file_no_redirects(url, auth, max_len).await
  }
}

/// Frontend services are independent of native filesystem and runtime behavior.
pub trait NativeServices: OutputEnvironment + InteractionEnvironment + UrlDownloader {
  fn compile_wasm<T: PluginEnvironment + ConcurrencyEnvironment + ProcessEnvironment>(
    &self,
    _environment: &T,
    _plugin_display: &str,
    _bytes: &[u8],
    _control: &crate::compiler::WasmCompileControl,
  ) -> Result<CompilationResult> {
    bail!("This environment has no Wasm compiler")
  }
  fn wasm_cache_key(&self, environment: &impl SystemEnvironment) -> String {
    environment.cpu_arch()
  }
}
impl<S: NativeServices> CompilerEnvironment for NativeEnvironment<S> {
  fn compile_wasm(&self, plugin_display: &str, bytes: &[u8], control: &crate::compiler::WasmCompileControl) -> Result<CompilationResult> {
    self.services.compile_wasm(self, plugin_display, bytes, control)
  }
  fn wasm_cache_key(&self) -> String {
    self.services.wasm_cache_key(self)
  }
}

/// Silent, noninteractive defaults for local embedding. Downloads and compiler
/// operations return an error until the caller supplies those services.
#[derive(Debug, Clone, Default)]
pub struct HeadlessServices;
impl NativeServices for HeadlessServices {}
impl OutputEnvironment for HeadlessServices {
  fn __log__(&self, _text: &str) {}
  fn log_stderr_with_context(&self, _text: &str, _context: &str) {}
  fn log_machine_readable(&self, _bytes: &[u8]) {}
  fn log_level(&self) -> LogLevel {
    LogLevel::Silent
  }
  fn log_action_with_progress<R: Send + Sync, F: FnOnce(Box<dyn Fn(usize)>) -> R + Send + Sync>(&self, _message: &str, action: F, _total: usize) -> R {
    action(Box::new(|_| {}))
  }
}
impl InteractionEnvironment for HeadlessServices {
  fn get_selection(&self, _prompt: &str, _indent: u16, _items: &[String]) -> Result<usize> {
    bail!("This environment is noninteractive")
  }
  fn get_multi_selection(&self, _prompt: &str, _indent: u16, _items: Vec<MultiSelectItem>) -> Result<Vec<usize>> {
    bail!("This environment is noninteractive")
  }

  fn stdout(&self) -> Box<dyn io::Write + Send> {
    Box::new(io::sink())
  }
  fn stdin(&self) -> Box<dyn io::Read + Send> {
    Box::new(io::empty())
  }
}
#[async_trait(?Send)]
impl UrlDownloader for HeadlessServices {
  async fn download_file_no_redirects(&self, url: &Url, _auth: Option<&str>, _max_len: Option<usize>) -> Result<Option<DownloadedFile>> {
    bail!("No downloader configured for {}", url)
  }
}
impl NativeEnvironment<HeadlessServices> {
  pub fn run_test_with_real_env(run: impl FnOnce(Self) -> dprint_async_runtime::LocalBoxFuture<'static, ()>) {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
    runtime.block_on(run(Self::new(HeadlessServices, "test").unwrap()));
  }
}

impl ConsentEnvironment for HeadlessServices {
  fn confirm_with_strategy(&self, _strategy: &dyn ShowConfirmStrategy) -> Result<bool> {
    bail!("This environment is noninteractive")
  }
  fn is_terminal_interactive(&self) -> bool {
    false
  }
}

impl<S: NativeServices> ConsentEnvironment for NativeEnvironment<S> {
  fn confirm(&self, prompt_message: &str, default_value: bool) -> Result<bool> {
    self.services.confirm(prompt_message, default_value)
  }
  fn confirm_with_strategy(&self, strategy: &dyn ShowConfirmStrategy) -> Result<bool> {
    self.services.confirm_with_strategy(strategy)
  }
  fn is_terminal_interactive(&self) -> bool {
    self.services.is_terminal_interactive()
  }
}
