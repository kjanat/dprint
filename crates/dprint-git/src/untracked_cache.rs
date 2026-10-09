//! Git's untracked cache index extension ([`read_untracked_extension`] and
//! [`untracked_cache_invalidate_path`] in [`dir.c`]).
//!
//! [`read_untracked_extension`]: https://github.com/git/git/blob/6de20f6092dcf9bdb1c8efe03db4b70c82b423dd/dir.c#L3889-L3980
//! [`untracked_cache_invalidate_path`]: https://github.com/git/git/blob/6de20f6092dcf9bdb1c8efe03db4b70c82b423dd/dir.c#L4038-L4047
//! [`dir.c`]: https://github.com/git/git/blob/6de20f6092dcf9bdb1c8efe03db4b70c82b423dd/dir.c

use anyhow::Result;
use anyhow::bail;

use crate::bytes::ByteReader;
use crate::ewah::read_ewah;

pub const DIR_SHOW_OTHER_DIRECTORIES: u32 = 1 << 1;
pub const DIR_HIDE_EMPTY_DIRECTORIES: u32 = 1 << 2;

/// Git's `struct stat_data` on disk.
const STAT_DATA_LEN: usize = 36;

#[derive(Debug, Clone)]
pub struct UntrackedCache {
  pub ident: Vec<u8>,
  pub info_exclude_oid: Vec<u8>,
  pub excludes_file_oid: Vec<u8>,
  pub dir_flags: u32,
  pub exclude_per_dir: Vec<u8>,
  /// `dirs[0]` is the root directory, if there is one.
  pub dirs: Vec<UntrackedDir>,
}

#[derive(Debug, Clone, Default)]
pub struct UntrackedDir {
  pub name: Vec<u8>,
  /// Untracked files, and untracked directories with a trailing `/`.
  pub untracked: Vec<Vec<u8>>,
  /// Sorted by name.
  pub subdirs: Vec<usize>,
  pub valid: bool,
  pub check_only: bool,
  pub has_exclude_file: bool,
}

impl UntrackedCache {
  pub fn parse(data: &[u8], hash_len: usize) -> Result<Self> {
    let Some((0, data)) = data.split_last() else {
      bail!("the extension doesn't end with a NUL");
    };
    let mut reader = ByteReader::new(data);
    let ident_len = usize::try_from(reader.varint()?)?;
    let ident = reader.take(ident_len)?;
    let ident = ident.split(|byte| *byte == 0).next().unwrap_or_default().to_vec();
    reader.take(STAT_DATA_LEN * 2)?;
    let dir_flags = reader.u32()?;
    let info_exclude_oid = reader.take(hash_len)?.to_vec();
    let excludes_file_oid = reader.take(hash_len)?.to_vec();
    let exclude_per_dir = reader.c_str()?.to_vec();
    let mut cache = UntrackedCache {
      ident,
      info_exclude_oid,
      excludes_file_oid,
      dir_flags,
      exclude_per_dir,
      dirs: Vec::new(),
    };
    if reader.is_empty() {
      return Ok(cache);
    }
    let dir_count = usize::try_from(reader.varint()?)?;
    if dir_count == 0 {
      if !reader.is_empty() {
        bail!("trailing data after an empty directory list");
      }
      return Ok(cache);
    }
    cache.dirs = read_dirs(&mut reader)?;
    if cache.dirs.len() != dir_count {
      bail!("expected {} directories but read {}", dir_count, cache.dirs.len());
    }
    let valid = read_ewah(&mut reader)?;
    let check_only = read_ewah(&mut reader)?;
    let has_exclude_file = read_ewah(&mut reader)?;
    for index in check_only {
      cache.dir_mut(index)?.check_only = true;
    }
    for index in valid {
      reader.take(STAT_DATA_LEN)?;
      cache.dir_mut(index)?.valid = true;
    }
    for index in has_exclude_file {
      reader.take(hash_len)?;
      cache.dir_mut(index)?.has_exclude_file = true;
    }
    if !reader.is_empty() {
      bail!("trailing data");
    }
    Ok(cache)
  }

  fn dir_mut(&mut self, index: usize) -> Result<&mut UntrackedDir> {
    let len = self.dirs.len();
    match self.dirs.get_mut(index) {
      Some(dir) => Ok(dir),
      None => bail!("a bitmap refers to directory {} of {}", index, len),
    }
  }

  pub fn child(&self, dir: usize, name: &[u8]) -> Option<usize> {
    let subdirs = &self.dirs[dir].subdirs;
    subdirs
      .binary_search_by(|subdir| self.dirs[*subdir].name.as_slice().cmp(name))
      .ok()
      .map(|position| subdirs[position])
  }

  /// Finds the directory at a `/` separated path relative to the root.
  pub fn find(&self, path: &[u8]) -> Option<usize> {
    let mut dir = if self.dirs.is_empty() { None } else { Some(0) }?;
    for name in path.split(|byte| *byte == b'/').filter(|name| !name.is_empty()) {
      dir = self.child(dir, name)?;
    }
    Some(dir)
  }

  fn child_or_insert(&mut self, dir: usize, name: &[u8]) -> usize {
    let position = match self.dirs[dir].subdirs.binary_search_by(|subdir| self.dirs[*subdir].name.as_slice().cmp(name)) {
      Ok(position) => return self.dirs[dir].subdirs[position],
      Err(position) => position,
    };
    let child = self.dirs.len();
    self.dirs.push(UntrackedDir {
      name: name.to_vec(),
      ..Default::default()
    });
    self.dirs[dir].subdirs.insert(position, child);
    child
  }

  /// Git's `untracked_cache_invalidate_trimmed_path`: invalidates the
  /// directory holding `path`, and with `DIR_SHOW_OTHER_DIRECTORIES` every
  /// directory above it too.
  pub fn invalidate_path(&mut self, path: &[u8]) {
    let path = path.strip_suffix(b"/").unwrap_or(path);
    if self.dirs.is_empty() || path.is_empty() {
      return;
    }
    let mut chain = vec![0];
    let mut components = path.split(|byte| *byte == b'/').peekable();
    while let Some(name) = components.next() {
      if components.peek().is_none() {
        break;
      }
      let parent = *chain.last().unwrap_or(&0);
      chain.push(self.child_or_insert(parent, name));
    }
    if self.dir_flags & DIR_SHOW_OTHER_DIRECTORIES != 0 {
      for dir in chain {
        self.invalidate_dir(dir);
      }
    } else if let Some(dir) = chain.last() {
      self.invalidate_dir(*dir);
    }
  }

  /// Git's `invalidate_gitignore`: invalidates a directory and everything
  /// below it.
  pub fn invalidate_tree(&mut self, dir: usize) {
    let mut pending = vec![dir];
    while let Some(dir) = pending.pop() {
      self.invalidate_dir(dir);
      pending.extend(self.dirs[dir].subdirs.iter().copied());
    }
  }

  fn invalidate_dir(&mut self, dir: usize) {
    let dir = &mut self.dirs[dir];
    dir.valid = false;
    dir.untracked.clear();
  }
}

/// Git's `write_one_dir` writes the directories in pre-order.
fn read_dirs(reader: &mut ByteReader) -> Result<Vec<UntrackedDir>> {
  let mut dirs = Vec::new();
  // directories that still have subdirectories to read, with how many
  let mut open: Vec<(usize, usize)> = Vec::new();
  loop {
    let untracked_count = usize::try_from(reader.varint()?)?;
    let subdir_count = usize::try_from(reader.varint()?)?;
    let name = reader.c_str()?.to_vec();
    let mut untracked = Vec::with_capacity(untracked_count.min(1 << 16));
    for _ in 0..untracked_count {
      untracked.push(reader.c_str()?.to_vec());
    }
    let index = dirs.len();
    dirs.push(UntrackedDir {
      name,
      untracked,
      ..Default::default()
    });
    if let Some((parent, remaining)) = open.last_mut() {
      dirs[*parent].subdirs.push(index);
      *remaining -= 1;
    }
    if subdir_count > 0 {
      open.push((index, subdir_count));
    }
    while open.last().is_some_and(|(_, remaining)| *remaining == 0) {
      open.pop();
    }
    if open.is_empty() {
      return Ok(dirs);
    }
  }
}

#[cfg(any(test, feature = "test-util"))]
pub(crate) mod test_writer {
  //! Writes the extension like git's `write_untracked_extension`.

  use super::STAT_DATA_LEN;
  use crate::bytes::encode_varint;
  use crate::ewah::write_ewah;

  pub struct TestDir {
    pub name: &'static str,
    pub untracked: Vec<&'static str>,
    pub subdirs: Vec<TestDir>,
    pub valid: bool,
    pub check_only: bool,
    pub has_exclude_file: bool,
  }

  impl TestDir {
    pub fn valid(name: &'static str, untracked: &[&'static str], subdirs: Vec<TestDir>) -> Self {
      Self {
        name,
        untracked: untracked.to_vec(),
        subdirs,
        valid: true,
        check_only: false,
        has_exclude_file: false,
      }
    }
  }

  pub struct TestUntrackedCache {
    pub ident: String,
    pub dir_flags: u32,
    pub info_exclude_oid: Vec<u8>,
    pub excludes_file_oid: Vec<u8>,
    pub root: Option<TestDir>,
  }

  pub fn write_untracked_cache(cache: &TestUntrackedCache) -> Vec<u8> {
    let mut ident = cache.ident.as_bytes().to_vec();
    ident.push(0);
    let mut bytes = encode_varint(ident.len() as u64);
    bytes.extend(ident);
    bytes.extend(vec![0; STAT_DATA_LEN * 2]);
    bytes.extend(cache.dir_flags.to_be_bytes());
    bytes.extend(&cache.info_exclude_oid);
    bytes.extend(&cache.excludes_file_oid);
    bytes.extend(b".gitignore\0");
    let Some(root) = &cache.root else {
      bytes.extend(encode_varint(0));
      bytes.push(0);
      return bytes;
    };
    let hash_len = cache.info_exclude_oid.len();
    let mut out = Vec::new();
    let mut valid = Vec::new();
    let mut check_only = Vec::new();
    let mut has_exclude_file = Vec::new();
    let mut count = 0;
    write_dir(root, &mut out, &mut count, &mut valid, &mut check_only, &mut has_exclude_file);
    bytes.extend(encode_varint(count as u64));
    bytes.extend(out);
    bytes.extend(write_ewah(&valid));
    bytes.extend(write_ewah(&check_only));
    bytes.extend(write_ewah(&has_exclude_file));
    bytes.extend(vec![0; STAT_DATA_LEN * valid.len()]);
    bytes.extend(vec![0xcd; hash_len * has_exclude_file.len()]);
    bytes.push(0);
    bytes
  }

  fn write_dir(dir: &TestDir, out: &mut Vec<u8>, count: &mut usize, valid: &mut Vec<usize>, check_only: &mut Vec<usize>, has_exclude_file: &mut Vec<usize>) {
    let index = *count;
    *count += 1;
    if dir.valid {
      valid.push(index);
    }
    if dir.check_only {
      check_only.push(index);
    }
    if dir.has_exclude_file {
      has_exclude_file.push(index);
    }
    out.extend(encode_varint(dir.untracked.len() as u64));
    out.extend(encode_varint(dir.subdirs.len() as u64));
    out.extend(dir.name.as_bytes());
    out.push(0);
    for name in &dir.untracked {
      out.extend(name.as_bytes());
      out.push(0);
    }
    for subdir in &dir.subdirs {
      write_dir(subdir, out, count, valid, check_only, has_exclude_file);
    }
  }
}

#[cfg(test)]
mod test {
  use super::test_writer::*;
  use super::*;

  fn sample(dir_flags: u32) -> UntrackedCache {
    let mut ignored = TestDir::valid("b", &[], vec![]);
    ignored.has_exclude_file = true;
    let mut untracked_dir = TestDir::valid("new", &[], vec![]);
    untracked_dir.check_only = true;
    let root = TestDir::valid(
      "",
      &["root.txt", "new/"],
      vec![
        TestDir::valid("a", &["a.txt"], vec![ignored, TestDir::valid("c", &["deep.txt"], vec![])]),
        untracked_dir,
      ],
    );
    let bytes = write_untracked_cache(&TestUntrackedCache {
      ident: "Location /repo, system Linux".to_string(),
      dir_flags,
      info_exclude_oid: vec![1; 20],
      excludes_file_oid: vec![0; 20],
      root: Some(root),
    });
    UntrackedCache::parse(&bytes, 20).unwrap()
  }

  fn names(cache: &UntrackedCache, dir: usize) -> Vec<String> {
    cache.dirs[dir].untracked.iter().map(|name| String::from_utf8(name.clone()).unwrap()).collect()
  }

  #[test]
  fn reads_the_directory_tree() {
    let cache = sample(DIR_SHOW_OTHER_DIRECTORIES | DIR_HIDE_EMPTY_DIRECTORIES);
    assert_eq!(cache.ident, b"Location /repo, system Linux");
    assert_eq!(cache.exclude_per_dir, b".gitignore");
    assert_eq!(cache.info_exclude_oid, vec![1; 20]);
    assert_eq!(cache.dirs.len(), 5);
    assert_eq!(names(&cache, 0), ["root.txt", "new/"]);
    let a = cache.find(b"a").unwrap();
    assert_eq!(names(&cache, a), ["a.txt"]);
    let b = cache.find(b"a/b").unwrap();
    assert!(cache.dirs[b].has_exclude_file);
    assert_eq!(names(&cache, cache.find(b"a/c").unwrap()), ["deep.txt"]);
    assert!(cache.dirs[cache.find(b"new").unwrap()].check_only);
    assert!(cache.dirs.iter().all(|dir| dir.valid));
    assert_eq!(cache.find(b"missing"), None);
    assert_eq!(cache.find(b""), Some(0));
  }

  #[test]
  fn reads_a_cache_without_directories() {
    let bytes = write_untracked_cache(&TestUntrackedCache {
      ident: "Location /repo, system Linux".to_string(),
      dir_flags: 0,
      info_exclude_oid: vec![0; 32],
      excludes_file_oid: vec![0; 32],
      root: None,
    });
    let cache = UntrackedCache::parse(&bytes, 32).unwrap();
    assert!(cache.dirs.is_empty());
    assert_eq!(cache.find(b""), None);
  }

  #[test]
  fn invalidates_the_parent_and_ancestors_when_showing_other_directories() {
    let mut cache = sample(DIR_SHOW_OTHER_DIRECTORIES | DIR_HIDE_EMPTY_DIRECTORIES);
    cache.invalidate_path(b"a/c/deep.txt");
    let valid = |cache: &UntrackedCache, path: &[u8]| cache.dirs[cache.find(path).unwrap()].valid;
    assert!(!valid(&cache, b""));
    assert!(!valid(&cache, b"a"));
    assert!(!valid(&cache, b"a/c"));
    assert!(valid(&cache, b"a/b"));
    assert!(cache.dirs[0].untracked.is_empty());
  }

  #[test]
  fn invalidates_only_the_parent_without_showing_other_directories() {
    let mut cache = sample(0);
    cache.invalidate_path(b"a/c/");
    let valid = |cache: &UntrackedCache, path: &[u8]| cache.dirs[cache.find(path).unwrap()].valid;
    assert!(valid(&cache, b""));
    assert!(!valid(&cache, b"a"));
    assert!(valid(&cache, b"a/c"));
  }

  #[test]
  fn creates_unknown_directories() {
    let mut cache = sample(0);
    cache.invalidate_path(b"a/z/y/file.txt");
    let created = cache.find(b"a/z/y").unwrap();
    assert!(!cache.dirs[created].valid);
    let a = cache.find(b"a").unwrap();
    let subdir_names = cache.dirs[a].subdirs.iter().map(|dir| cache.dirs[*dir].name.clone()).collect::<Vec<_>>();
    assert_eq!(subdir_names, [b"b".to_vec(), b"c".to_vec(), b"z".to_vec()]);
  }

  #[test]
  fn invalidates_a_whole_tree() {
    let mut cache = sample(0);
    cache.invalidate_tree(cache.find(b"a").unwrap());
    assert!(cache.dirs[0].valid);
    assert!(cache.dirs[cache.find(b"new").unwrap()].valid);
    for path in [b"a".as_slice(), b"a/b", b"a/c"] {
      assert!(!cache.dirs[cache.find(path).unwrap()].valid);
    }
  }

  #[test]
  fn rejects_malformed_extensions() {
    assert!(UntrackedCache::parse(b"", 20).is_err());
    assert!(UntrackedCache::parse(b"abc", 20).is_err());
    let mut bytes = write_untracked_cache(&TestUntrackedCache {
      ident: "x".to_string(),
      dir_flags: 0,
      info_exclude_oid: vec![0; 20],
      excludes_file_oid: vec![0; 20],
      root: Some(TestDir::valid("", &["a"], vec![])),
    });
    let last = bytes.len() - 1;
    bytes.insert(last, 7);
    assert!(UntrackedCache::parse(&bytes, 20).is_err());
  }
}
