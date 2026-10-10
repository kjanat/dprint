use anyhow::Result;
use anyhow::bail;

use crate::ewah::read_ewah;
use crate::reader::Reader;

pub const DIR_SHOW_OTHER_DIRECTORIES: u32 = 1 << 1;
pub const DIR_HIDE_EMPTY_DIRECTORIES: u32 = 1 << 2;

const STAT_DATA_LEN: usize = 36;

#[derive(Debug, Clone)]
pub struct UntrackedCache {
  pub ident: Vec<u8>,
  pub info_exclude_oid: Vec<u8>,
  pub excludes_file_oid: Vec<u8>,
  pub dir_flags: u32,
  pub exclude_per_dir: Vec<u8>,
  pub dirs: Vec<UntrackedDir>,
}

#[derive(Debug, Clone, Default)]
pub struct UntrackedDir {
  pub name: Vec<u8>,
  pub untracked: Vec<Vec<u8>>,
  pub subdirs: Vec<usize>,
  pub valid: bool,
  pub check_only: bool,
  pub has_exclude_file: bool,
}

impl UntrackedCache {
  pub fn parse(data: &[u8], hash_len: usize) -> Result<Self> {
    let mut reader = Reader::new(data);
    let ident_len = reader.varint()?;
    let ident = match reader.bytes(ident_len)?.split_last() {
      None => Vec::new(),
      Some((0, ident)) => ident.to_vec(),
      Some(_) => bail!("the untracked cache's ident isn't NUL-terminated"),
    };
    reader.skip(2 * STAT_DATA_LEN)?;
    let dir_flags = reader.u32()?;
    let info_exclude_oid = reader.bytes(hash_len)?.to_vec();
    let excludes_file_oid = reader.bytes(hash_len)?.to_vec();
    let exclude_per_dir = reader.nul_terminated()?.to_vec();
    let dir_count = reader.varint()?;
    let mut cache = UntrackedCache {
      ident,
      info_exclude_oid,
      excludes_file_oid,
      dir_flags,
      exclude_per_dir,
      dirs: Vec::new(),
    };
    if dir_count == 0 {
      // Git ends the extension right after a zero count.
      if !matches!(reader.remaining(), [] | [0]) {
        bail!("the untracked cache has data after zero directories");
      }
      return Ok(cache);
    }

    cache.dirs = read_dir_blocks(&mut reader, dir_count)?;
    let valid = read_ewah(&mut reader, dir_count)?;
    let check_only = read_ewah(&mut reader, dir_count)?;
    let has_exclude_file = read_ewah(&mut reader, dir_count)?;
    for position in &valid {
      cache.dirs[*position].valid = true;
    }
    for position in check_only {
      cache.dirs[position].check_only = true;
    }
    for position in &has_exclude_file {
      cache.dirs[*position].has_exclude_file = true;
    }
    // Git writes one stat data entry per valid directory.
    reader.skip(valid.len().saturating_mul(STAT_DATA_LEN))?;
    reader.skip(has_exclude_file.len().saturating_mul(hash_len))?;
    if reader.remaining() != [0] {
      bail!("the untracked cache doesn't end with a NUL after its directories");
    }
    Ok(cache)
  }

  pub fn child(&self, dir: usize, name: &[u8]) -> Option<usize> {
    let subdirs = &self.dirs.get(dir)?.subdirs;
    let found = subdirs.binary_search_by(|subdir| self.name_of(*subdir).cmp(name)).ok()?;
    Some(subdirs[found])
  }

  pub fn find(&self, path: &[u8]) -> Option<usize> {
    if self.dirs.is_empty() {
      return None;
    }
    if path.is_empty() {
      return Some(0);
    }
    path.split(|byte| *byte == b'/').try_fold(0, |dir, name| self.child(dir, name))
  }

  pub fn invalidate_path(&mut self, path: &[u8]) {
    let path = path.strip_suffix(b"/").unwrap_or(path);
    if self.dirs.is_empty() {
      return;
    }
    if path.is_empty() {
      self.invalidate_tree(0);
      return;
    }
    let (parent, name) = match path.iter().rposition(|byte| *byte == b'/') {
      Some(slash) => (&path[..slash], &path[slash + 1..]),
      None => (&b""[..], path),
    };
    let mut dir = 0;
    self.dirs[0].valid = false;
    if !parent.is_empty() {
      for component in parent.split(|byte| *byte == b'/') {
        let Some(child) = self.child(dir, component) else {
          return;
        };
        dir = child;
        self.dirs[dir].valid = false;
      }
    }
    if let Some(named) = self.child(dir, name) {
      self.invalidate_tree(named);
    }
  }

  pub fn invalidate_tree(&mut self, dir: usize) {
    let mut pending = vec![dir];
    while let Some(dir) = pending.pop() {
      if let Some(dir) = self.dirs.get_mut(dir) {
        dir.valid = false;
        pending.extend_from_slice(&dir.subdirs);
      }
    }
  }

  fn name_of(&self, dir: usize) -> &[u8] {
    self.dirs.get(dir).map_or(&[], |dir| dir.name.as_slice())
  }
}

fn read_dir_blocks(reader: &mut Reader, dir_count: usize) -> Result<Vec<UntrackedDir>> {
  let mut dirs: Vec<UntrackedDir> = Vec::with_capacity(dir_count.min(reader.remaining().len() / 3));
  let mut open: Vec<(usize, usize)> = Vec::new();
  loop {
    if dirs.len() == dir_count {
      bail!("the untracked cache has more directory blocks than its count of {dir_count}");
    }
    let untracked_count = reader.varint()?;
    let subdir_count = reader.varint()?;
    let name = reader.nul_terminated()?.to_vec();
    let mut untracked = Vec::with_capacity(untracked_count.min(reader.remaining().len()));
    for _ in 0..untracked_count {
      untracked.push(reader.nul_terminated()?.to_vec());
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
      break;
    }
  }
  if dirs.len() != dir_count {
    bail!("the untracked cache has {} directory blocks, not {dir_count}", dirs.len());
  }
  for dir in 0..dirs.len() {
    let mut subdirs = std::mem::take(&mut dirs[dir].subdirs);
    subdirs.sort_by(|a, b| dirs[*a].name.cmp(&dirs[*b].name));
    if subdirs.windows(2).any(|pair| dirs[pair[0]].name == dirs[pair[1]].name) {
      bail!("the untracked cache lists a directory twice");
    }
    dirs[dir].subdirs = subdirs;
  }
  Ok(dirs)
}

#[cfg(any(test, feature = "test-util"))]
pub(crate) mod test_writer {
  use crate::ewah::write_ewah;
  use crate::reader::write_varint;

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
      TestDir {
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

  fn preorder<'a>(dir: &'a TestDir, out: &mut Vec<&'a TestDir>) {
    out.push(dir);
    for subdir in &dir.subdirs {
      preorder(subdir, out);
    }
  }

  pub fn write_untracked_cache(cache: &TestUntrackedCache) -> Vec<u8> {
    let mut out = Vec::new();
    write_varint(&mut out, cache.ident.len() + 1);
    out.extend_from_slice(cache.ident.as_bytes());
    out.push(0);
    out.extend_from_slice(&[0; 2 * super::STAT_DATA_LEN]);
    out.extend_from_slice(&cache.dir_flags.to_be_bytes());
    out.extend_from_slice(&cache.info_exclude_oid);
    out.extend_from_slice(&cache.excludes_file_oid);
    out.extend_from_slice(b".gitignore\0");
    let mut dirs = Vec::new();
    if let Some(root) = &cache.root {
      preorder(root, &mut dirs);
    }
    write_varint(&mut out, dirs.len());
    if dirs.is_empty() {
      return out;
    }
    for dir in &dirs {
      write_varint(&mut out, dir.untracked.len());
      write_varint(&mut out, dir.subdirs.len());
      out.extend_from_slice(dir.name.as_bytes());
      out.push(0);
      for name in &dir.untracked {
        out.extend_from_slice(name.as_bytes());
        out.push(0);
      }
    }
    let positions = |flag: fn(&TestDir) -> bool| -> Vec<usize> { dirs.iter().enumerate().filter(|(_, dir)| flag(dir)).map(|(index, _)| index).collect() };
    let valid = positions(|dir| dir.valid);
    let has_exclude_file = positions(|dir| dir.has_exclude_file);
    write_ewah(&mut out, &valid);
    write_ewah(&mut out, &positions(|dir| dir.check_only));
    write_ewah(&mut out, &has_exclude_file);
    out.resize(out.len() + valid.len() * super::STAT_DATA_LEN, 0);
    out.resize(out.len() + has_exclude_file.len() * cache.info_exclude_oid.len(), 0);
    out.push(0);
    out
  }
}

#[cfg(test)]
mod test {
  use super::test_writer::*;
  use super::*;
  use crate::index_file::parse_index;
  use crate::test_git::TempRepo;

  fn ident() -> String {
    "Location /repo, system Linux".to_string()
  }

  fn cache(root: Option<TestDir>) -> UntrackedCache {
    let data = write_untracked_cache(&TestUntrackedCache {
      ident: ident(),
      dir_flags: DIR_SHOW_OTHER_DIRECTORIES | DIR_HIDE_EMPTY_DIRECTORIES,
      info_exclude_oid: vec![1; 20],
      excludes_file_oid: vec![2; 20],
      root,
    });
    UntrackedCache::parse(&data, 20).unwrap()
  }

  fn tree() -> UntrackedCache {
    let mut d = TestDir::valid("d", &["x"], vec![]);
    d.check_only = true;
    d.has_exclude_file = true;
    let mut e = TestDir::valid("e", &[], vec![]);
    e.valid = false;
    let a = TestDir::valid("a", &["f.ts"], vec![TestDir::valid("b", &[], vec![TestDir::valid("c", &["g/"], vec![])])]);
    let mut root = TestDir::valid("", &["new.ts", "newdir/"], vec![e, a, d]);
    root.has_exclude_file = true;
    cache(Some(root))
  }

  fn path_of(cache: &UntrackedCache, dir: usize) -> String {
    fn walk(cache: &UntrackedCache, current: usize, target: usize, prefix: &str) -> Option<String> {
      if current == target {
        return Some(prefix.to_string());
      }
      cache.dirs[current].subdirs.iter().find_map(|child| {
        let name = String::from_utf8_lossy(&cache.dirs[*child].name);
        let path = if prefix.is_empty() { name.into_owned() } else { format!("{prefix}/{name}") };
        walk(cache, *child, target, &path)
      })
    }
    walk(cache, 0, dir, "").unwrap()
  }

  fn valid_paths(cache: &UntrackedCache) -> Vec<String> {
    let mut paths: Vec<String> = (0..cache.dirs.len())
      .filter(|dir| cache.dirs[*dir].valid)
      .map(|dir| path_of(cache, dir))
      .collect();
    paths.sort();
    paths
  }

  #[test]
  fn round_trips_the_test_writer() {
    let cache = tree();
    assert_eq!(cache.ident, ident().as_bytes());
    assert_eq!(cache.dir_flags, 6);
    assert_eq!(cache.info_exclude_oid, vec![1; 20]);
    assert_eq!(cache.excludes_file_oid, vec![2; 20]);
    assert_eq!(cache.exclude_per_dir, b".gitignore");
    assert_eq!(cache.dirs.len(), 6);
    let root = &cache.dirs[0];
    assert!(root.name.is_empty());
    assert_eq!(root.untracked, vec![b"new.ts".to_vec(), b"newdir/".to_vec()]);
    let names: Vec<&[u8]> = root.subdirs.iter().map(|dir| cache.dirs[*dir].name.as_slice()).collect();
    assert_eq!(names, vec![&b"a"[..], b"d", b"e"]);
    assert!(root.valid && !root.check_only && root.has_exclude_file);
    let d = &cache.dirs[cache.find(b"d").unwrap()];
    assert!(d.valid && d.check_only && d.has_exclude_file);
    assert_eq!(d.untracked, vec![b"x".to_vec()]);
    assert!(!cache.dirs[cache.find(b"e").unwrap()].valid);
    let c = &cache.dirs[cache.find(b"a/b/c").unwrap()];
    assert_eq!(c.untracked, vec![b"g/".to_vec()]);
    assert!(c.subdirs.is_empty());
  }

  #[test]
  fn reads_a_cache_without_directories() {
    let cache = cache(None);
    assert!(cache.dirs.is_empty());
    assert_eq!(cache.find(b""), None);
    let mut cache = cache;
    cache.invalidate_path(b"a");
    cache.invalidate_tree(0);
  }

  #[test]
  fn finds_directories() {
    let cache = tree();
    assert_eq!(cache.find(b""), Some(0));
    for path in ["a", "a/b", "a/b/c", "d", "e"] {
      let dir = cache.find(path.as_bytes()).unwrap();
      assert_eq!(path_of(&cache, dir), path);
    }
    for path in ["b", "a/c", "a/b/c/d", "a/", "/a", "a//b", "new.ts"] {
      assert_eq!(cache.find(path.as_bytes()), None, "{path}");
    }
    let a = cache.find(b"a").unwrap();
    assert_eq!(cache.child(a, b"b"), cache.find(b"a/b"));
    assert_eq!(cache.child(a, b"x"), None);
    assert_eq!(cache.child(100, b"b"), None);
  }

  #[test]
  fn invalidates_a_changed_file_and_its_ancestors() {
    let mut cache = tree();
    cache.invalidate_path(b"a/b/new.ts");
    assert_eq!(valid_paths(&cache), vec!["a/b/c", "d"]);
    let mut cache = tree();
    cache.invalidate_path(b"top.ts");
    assert_eq!(valid_paths(&cache), vec!["a", "a/b", "a/b/c", "d"]);
  }

  #[test]
  fn invalidates_existing_ancestors_of_uncached_paths() {
    let mut cache = tree();
    cache.invalidate_path(b"a/newdir/deeper/x.ts");
    assert_eq!(valid_paths(&cache), vec!["a/b", "a/b/c", "d"]);
    let len = cache.dirs.len();
    cache.invalidate_path(b"zzz/yyy/");
    assert_eq!(cache.dirs.len(), len);
  }

  #[test]
  fn invalidates_a_changed_directory_and_everything_below() {
    let mut cache = tree();
    cache.invalidate_path(b"a/");
    assert_eq!(valid_paths(&cache), vec!["d"]);
    let mut cache = tree();
    cache.invalidate_path(b"a/b/");
    assert_eq!(valid_paths(&cache), vec!["d"]);
    let mut cache = tree();
    cache.invalidate_path(b"a/b/c/");
    assert_eq!(valid_paths(&cache), vec!["d"]);
    let mut cache = tree();
    cache.invalidate_path(b"d");
    assert_eq!(valid_paths(&cache), vec!["a", "a/b", "a/b/c"]);
    let mut cache = tree();
    cache.invalidate_path(b"/");
    assert!(valid_paths(&cache).is_empty());
  }

  #[test]
  fn invalidates_trees() {
    let mut cache = tree();
    cache.invalidate_tree(cache.find(b"a/b").unwrap());
    assert_eq!(valid_paths(&cache), vec!["", "a", "d"]);
    cache.invalidate_tree(1000);
    cache.invalidate_tree(0);
    assert!(valid_paths(&cache).is_empty());
  }

  #[test]
  fn sorts_subdirectories_by_name_bytes() {
    let names = ["z", "a.b", "a", "A", "a-b", "a/"];
    let root = TestDir::valid("", &[], names.iter().map(|name| TestDir::valid(name, &[], vec![])).collect());
    let cache = cache(Some(root));
    let sorted: Vec<&[u8]> = cache.dirs[0].subdirs.iter().map(|dir| cache.dirs[*dir].name.as_slice()).collect();
    assert_eq!(sorted, vec![&b"A"[..], b"a", b"a-b", b"a.b", b"a/", b"z"]);
    for name in names {
      assert_eq!(
        cache.child(0, name.as_bytes()).map(|dir| cache.dirs[dir].name.as_slice()),
        Some(name.as_bytes())
      );
    }
  }

  fn write(root: Option<TestDir>) -> Vec<u8> {
    write_untracked_cache(&TestUntrackedCache {
      ident: ident(),
      dir_flags: 6,
      info_exclude_oid: vec![0; 20],
      excludes_file_oid: vec![0; 20],
      root,
    })
  }

  #[test]
  fn rejects_malformed_caches() {
    let duplicate = TestDir::valid("", &[], vec![TestDir::valid("a", &[], vec![]), TestDir::valid("a", &[], vec![])]);
    assert!(UntrackedCache::parse(&write(Some(duplicate)), 20).is_err());
    let good = write(Some(TestDir::valid("", &["x"], vec![TestDir::valid("a", &["y"], vec![])])));
    assert!(UntrackedCache::parse(&good, 20).is_ok());
    assert!(UntrackedCache::parse(&good, 32).is_err());
    let mut trailing = good.clone();
    trailing.push(0);
    assert!(UntrackedCache::parse(&trailing, 20).is_err());
    let mut unterminated = good.clone();
    unterminated[ident().len() + 1] = b'x';
    assert!(UntrackedCache::parse(&unterminated, 20).is_err());
    let mut empty = write(None);
    assert!(UntrackedCache::parse(&empty, 20).is_ok());
    empty.push(0);
    assert!(UntrackedCache::parse(&empty, 20).is_ok());
    empty.push(0);
    assert!(UntrackedCache::parse(&empty, 20).is_err());
  }

  #[test]
  fn rejects_wrong_directory_counts() {
    let good = write(Some(TestDir::valid("", &[], vec![TestDir::valid("a", &[], vec![])])));
    let count_at = ident().len() + 2 + 72 + 4 + 40 + ".gitignore\0".len();
    assert_eq!(good[count_at], 2);
    for count in [1, 3, 0x7f] {
      let mut bad = good.clone();
      bad[count_at] = count;
      assert!(UntrackedCache::parse(&bad, 20).is_err(), "{count}");
    }
  }

  #[test]
  fn never_panics_on_truncated_or_corrupted_input() {
    let mut deep = TestDir::valid("deep", &["u1", "u2/"], vec![]);
    deep.check_only = true;
    deep.has_exclude_file = true;
    let data = write(Some(TestDir::valid(
      "",
      &["x"],
      vec![TestDir::valid("a", &["y"], vec![deep]), TestDir::valid("b", &[], vec![])],
    )));
    assert!(UntrackedCache::parse(&data, 20).is_ok());
    for len in 0..data.len() {
      assert!(UntrackedCache::parse(&data[..len], 20).is_err(), "{len}");
    }
    for position in 0..data.len() {
      for value in [0, 1, 0x7f, 0x80, 0xff] {
        let mut corrupted = data.clone();
        corrupted[position] = value;
        if let Ok(mut cache) = UntrackedCache::parse(&corrupted, 20) {
          let _ = cache.find(b"a/deep");
          cache.invalidate_path(b"a/deep/");
          cache.invalidate_path(b"a/x");
        }
      }
    }
  }

  fn git_cache(repo: &TempRepo, untracked_files: &str) -> UntrackedCache {
    repo.git(&["status", "--porcelain", untracked_files]);
    repo.git(&["status", "--porcelain", untracked_files]);
    parse_index(&repo.read(".git/index"), 20).unwrap().untracked_cache.unwrap()
  }

  fn untracked_of(cache: &UntrackedCache, path: &str) -> Vec<String> {
    let mut names: Vec<String> = cache.dirs[cache.find(path.as_bytes()).unwrap()]
      .untracked
      .iter()
      .map(|name| String::from_utf8_lossy(name).into_owned())
      .collect();
    names.sort();
    names
  }

  #[test]
  fn reads_caches_git_writes() {
    let Some(repo) = TempRepo::new(&[]) else {
      return;
    };
    repo.git(&["config", "core.untrackedCache", "true"]);
    for (path, contents) in [
      (".gitignore", "ignored/\n*.log\n"),
      ("a.ts", ""),
      ("src/b.ts", ""),
      ("src/deep/c.ts", ""),
      ("src/deep/u.ts", ""),
      ("tracked/t.ts", ""),
      ("tracked/.gitignore", "z.ts\n"),
      ("tracked/z.ts", ""),
      ("tracked/y.ts", ""),
      ("tracked/inner/i.ts", ""),
      ("new.ts", ""),
      ("newdir/d.ts", ""),
      ("newdir/sub/s.ts", ""),
      ("x.log", ""),
      ("ignored/e.ts", ""),
    ] {
      repo.write(path, contents.as_bytes());
    }
    for index in 0..200 {
      repo.write(&format!("src/u{index}"), b"");
    }
    for index in 0..130 {
      repo.write(&format!("d{index}/t"), b"");
    }
    std::fs::create_dir_all(repo.path("emptydir")).unwrap();
    repo.git(&[
      "add",
      ".gitignore",
      "a.ts",
      "src/b.ts",
      "src/deep/c.ts",
      "tracked/t.ts",
      "tracked/.gitignore",
      "tracked/inner/i.ts",
    ]);
    repo.git(&["add", "d*/t"]);

    let cache = git_cache(&repo, "--untracked-files=normal");
    assert_eq!(cache.ident, format!("Location {}, system Linux", repo.root().display()).as_bytes());
    assert_eq!(cache.dir_flags, DIR_SHOW_OTHER_DIRECTORIES | DIR_HIDE_EMPTY_DIRECTORIES);
    assert_eq!(cache.exclude_per_dir, b".gitignore");
    assert_eq!(untracked_of(&cache, ""), vec!["new.ts", "newdir/"]);
    let mut expected: Vec<String> = (0..200).map(|index| format!("u{index}")).collect();
    expected.sort();
    assert_eq!(untracked_of(&cache, "src"), expected);
    assert_eq!(untracked_of(&cache, "src/deep"), vec!["u.ts"]);
    assert_eq!(untracked_of(&cache, "tracked"), vec!["y.ts"]);
    for path in ["", "src", "src/deep", "tracked", "tracked/inner", "newdir", "emptydir", "d0", "d129"] {
      assert!(cache.dirs[cache.find(path.as_bytes()).unwrap()].valid, "{path}");
    }
    assert!(cache.dirs[cache.find(b"newdir").unwrap()].check_only);
    assert!(!cache.dirs[cache.find(b"src").unwrap()].check_only);
    assert!(cache.dirs[0].has_exclude_file);
    assert!(cache.dirs[cache.find(b"tracked").unwrap()].has_exclude_file);
    assert_eq!(cache.dirs[0].subdirs.len(), 130 + 4);

    repo.write("tracked/added.ts", b"");
    repo.git(&["add", "tracked/added.ts"]);
    let cache = parse_index(&repo.read(".git/index"), 20).unwrap().untracked_cache.unwrap();
    assert!(!cache.dirs[0].valid);
    assert!(!cache.dirs[cache.find(b"tracked").unwrap()].valid);
    assert!(cache.dirs[cache.find(b"src").unwrap()].valid);
  }

  #[test]
  fn reads_an_empty_cache_git_writes() {
    let Some(repo) = TempRepo::new(&[]) else {
      return;
    };
    repo.write("a", b"a\n");
    repo.git(&["add", "a"]);
    repo.git(&["update-index", "--untracked-cache"]);
    let cache = parse_index(&repo.read(".git/index"), 20).unwrap().untracked_cache.unwrap();
    assert!(cache.dirs.is_empty());
    assert_eq!(cache.exclude_per_dir, b".gitignore");
  }

  #[test]
  fn reads_caches_in_sha256_repositories() {
    let Some(repo) = TempRepo::new(&["--object-format=sha256"]) else {
      return;
    };
    repo.git(&["config", "core.untrackedCache", "true"]);
    repo.write(".gitignore", b"*.log\n");
    repo.write("sub/new.ts", b"");
    repo.git(&["add", ".gitignore"]);
    repo.git(&["status", "--porcelain"]);
    let cache = parse_index(&repo.read(".git/index"), 32).unwrap().untracked_cache.unwrap();
    assert_eq!(cache.info_exclude_oid.len(), 32);
    assert_eq!(untracked_of(&cache, ""), vec!["sub/"]);
    assert!(cache.dirs[0].has_exclude_file);
  }
}
