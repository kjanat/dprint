use anyhow::Result;
use anyhow::bail;

use crate::ewah::read_ewah;
use crate::reader::Reader;
use crate::untracked_cache::UntrackedCache;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
  File,
  Symlink,
  Gitlink,
}

#[derive(Debug)]
pub struct IndexEntry {
  pub path: Vec<u8>,
  pub kind: EntryKind,
  pub stage: u8,
  pub skip_worktree: bool,
}

#[derive(Debug, Default)]
pub struct IndexFile {
  pub entries: Vec<IndexEntry>,
  pub fsmonitor: Option<FsmonitorData>,
  pub untracked_cache: Option<UntrackedCache>,
}

#[derive(Debug)]
pub struct FsmonitorData {
  pub token: Vec<u8>,
  pub dirty_entries: Vec<usize>,
}

const SIGNATURE: &[u8; 4] = b"DIRC";
const STAT_FIELDS_BEFORE_MODE: usize = 24;
const STAT_FIELDS_AFTER_MODE: usize = 12;
const EXTENDED_FLAG: u16 = 0x4000;
const STAGE_SHIFT: u16 = 12;
const NAME_LEN_MASK: u16 = 0xfff;
const SKIP_WORKTREE_FLAG: u16 = 0x4000;

pub fn parse_index(data: &[u8], hash_len: usize) -> Result<IndexFile> {
  if hash_len != 20 && hash_len != 32 {
    bail!("unsupported hash length {hash_len}");
  }
  let Some(body_len) = data.len().checked_sub(hash_len) else {
    bail!("the index is shorter than its checksum");
  };
  let mut reader = Reader::new(&data[..body_len]);
  if reader.bytes(4)? != SIGNATURE {
    bail!("the index doesn't start with DIRC");
  }
  let version = reader.u32()?;
  if !(2..=4).contains(&version) {
    bail!("unsupported index version {version}");
  }
  let count = reader.usize32()?;
  let mut entries: Vec<IndexEntry> = Vec::with_capacity(count.min(body_len / (STAT_FIELDS_BEFORE_MODE + 4)));
  for _ in 0..count {
    let previous_path = entries.last().map_or(&[][..], |entry| entry.path.as_slice());
    let entry = read_entry(&mut reader, version, hash_len, previous_path)?;
    if let Some(previous) = entries.last()
      && !is_ordered(previous, &entry)
    {
      bail!(
        "the index lists {:?} stage {} after {:?} stage {}",
        String::from_utf8_lossy(&entry.path),
        entry.stage,
        String::from_utf8_lossy(&previous.path),
        previous.stage
      );
    }
    entries.push(entry);
  }

  let mut index = IndexFile {
    entries,
    fsmonitor: None,
    untracked_cache: None,
  };
  let mut has_fsmonitor = false;
  while !reader.is_empty() {
    let signature: [u8; 4] = reader.array()?;
    let size = reader.usize32()?;
    let payload = reader.bytes(size)?;
    match &signature {
      b"FSMN" => {
        if std::mem::replace(&mut has_fsmonitor, true) {
          bail!("the index has two FSMN extensions");
        }
        index.fsmonitor = parse_fsmonitor(payload, index.entries.len())?;
      }
      b"UNTR" => {
        if index.untracked_cache.is_some() {
          bail!("the index has two UNTR extensions");
        }
        index.untracked_cache = Some(UntrackedCache::parse(payload, hash_len)?);
      }
      // gitformat-index.adoc, "Extensions", makes signatures starting with 'A'..'Z' optional.
      [b'A'..=b'Z', ..] => {}
      _ => bail!("the index uses the {} extension", String::from_utf8_lossy(&signature)),
    }
  }
  Ok(index)
}

fn read_entry(reader: &mut Reader, version: u32, hash_len: usize, previous_path: &[u8]) -> Result<IndexEntry> {
  let start = reader.position();
  reader.skip(STAT_FIELDS_BEFORE_MODE)?;
  let mode = reader.u32()?;
  reader.skip(STAT_FIELDS_AFTER_MODE + hash_len)?;
  let flags = reader.u16()?;
  let extended_flags = if flags & EXTENDED_FLAG != 0 { reader.u16()? } else { 0 };
  let path = if version == 4 {
    let strip = reader.varint()?;
    let suffix = reader.nul_terminated()?;
    let Some(keep) = previous_path.len().checked_sub(strip) else {
      bail!("an index entry removes {strip} bytes from a {}-byte path", previous_path.len());
    };
    [&previous_path[..keep], suffix].concat()
  } else {
    let name = reader.nul_terminated()?.to_vec();
    let len = reader.position() - start;
    let padding = reader.bytes(len.next_multiple_of(8) - len)?;
    if padding.iter().any(|byte| *byte != 0) {
      bail!("an index entry has non-NUL padding");
    }
    name
  };
  let name_len = usize::from(flags & NAME_LEN_MASK);
  if path.is_empty() || (name_len < usize::from(NAME_LEN_MASK) && path.len() != name_len) || path.len() < name_len {
    bail!("an index entry's path {:?} doesn't match its length {name_len}", String::from_utf8_lossy(&path));
  }
  let kind = match mode >> 12 {
    0b1000 => EntryKind::File,
    0b1010 => EntryKind::Symlink,
    0b1110 => EntryKind::Gitlink,
    _ => bail!("the index entry {:?} has mode {mode:o}", String::from_utf8_lossy(&path)),
  };
  Ok(IndexEntry {
    path,
    kind,
    stage: ((flags >> STAGE_SHIFT) & 3) as u8,
    skip_worktree: extended_flags & SKIP_WORKTREE_FLAG != 0,
  })
}

fn is_ordered(previous: &IndexEntry, entry: &IndexEntry) -> bool {
  match previous.path.cmp(&entry.path) {
    std::cmp::Ordering::Less => true,
    std::cmp::Ordering::Equal => previous.stage != 0 && previous.stage < entry.stage,
    std::cmp::Ordering::Greater => false,
  }
}

fn parse_fsmonitor(payload: &[u8], entry_count: usize) -> Result<Option<FsmonitorData>> {
  let mut reader = Reader::new(payload);
  match reader.u32()? {
    1 => return Ok(None),
    2 => {}
    version => bail!("unsupported FSMN version {version}"),
  }
  let token = reader.nul_terminated()?.to_vec();
  let bitmap_len = reader.usize32()?;
  let mut bitmap = Reader::new(reader.bytes(bitmap_len)?);
  let dirty_entries = read_ewah(&mut bitmap, entry_count)?;
  if !bitmap.is_empty() || !reader.is_empty() {
    bail!("the FSMN extension has trailing data");
  }
  Ok(Some(FsmonitorData { token, dirty_entries }))
}

#[cfg(any(test, feature = "test-util"))]
pub(crate) mod test_writer {
  use crate::ewah::write_ewah;
  use crate::hash::blob_oid;
  use crate::hash::digest;
  use crate::reader::write_varint;

  pub struct TestEntry {
    pub path: String,
    pub mode: u32,
    pub stage: u8,
    pub skip_worktree: bool,
  }

  impl TestEntry {
    pub fn file(path: &str) -> Self {
      Self::with_mode(path, 0o100644)
    }

    pub fn with_mode(path: &str, mode: u32) -> Self {
      TestEntry {
        path: path.to_string(),
        mode,
        stage: 0,
        skip_worktree: false,
      }
    }
  }

  pub fn write_index(version: u32, hash_len: usize, entries: &[TestEntry], extensions: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
    let oid = blob_oid(&[b""], hash_len).unwrap_or_else(|| panic!("unsupported hash length {hash_len}"));
    let mut out = super::SIGNATURE.to_vec();
    out.extend_from_slice(&version.to_be_bytes());
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    let mut previous_path: &[u8] = b"";
    for entry in entries {
      let start = out.len();
      let path = entry.path.as_bytes();
      out.extend_from_slice(&[0; super::STAT_FIELDS_BEFORE_MODE]);
      out.extend_from_slice(&entry.mode.to_be_bytes());
      out.extend_from_slice(&[0; super::STAT_FIELDS_AFTER_MODE]);
      out.extend_from_slice(&oid);
      let mut flags = (u16::from(entry.stage) << super::STAGE_SHIFT) | path.len().min(usize::from(super::NAME_LEN_MASK)) as u16;
      if entry.skip_worktree {
        flags |= super::EXTENDED_FLAG;
      }
      out.extend_from_slice(&flags.to_be_bytes());
      if entry.skip_worktree {
        out.extend_from_slice(&super::SKIP_WORKTREE_FLAG.to_be_bytes());
      }
      if version == 4 {
        let common = previous_path.iter().zip(path).take_while(|(a, b)| a == b).count();
        write_varint(&mut out, previous_path.len() - common);
        out.extend_from_slice(&path[common..]);
        out.push(0);
      } else {
        out.extend_from_slice(path);
        out.push(0);
        let len = out.len() - start;
        out.resize(start + len.next_multiple_of(8), 0);
      }
      previous_path = path;
    }
    for (signature, payload) in extensions {
      out.extend_from_slice(*signature);
      out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
      out.extend_from_slice(payload);
    }
    let checksum = digest(&[&out], hash_len).unwrap_or_default();
    out.extend_from_slice(&checksum);
    out
  }

  pub fn fsmonitor_extension(token: &str, dirty_entries: &[usize]) -> Vec<u8> {
    let mut bitmap = Vec::new();
    write_ewah(&mut bitmap, dirty_entries);
    let mut out = 2u32.to_be_bytes().to_vec();
    out.extend_from_slice(token.as_bytes());
    out.push(0);
    out.extend_from_slice(&(bitmap.len() as u32).to_be_bytes());
    out.extend_from_slice(&bitmap);
    out
  }
}

#[cfg(test)]
mod test {
  use super::test_writer::*;
  use super::*;
  use crate::test_git::TempRepo;
  use crate::untracked_cache::test_writer::*;

  fn entry(path: &str, stage: u8) -> TestEntry {
    TestEntry {
      stage,
      ..TestEntry::file(path)
    }
  }

  fn summary(index: &IndexFile) -> Vec<(String, EntryKind, u8, bool)> {
    index
      .entries
      .iter()
      .map(|entry| (String::from_utf8_lossy(&entry.path).into_owned(), entry.kind, entry.stage, entry.skip_worktree))
      .collect()
  }

  fn varied_entries() -> Vec<TestEntry> {
    let long_a = format!("{}/a", "d".repeat(300));
    let long_b = format!("{}/b", "e".repeat(5000));
    vec![
      entry("conflict", 1),
      entry("conflict", 2),
      entry("conflict", 3),
      TestEntry::file(&long_a),
      TestEntry::file("dir/sub/aaaaaaaa"),
      TestEntry::file("dir/sub/aaaabbbb"),
      TestEntry::file(&long_b),
      TestEntry::with_mode("exe", 0o100755),
      TestEntry::with_mode("gitlink", 0o160000),
      TestEntry::with_mode("link", 0o120000),
      TestEntry {
        skip_worktree: true,
        ..TestEntry::file("sparse")
      },
      TestEntry::file("z"),
    ]
  }

  fn expected_summary(entries: &[TestEntry]) -> Vec<(String, EntryKind, u8, bool)> {
    entries
      .iter()
      .map(|entry| {
        let kind = match entry.mode {
          0o160000 => EntryKind::Gitlink,
          0o120000 => EntryKind::Symlink,
          _ => EntryKind::File,
        };
        (entry.path.clone(), kind, entry.stage, entry.skip_worktree)
      })
      .collect()
  }

  #[test]
  fn round_trips_every_version_and_hash() {
    let entries = varied_entries();
    for version in [2, 3, 4] {
      for hash_len in [20, 32] {
        let index = parse_index(&write_index(version, hash_len, &entries, &[]), hash_len).unwrap();
        assert_eq!(summary(&index), expected_summary(&entries), "version {version} hash {hash_len}");
        assert!(index.fsmonitor.is_none());
        assert!(index.untracked_cache.is_none());
      }
    }
  }

  #[test]
  fn reads_an_empty_index() {
    let index = parse_index(&write_index(2, 20, &[], &[]), 20).unwrap();
    assert!(index.entries.is_empty());
  }

  #[test]
  fn writes_the_documented_layout() {
    let data = write_index(2, 20, &[TestEntry::file("a")], &[]);
    assert_eq!(&data[..12], b"DIRC\0\0\0\x02\0\0\0\x01");
    assert_eq!(&data[36..40], &0o100644u32.to_be_bytes());
    assert_eq!(&data[72..76], b"\0\x01a\0");
    assert_eq!(data.len(), 12 + 64 + 20);
    let data = write_index(4, 20, &[TestEntry::file("ab"), TestEntry::file("ac")], &[]);
    assert_eq!(&data[12 + 62..12 + 62 + 4], b"\0ab\0");
    assert_eq!(&data[12 + 66 + 62..12 + 66 + 62 + 3], b"\x01c\0");
  }

  #[test]
  fn reads_the_fsmonitor_token_and_dirty_entries() {
    let entries = ["a", "b", "c", "d"].map(TestEntry::file);
    let index = parse_index(&write_index(2, 20, &entries, &[(b"FSMN", fsmonitor_extension("builtin:x:1", &[1, 3]))]), 20).unwrap();
    let fsmonitor = index.fsmonitor.unwrap();
    assert_eq!(fsmonitor.token, b"builtin:x:1");
    assert_eq!(fsmonitor.dirty_entries, vec![1, 3]);
    let index = parse_index(&write_index(2, 20, &entries, &[(b"FSMN", fsmonitor_extension("t", &[]))]), 20).unwrap();
    assert!(index.fsmonitor.unwrap().dirty_entries.is_empty());
  }

  #[test]
  fn writes_the_fsmonitor_extension_git_writes() {
    let mut expected = 2u32.to_be_bytes().to_vec();
    expected.extend_from_slice(b"tok:2\0");
    expected.extend_from_slice(&[0, 0, 0, 28, 0, 0, 0, 2, 0, 0, 0, 2, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0]);
    assert_eq!(fsmonitor_extension("tok:2", &[1]), expected);
  }

  #[test]
  fn ignores_the_timestamp_fsmonitor_version() {
    let mut payload = 1u32.to_be_bytes().to_vec();
    payload.extend_from_slice(&1_700_000_000_000_000_000u64.to_be_bytes());
    payload.extend_from_slice(&20u32.to_be_bytes());
    crate::ewah::write_ewah(&mut payload, &[]);
    let index = parse_index(&write_index(2, 20, &[TestEntry::file("a")], &[(b"FSMN", payload)]), 20).unwrap();
    assert!(index.fsmonitor.is_none());
  }

  #[test]
  fn rejects_bad_fsmonitor_extensions() {
    let entries = ["a", "b"].map(TestEntry::file);
    let parse = |payload: Vec<u8>| parse_index(&write_index(2, 20, &entries, &[(b"FSMN", payload)]), 20);
    assert!(parse(fsmonitor_extension("t", &[2])).is_err());
    let mut unknown = fsmonitor_extension("t", &[]);
    unknown[3] = 3;
    assert!(parse(unknown).is_err());
    let mut trailing = fsmonitor_extension("t", &[]);
    trailing.push(0);
    assert!(parse(trailing).is_err());
    let good = fsmonitor_extension("t", &[1]);
    for len in 0..good.len() {
      assert!(parse(good[..len].to_vec()).is_err(), "{len}");
    }
    let twice = [(b"FSMN", fsmonitor_extension("t", &[])), (b"FSMN", fsmonitor_extension("t", &[]))];
    assert!(parse_index(&write_index(2, 20, &entries, &twice), 20).is_err());
  }

  #[test]
  fn reads_the_untracked_cache() {
    let cache = write_untracked_cache(&TestUntrackedCache {
      ident: "Location /repo, system Linux".to_string(),
      dir_flags: 6,
      info_exclude_oid: vec![1; 32],
      excludes_file_oid: vec![2; 32],
      root: Some(TestDir::valid("", &["new.ts"], vec![TestDir::valid("src", &[], vec![])])),
    });
    let index = parse_index(&write_index(3, 32, &[TestEntry::file("src/a.ts")], &[(b"UNTR", cache)]), 32).unwrap();
    let cache = index.untracked_cache.unwrap();
    assert_eq!(cache.ident, b"Location /repo, system Linux");
    assert_eq!(cache.find(b"src"), Some(1));
    let twice = [(b"UNTR", Vec::new()), (b"UNTR", Vec::new())];
    assert!(parse_index(&write_index(2, 20, &[], &twice), 20).is_err());
  }

  #[test]
  fn skips_optional_extensions_and_rejects_required_ones() {
    let entries = [TestEntry::file("a")];
    let optional = [(b"TREE", b"\x001 0\n".to_vec()), (b"ZZZZ", vec![1, 2, 3]), (b"EOIE", vec![0; 24])];
    assert_eq!(parse_index(&write_index(2, 20, &entries, &optional), 20).unwrap().entries.len(), 1);
    let err = parse_index(&write_index(2, 20, &entries, &[(b"link", vec![0; 20])]), 20).unwrap_err();
    assert!(err.to_string().contains("the index uses the link extension"), "{err}");
    let err = parse_index(&write_index(2, 20, &entries, &[(b"sdir", Vec::new())]), 20).unwrap_err();
    assert!(err.to_string().contains("the index uses the sdir extension"), "{err}");
  }

  #[test]
  fn rejects_misordered_entries() {
    let cases: &[&[TestEntry]] = &[
      &[TestEntry::file("b"), TestEntry::file("a")],
      &[TestEntry::file("a"), TestEntry::file("a")],
      &[TestEntry::file("a"), entry("a", 1)],
      &[entry("a", 1), TestEntry::file("a")],
      &[entry("a", 2), entry("a", 1)],
      &[entry("a", 2), entry("a", 2)],
      &[TestEntry::file("a/b"), TestEntry::file("a.b")],
    ];
    for entries in cases {
      for version in [2, 4] {
        assert!(parse_index(&write_index(version, 20, entries, &[]), 20).is_err());
      }
    }
    let ordered = [
      TestEntry::file("a"),
      entry("a-b", 1),
      entry("a-b", 3),
      TestEntry::file("a.b"),
      TestEntry::file("a/b"),
      TestEntry::file("a0"),
    ];
    assert_eq!(parse_index(&write_index(2, 20, &ordered, &[]), 20).unwrap().entries.len(), 6);
  }

  #[test]
  fn rejects_unknown_modes() {
    for mode in [0o040000, 0o100000 | (1 << 16), 0o060644, 0] {
      assert!(
        parse_index(&write_index(2, 20, &[TestEntry::with_mode("a", mode)], &[]), 20).is_err(),
        "{mode:o}"
      );
    }
    for mode in [0o100600, 0o100777, 0o120777] {
      assert!(
        parse_index(&write_index(2, 20, &[TestEntry::with_mode("a", mode)], &[]), 20).is_ok(),
        "{mode:o}"
      );
    }
  }

  #[test]
  fn rejects_bad_headers() {
    let good = write_index(2, 20, &[TestEntry::file("a")], &[]);
    assert!(parse_index(&good, 21).is_err());
    let mut bad = good.clone();
    bad[0] = b'X';
    assert!(parse_index(&bad, 20).is_err());
    for version in [0u32, 1, 5] {
      let mut bad = good.clone();
      bad[4..8].copy_from_slice(&version.to_be_bytes());
      assert!(parse_index(&bad, 20).is_err());
    }
    let mut bad = good.clone();
    bad[8..12].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(parse_index(&bad, 20).is_err());
  }

  #[test]
  fn rejects_inconsistent_entries() {
    let good = write_index(2, 20, &[TestEntry::file("abc")], &[]);
    let mut bad = good.clone();
    bad[12 + 61] = 2;
    assert!(parse_index(&bad, 20).is_err());
    let mut bad = good.clone();
    bad[12 + 62 + 3 + 1] = 1;
    assert!(parse_index(&bad, 20).is_err());
    let v4 = write_index(4, 20, &[TestEntry::file("ab"), TestEntry::file("ac")], &[]);
    let mut bad = v4.clone();
    bad[12 + 66 + 62] = 3;
    assert!(parse_index(&bad, 20).is_err());
  }

  #[test]
  fn never_panics_on_truncated_or_corrupted_input() {
    let cache = write_untracked_cache(&TestUntrackedCache {
      ident: "Location /r, system Linux".to_string(),
      dir_flags: 6,
      info_exclude_oid: vec![0; 20],
      excludes_file_oid: vec![0; 20],
      root: Some(TestDir::valid("", &["x"], vec![TestDir::valid("d", &["y/"], vec![])])),
    });
    let fsmonitor = fsmonitor_extension("tok", &[0, 3]);
    for version in [2, 3, 4] {
      let data = write_index(version, 20, &varied_entries(), &[(b"FSMN", fsmonitor.clone()), (b"UNTR", cache.clone())]);
      assert!(parse_index(&data, 20).is_ok());
      let extensions_end = data.len() - 20;
      let after_fsmonitor = extensions_end - 8 - cache.len();
      let entries_end = after_fsmonitor - 8 - fsmonitor.len();
      for len in 0..extensions_end {
        let mut truncated = data[..len].to_vec();
        truncated.extend_from_slice(&[0; 20]);
        let result = parse_index(&truncated, 20);
        if len == entries_end || len == after_fsmonitor {
          assert!(result.is_ok(), "version {version} length {len}");
        } else {
          assert!(result.is_err(), "version {version} length {len}");
        }
        let _ = parse_index(&data[..len], 20);
      }
      for position in (0..data.len()).step_by(7) {
        for value in [0, 0x7f, 0x80, 0xff] {
          let mut corrupted = data.clone();
          corrupted[position] = value;
          let _ = parse_index(&corrupted, 20);
        }
      }
    }
  }

  fn ls_files(repo: &TempRepo) -> Vec<(String, EntryKind, u8, bool)> {
    let output = repo.git(&["ls-files", "--stage", "-t", "-z"]);
    output
      .split(|byte| *byte == 0)
      .filter(|line| !line.is_empty())
      .map(|line| {
        let line = String::from_utf8_lossy(line);
        let (tag, rest) = line.split_once(' ').unwrap();
        let (info, path) = rest.split_once('\t').unwrap();
        let fields: Vec<&str> = info.split(' ').collect();
        let kind = match fields[0] {
          "160000" => EntryKind::Gitlink,
          "120000" => EntryKind::Symlink,
          _ => EntryKind::File,
        };
        (path.to_string(), kind, fields[2].parse().unwrap(), tag == "S")
      })
      .collect()
  }

  #[test]
  fn git_accepts_written_indexes() {
    let entries: Vec<TestEntry> = varied_entries().into_iter().filter(|entry| !entry.skip_worktree).collect();
    for version in [2, 3, 4] {
      for (format, hash_len) in [("sha1", 20), ("sha256", 32)] {
        let Some(repo) = TempRepo::new(&[&format!("--object-format={format}")]) else {
          return;
        };
        std::fs::write(repo.path(".git/index"), write_index(version, hash_len, &entries, &[])).unwrap();
        assert_eq!(ls_files(&repo), expected_summary(&entries), "version {version} {format}");
      }
    }
  }

  #[test]
  fn git_accepts_written_extensions() {
    let Some(repo) = TempRepo::new(&[]) else {
      return;
    };
    let entries = ["a", "b"].map(TestEntry::file);
    let cache = write_untracked_cache(&TestUntrackedCache {
      ident: format!("Location {}, system Linux", repo.root().display()),
      dir_flags: 6,
      info_exclude_oid: vec![0; 20],
      excludes_file_oid: vec![0; 20],
      root: Some(TestDir::valid("", &["new"], vec![])),
    });
    let extensions = [(b"FSMN", fsmonitor_extension("tok", &[1])), (b"UNTR", cache)];
    std::fs::write(repo.path(".git/index"), write_index(2, 20, &entries, &extensions)).unwrap();
    let output = repo.try_git(&["ls-files"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(output.stderr.is_empty(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout, b"a\nb\n");
  }

  #[test]
  fn reads_indexes_git_writes() {
    for version in ["2", "3", "4"] {
      let Some(repo) = TempRepo::new(&["-b", "main"]) else {
        return;
      };
      repo.git(&["config", "index.version", version]);
      repo.write("conflict", b"base\n");
      repo.write("dir/sub/aaaaaaaa", b"x\n");
      repo.write("dir/sub/aaaabbbb", b"y\n");
      repo.write(&format!("{}/a", "d".repeat(200)), b"long\n");
      repo.write(&format!("{}/b", "e".repeat(200)), b"long\n");
      repo.write("sparse", b"s\n");
      repo.write("exe", b"e\n");
      repo.git(&["add", "."]);
      repo.git(&["update-index", "--chmod=+x", "exe"]);
      repo.git(&["commit", "-q", "-m", "one"]);
      repo.git(&["checkout", "-q", "-b", "other"]);
      repo.write("conflict", b"other\n");
      repo.git(&["commit", "-q", "-am", "other"]);
      repo.git(&["checkout", "-q", "main"]);
      repo.write("conflict", b"main\n");
      repo.git(&["commit", "-q", "-am", "main"]);
      assert!(!repo.try_git(&["merge", "-q", "other"]).status.success());
      repo.git(&["update-index", "--skip-worktree", "sparse"]);
      let head = String::from_utf8(repo.git(&["rev-parse", "HEAD"])).unwrap();
      repo.git(&["update-index", "--add", "--cacheinfo", &format!("160000,{},gitlink", head.trim())]);
      let blob = String::from_utf8(repo.git(&["hash-object", "-w", "exe"])).unwrap();
      repo.git(&["update-index", "--add", "--cacheinfo", &format!("120000,{},link", blob.trim())]);
      let index = parse_index(&repo.read(".git/index"), 20).unwrap();
      let expected = ls_files(&repo);
      assert_eq!(summary(&index), expected, "version {version}");
      assert!(expected.iter().any(|(path, _, stage, _)| path == "conflict" && *stage == 3));
      assert!(expected.iter().any(|(path, _, _, skip)| path == "sparse" && *skip));
    }
  }

  #[test]
  fn reads_fsmonitor_data_git_writes() {
    let Some(repo) = TempRepo::new(&[]) else {
      return;
    };
    let hook = repo.path(".git/fsmonitor-hook");
    std::fs::write(&hook, "printf 'tok:1\\0/'\n").unwrap();
    repo.git(&["config", "core.fsmonitor", &format!("sh {}", hook.display())]);
    repo.git(&["config", "core.fsmonitorHookVersion", "2"]);
    for path in ["a", "b", "c", "sub/d"] {
      repo.write(path, path.as_bytes());
    }
    repo.git(&["add", "."]);
    repo.git(&["status", "--porcelain"]);
    let fsmonitor = parse_index(&repo.read(".git/index"), 20).unwrap().fsmonitor.unwrap();
    assert_eq!(fsmonitor.token, b"tok:1");
    assert!(fsmonitor.dirty_entries.is_empty());
    std::fs::write(&hook, "printf 'tok:2\\0b\\0sub/d\\0'\n").unwrap();
    repo.write("b", b"changed");
    repo.git(&["status", "--porcelain"]);
    let fsmonitor = parse_index(&repo.read(".git/index"), 20).unwrap().fsmonitor.unwrap();
    assert_eq!(fsmonitor.token, b"tok:2");
    assert_eq!(fsmonitor.dirty_entries, vec![1]);
  }
}
