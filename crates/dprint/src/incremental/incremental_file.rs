use parking_lot::Mutex;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;
use std::time::SystemTime;

use crate::environment::CanonicalizedPathBuf;
use crate::environment::Environment;
use crate::utils::get_bytes_hash;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IncrementalFileData {
  plugins_hash: u64,
  file_hashes: HashSet<u64>,
  /// The size and modification time of files known to be formatted, keyed by
  /// a hash of their path, so a file that hasn't changed can be skipped
  /// without reading it.
  #[serde(default, skip_serializing_if = "HashMap::is_empty")]
  file_stats: HashMap<u64, FileStat>,
}

impl IncrementalFileData {
  pub fn new(plugins_hash: u64) -> IncrementalFileData {
    IncrementalFileData {
      plugins_hash,
      file_hashes: Default::default(),
      file_stats: Default::default(),
    }
  }
}

/// A file's size, modification time (nanoseconds since the epoch), and the
/// hash of the formatted text it had then.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
struct FileStat(u64, u64, u64);

/// The size and modification time of a file on disk.
#[derive(Clone, Copy, Debug)]
pub struct FileMetadata {
  pub len: u64,
  pub modified: SystemTime,
}

/// A file modified this recently before the run may be modified again within
/// the same timestamp tick (some file systems only store seconds, or even two
/// second steps), which would go unnoticed, so its metadata isn't trusted.
const MIN_UNCHANGED_AGE: Duration = Duration::from_secs(3);

fn file_stat(metadata: &FileMetadata, run_start: SystemTime, content_hash: u64) -> Option<FileStat> {
  let modified_ns = u64::try_from(metadata.modified.duration_since(SystemTime::UNIX_EPOCH).ok()?.as_nanos()).ok()?;
  let age = run_start.duration_since(metadata.modified).ok()?;
  (age >= MIN_UNCHANGED_AGE).then_some(FileStat(metadata.len, modified_ns, content_hash))
}

pub struct IncrementalFile<TEnvironment: Environment> {
  file_path: CanonicalizedPathBuf,
  run_start: SystemTime,
  /// The data read from the existing file, when it exists and was
  /// created with the same plugins.
  read_data: Option<IncrementalFileData>,
  write_data: Mutex<IncrementalFileData>,
  /// Whether the run only covers some of the files, in which case the hashes
  /// of the files not seen this run are kept when writing.
  is_partial_run: bool,
  environment: TEnvironment,
}

impl<TEnvironment: Environment> IncrementalFile<TEnvironment> {
  pub fn new(file_path: CanonicalizedPathBuf, plugins_hash: u64, is_partial_run: bool, environment: TEnvironment) -> Self {
    let read_data = read_incremental(&file_path, &environment).and_then(|read_data| {
      if read_data.plugins_hash == plugins_hash {
        Some(read_data)
      } else {
        log_debug!(environment, "Plugins changed. Creating new incremental file.");
        None
      }
    });
    IncrementalFile {
      file_path,
      run_start: environment.sys_time_now(),
      read_data,
      write_data: Mutex::new(IncrementalFileData::new(plugins_hash)),
      is_partial_run,
      environment,
    }
  }

  /// Whether any formatted files are known, which is not the case on a first
  /// run or after the plugins or their configuration changed.
  pub fn has_known_files(&self) -> bool {
    self.read_data.as_ref().is_some_and(|data| !data.file_hashes.is_empty())
  }

  /// If the file is known to be formatted because it's the same size and has
  /// the same modification time as when its text was last known formatted.
  /// This avoids reading the file.
  pub fn is_file_known_formatted_by_metadata(&self, file_path: &Path, metadata: &FileMetadata) -> bool {
    let Some(read_data) = &self.read_data else {
      return false;
    };
    let path_hash = get_bytes_hash(file_path.as_os_str().as_encoded_bytes());
    let Some(stat) = read_data.file_stats.get(&path_hash) else {
      return false;
    };
    let FileStat(len, modified_ns, content_hash) = *stat;
    let is_unchanged = file_stat(metadata, self.run_start, content_hash).is_some_and(|current| current.0 == len && current.1 == modified_ns);
    if is_unchanged && read_data.file_hashes.contains(&content_hash) {
      let mut write_data = self.write_data.lock();
      write_data.file_hashes.insert(content_hash);
      write_data.file_stats.insert(path_hash, *stat);
      true
    } else {
      false
    }
  }

  /// If the file text is known to be formatted. `metadata` is the file's
  /// metadata from before it was read, which is remembered so the file can be
  /// skipped without reading it next time.
  pub fn is_file_known_formatted(&self, file_path: &Path, file_text: &[u8], metadata: Option<&FileMetadata>) -> bool {
    let hash = get_bytes_hash(file_text);
    if self.read_data.as_ref().is_some_and(|data| data.file_hashes.contains(&hash)) {
      // the file is the same, so save it in the write data
      let mut write_data = self.write_data.lock();
      write_data.file_hashes.insert(hash);
      // the size check guards against the file changing between getting its
      // metadata and reading it
      if let Some(metadata) = metadata.filter(|metadata| metadata.len == file_text.len() as u64)
        && let Some(stat) = file_stat(metadata, self.run_start, hash)
      {
        write_data.file_stats.insert(get_bytes_hash(file_path.as_os_str().as_encoded_bytes()), stat);
      }
      true
    } else {
      false
    }
  }

  pub fn update_file(&self, file_text: &[u8]) {
    let hash = get_bytes_hash(file_text);
    self.add_to_write_data(hash)
  }

  fn add_to_write_data(&self, hash: u64) {
    let mut write_data = self.write_data.lock();
    write_data.file_hashes.insert(hash);
  }

  pub fn write(&self) {
    let write_data = self.write_data.lock();
    if let Some(read_data) = &self.read_data {
      // don't rewrite the file when nothing new was learned, which is the case
      // when every file seen this run was already known to be formatted; this
      // keeps the file's bytes stable so a CI cache of the cache directory can
      // detect it hasn't changed
      let learned_nothing = write_data.file_hashes.is_subset(&read_data.file_hashes)
        && write_data
          .file_stats
          .iter()
          .all(|(path_hash, stat)| read_data.file_stats.get(path_hash) == Some(stat));
      if learned_nothing {
        log_debug!(self.environment, "Incremental file unchanged. Skipping write.");
        return;
      }
      // a partial run keeps the hashes of the files it didn't see, while a full
      // run writes only what it saw so the hashes of changed and deleted files
      // get pruned
      if self.is_partial_run {
        let mut file_stats = read_data.file_stats.clone();
        file_stats.extend(write_data.file_stats.iter().map(|(path_hash, stat)| (*path_hash, *stat)));
        let merged_data = IncrementalFileData {
          plugins_hash: write_data.plugins_hash,
          file_hashes: write_data.file_hashes.union(&read_data.file_hashes).copied().collect(),
          file_stats,
        };
        write_incremental(&self.file_path, &merged_data, &self.environment);
        return;
      }
    }
    write_incremental(&self.file_path, &write_data, &self.environment);
  }
}

fn read_incremental(file_path: impl AsRef<Path>, environment: &impl Environment) -> Option<IncrementalFileData> {
  let file_text = match environment.read_file(&file_path) {
    Ok(file_text) => file_text,
    Err(err) => {
      if environment.path_exists(&file_path) {
        log_warn!(environment, "Error reading incremental file {}: {}", file_path.as_ref().display(), err);
      }
      return None;
    }
  };

  match serde_json::from_str(&file_text) {
    Ok(file_data) => Some(file_data),
    Err(err) => {
      log_warn!(environment, "Error deserializing incremental file {}: {}", file_path.as_ref().display(), err);
      None
    }
  }
}

fn write_incremental(file_path: impl AsRef<Path>, file_data: &IncrementalFileData, environment: &impl Environment) {
  let json_text = match serde_json::to_string(&file_data) {
    Ok(json_text) => json_text,
    Err(err) => {
      log_warn!(environment, "Error serializing incremental file {}: {}", file_path.as_ref().display(), err);
      return;
    }
  };
  if let Err(err) = environment.atomic_write_file_bytes(&file_path, json_text.as_bytes()) {
    log_warn!(environment, "Error saving incremental file {}: {}", file_path.as_ref().display(), err);
  }
}
