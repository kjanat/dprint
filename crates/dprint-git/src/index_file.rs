//! Reads git's index file ([`Documentation/gitformat-index.adoc`],
//! [`create_from_disk`] and [`read_index_extension`] in [`read-cache.c`]).
//!
//! [`Documentation/gitformat-index.adoc`]: https://github.com/git/git/blob/6de20f6092dcf9bdb1c8efe03db4b70c82b423dd/Documentation/gitformat-index.adoc
//! [`read-cache.c`]: https://github.com/git/git/blob/6de20f6092dcf9bdb1c8efe03db4b70c82b423dd/read-cache.c
//! [`create_from_disk`]: https://github.com/git/git/blob/6de20f6092dcf9bdb1c8efe03db4b70c82b423dd/read-cache.c#L1781-L1892
//! [`read_index_extension`]: https://github.com/git/git/blob/6de20f6092dcf9bdb1c8efe03db4b70c82b423dd/read-cache.c#L1743-L1779

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;

use crate::bytes::ByteReader;
use crate::ewah::read_ewah;
use crate::untracked_cache::UntrackedCache;

const STAT_DATA_LEN: usize = 40;
const FLAG_NAME_MASK: u16 = 0x0fff;
const FLAG_STAGE_MASK: u16 = 0x3000;
const FLAG_STAGE_SHIFT: u16 = 12;
const FLAG_EXTENDED: u16 = 0x4000;
const EXTENDED_FLAG_SKIP_WORKTREE: u16 = 0x4000;

const MODE_TYPE_MASK: u32 = 0o170000;
const MODE_REGULAR: u32 = 0o100000;
const MODE_SYMLINK: u32 = 0o120000;
const MODE_GITLINK: u32 = 0o160000;

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
  /// 0 for a merged path, 1 to 3 for the sides of a conflict.
  pub stage: u8,
  pub skip_worktree: bool,
}

#[derive(Debug, Default)]
pub struct IndexFile {
  /// Sorted by path, then by stage. A conflicted path appears once per stage.
  pub entries: Vec<IndexEntry>,
  /// `None` without the extension or with a protocol V1 timestamp.
  pub fsmonitor: Option<FsmonitorData>,
  pub untracked_cache: Option<UntrackedCache>,
}

#[derive(Debug)]
pub struct FsmonitorData {
  pub token: Vec<u8>,
  /// Positions in `IndexFile::entries` without git's fsmonitor-valid flag.
  pub dirty_entries: Vec<usize>,
}

pub fn parse_index(data: &[u8], hash_len: usize) -> Result<IndexFile> {
  if data.len() < 12 + hash_len {
    bail!("the index is too short");
  }
  let mut reader = ByteReader::new(&data[..data.len() - hash_len]);
  if reader.take(4)? != b"DIRC" {
    bail!("the index has no DIRC signature");
  }
  let version = reader.u32()?;
  if !(2..=4).contains(&version) {
    bail!("unsupported index version {}", version);
  }
  let entry_count = reader.u32()? as usize;
  let mut entries = Vec::with_capacity(entry_count.min(1 << 20));
  for _ in 0..entry_count {
    let previous = entries.last().map(|entry: &IndexEntry| entry.path.as_slice());
    let entry = read_entry(&mut reader, version, hash_len, previous).with_context(|| format!("reading index entry {}", entries.len()))?;
    entries.push(entry);
  }
  check_entry_order(&entries)?;

  let mut index = IndexFile {
    entries,
    fsmonitor: None,
    untracked_cache: None,
  };
  while !reader.is_empty() {
    let signature = reader.take(4)?;
    let len = reader.u32()? as usize;
    let extension = reader.take(len)?;
    match signature {
      b"FSMN" => index.fsmonitor = read_fsmonitor(extension).context("reading the FSMN extension")?,
      b"UNTR" => index.untracked_cache = Some(UntrackedCache::parse(extension, hash_len).context("reading the UNTR extension")?),
      [b'A'..=b'Z', ..] => {}
      _ => bail!("the index uses the {} extension", String::from_utf8_lossy(signature)),
    }
  }
  Ok(index)
}

/// Git's `check_ce_order`.
fn check_entry_order(entries: &[IndexEntry]) -> Result<()> {
  for pair in entries.windows(2) {
    match pair[0].path.cmp(&pair[1].path) {
      std::cmp::Ordering::Less => {}
      std::cmp::Ordering::Greater => bail!("unordered stage entries in index"),
      std::cmp::Ordering::Equal if pair[0].stage == 0 => {
        bail!("multiple stage entries for merged file '{}'", String::from_utf8_lossy(&pair[0].path))
      }
      std::cmp::Ordering::Equal if pair[0].stage > pair[1].stage => {
        bail!("unordered stage entries for '{}'", String::from_utf8_lossy(&pair[0].path))
      }
      std::cmp::Ordering::Equal => {}
    }
  }
  Ok(())
}

fn read_entry(reader: &mut ByteReader, version: u32, hash_len: usize, previous_path: Option<&[u8]>) -> Result<IndexEntry> {
  let start = reader.pos();
  let stat = reader.take(STAT_DATA_LEN)?;
  let mode = u32::from_be_bytes(stat[24..28].try_into()?);
  reader.take(hash_len)?;
  let flags = reader.u16()?;
  let extended_flags = if flags & FLAG_EXTENDED != 0 { reader.u16()? } else { 0 };
  let path = if version == 4 {
    let strip_len = usize::try_from(reader.varint()?)?;
    let previous_path = previous_path.unwrap_or_default();
    let Some(keep_len) = previous_path.len().checked_sub(strip_len) else {
      bail!("the entry strips {} bytes from a {} byte path", strip_len, previous_path.len());
    };
    let suffix = reader.c_str()?;
    let mut path = Vec::with_capacity(keep_len + suffix.len());
    path.extend_from_slice(&previous_path[..keep_len]);
    path.extend_from_slice(suffix);
    path
  } else {
    let path = reader.c_str()?.to_vec();
    let name_len = usize::from(flags & FLAG_NAME_MASK);
    if name_len != usize::from(FLAG_NAME_MASK) && name_len != path.len() {
      bail!("the entry's name length {} doesn't match its {} byte path", name_len, path.len());
    }
    let unpadded_len = reader.pos() - 1 - start;
    reader.skip_to(start + ((unpadded_len + 8) & !7))?;
    path
  };
  let kind = match mode & MODE_TYPE_MASK {
    MODE_REGULAR => EntryKind::File,
    MODE_SYMLINK => EntryKind::Symlink,
    MODE_GITLINK => EntryKind::Gitlink,
    _ => bail!("unknown mode {:o}", mode),
  };
  Ok(IndexEntry {
    path,
    kind,
    stage: ((flags & FLAG_STAGE_MASK) >> FLAG_STAGE_SHIFT) as u8,
    skip_worktree: extended_flags & EXTENDED_FLAG_SKIP_WORKTREE != 0,
  })
}

fn read_fsmonitor(data: &[u8]) -> Result<Option<FsmonitorData>> {
  let mut reader = ByteReader::new(data);
  let token = match reader.u32()? {
    1 => {
      reader.u64()?;
      None
    }
    2 => Some(reader.c_str()?.to_vec()),
    version => bail!("unknown version {}", version),
  };
  let ewah_len = reader.u32()? as usize;
  let mut ewah_reader = ByteReader::new(reader.take(ewah_len)?);
  let dirty_entries = read_ewah(&mut ewah_reader)?;
  if !ewah_reader.is_empty() || !reader.is_empty() {
    bail!("trailing data");
  }
  Ok(token.map(|token| FsmonitorData { token, dirty_entries }))
}

#[cfg(any(test, feature = "test-util"))]
pub(crate) mod test_writer {
  //! Writes index files like git's `do_write_index`.

  use crate::bytes::encode_varint;
  use crate::ewah::write_ewah;

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
      Self {
        path: path.to_string(),
        mode,
        stage: 0,
        skip_worktree: false,
      }
    }
  }

  pub fn write_index(version: u32, hash_len: usize, entries: &[TestEntry], extensions: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
    let mut bytes = b"DIRC".to_vec();
    bytes.extend(version.to_be_bytes());
    bytes.extend((entries.len() as u32).to_be_bytes());
    let mut previous: &[u8] = b"";
    for entry in entries {
      let start = bytes.len();
      let mut stat = [0u8; 40];
      stat[24..28].copy_from_slice(&entry.mode.to_be_bytes());
      bytes.extend(stat);
      bytes.extend(vec![0xab; hash_len]);
      let path = entry.path.as_bytes();
      let mut flags = path.len().min(0xfff) as u16 | u16::from(entry.stage) << 12;
      if entry.skip_worktree {
        flags |= 0x4000;
      }
      bytes.extend(flags.to_be_bytes());
      if entry.skip_worktree {
        bytes.extend(0x4000u16.to_be_bytes());
      }
      if version == 4 {
        let common = previous.iter().zip(path).take_while(|(a, b)| a == b).count();
        bytes.extend(encode_varint((previous.len() - common) as u64));
        bytes.extend(&path[common..]);
        bytes.push(0);
      } else {
        bytes.extend(path);
        let len = bytes.len() - start;
        bytes.extend(vec![0; ((len + 8) & !7) - len]);
      }
      previous = path;
    }
    for (signature, data) in extensions {
      bytes.extend(signature.as_slice());
      bytes.extend((data.len() as u32).to_be_bytes());
      bytes.extend(data);
    }
    bytes.extend(vec![0; hash_len]);
    bytes
  }

  pub fn fsmonitor_extension(token: &str, dirty_entries: &[usize]) -> Vec<u8> {
    let mut bytes = 2u32.to_be_bytes().to_vec();
    bytes.extend(token.as_bytes());
    bytes.push(0);
    let ewah = write_ewah(dirty_entries);
    bytes.extend((ewah.len() as u32).to_be_bytes());
    bytes.extend(ewah);
    bytes
  }
}

#[cfg(test)]
mod test {
  use super::test_writer::*;
  use super::*;

  #[test]
  fn reads_entries_of_every_version() {
    let entries = [
      TestEntry::file("a.txt"),
      TestEntry::file("dir/b.txt"),
      TestEntry::file("dir/c.txt"),
      TestEntry::with_mode("dir/link", 0o120000),
      TestEntry {
        skip_worktree: true,
        ..TestEntry::with_mode("dir/sparse.txt", 0o100755)
      },
      TestEntry::with_mode("sub", 0o160000),
    ];
    for version in [2, 3, 4] {
      for hash_len in [20, 32] {
        let index = parse_index(&write_index(version, hash_len, &entries, &[]), hash_len).unwrap();
        let paths = index
          .entries
          .iter()
          .map(|entry| String::from_utf8(entry.path.clone()).unwrap())
          .collect::<Vec<_>>();
        assert_eq!(
          paths,
          ["a.txt", "dir/b.txt", "dir/c.txt", "dir/link", "dir/sparse.txt", "sub"],
          "version {version}"
        );
        let kinds = index.entries.iter().map(|entry| entry.kind).collect::<Vec<_>>();
        assert_eq!(
          kinds,
          [
            EntryKind::File,
            EntryKind::File,
            EntryKind::File,
            EntryKind::Symlink,
            EntryKind::File,
            EntryKind::Gitlink
          ]
        );
        let skip_worktree = index.entries.iter().map(|entry| entry.skip_worktree).collect::<Vec<_>>();
        assert_eq!(skip_worktree, [false, false, false, false, true, false]);
        assert!(index.fsmonitor.is_none());
        assert!(index.untracked_cache.is_none());
      }
    }
  }

  #[test]
  fn checks_the_entry_order_like_git() {
    let conflict = |stage| TestEntry { stage, ..TestEntry::file("c") };
    let index = parse_index(&write_index(2, 20, &[TestEntry::file("a"), conflict(1), conflict(2), conflict(3)], &[]), 20).unwrap();
    let stages = index.entries.iter().map(|entry| entry.stage).collect::<Vec<_>>();
    assert_eq!(stages, [0, 1, 2, 3]);
    for (entries, message) in [
      (vec![TestEntry::file("b"), TestEntry::file("a")], "unordered stage entries in index"),
      (vec![TestEntry::file("c"), conflict(1)], "multiple stage entries for merged file 'c'"),
      (vec![conflict(2), conflict(1)], "unordered stage entries for 'c'"),
    ] {
      let err = parse_index(&write_index(2, 20, &entries, &[]), 20).unwrap_err();
      assert_eq!(err.to_string(), message);
    }
  }

  #[test]
  fn reads_long_paths() {
    let path = format!("{}file", "d/".repeat(3000));
    for version in [2, 4] {
      let index = parse_index(&write_index(version, 20, &[TestEntry::file(&path)], &[]), 20).unwrap();
      assert_eq!(index.entries[0].path, path.as_bytes());
    }
  }

  #[test]
  fn reads_the_fsmonitor_token() {
    let extensions = [(b"FSMN", fsmonitor_extension("builtin:abc:7", &[1, 3]))];
    let index = parse_index(
      &write_index(
        2,
        20,
        &[TestEntry::file("a"), TestEntry::file("b"), TestEntry::file("c"), TestEntry::file("d")],
        &extensions,
      ),
      20,
    )
    .unwrap();
    let fsmonitor = index.fsmonitor.unwrap();
    assert_eq!(fsmonitor.token, b"builtin:abc:7");
    assert_eq!(fsmonitor.dirty_entries, [1, 3]);
  }

  #[test]
  fn ignores_a_v1_fsmonitor_timestamp() {
    let ewah = crate::ewah::write_ewah(&[]);
    let mut data = 1u32.to_be_bytes().to_vec();
    data.extend(123u64.to_be_bytes());
    data.extend((ewah.len() as u32).to_be_bytes());
    data.extend(ewah);
    let index = parse_index(&write_index(2, 20, &[], &[(b"FSMN", data)]), 20).unwrap();
    assert!(index.fsmonitor.is_none());
  }

  #[test]
  fn skips_unknown_optional_extensions_and_rejects_required_ones() {
    let index = parse_index(&write_index(2, 20, &[], &[(b"TREE", vec![1, 2, 3]), (b"EOIE", vec![0; 24])]), 20).unwrap();
    assert!(index.entries.is_empty());
    for signature in [b"link", b"sdir"] {
      let err = parse_index(&write_index(2, 20, &[], &[(signature, Vec::new())]), 20).unwrap_err();
      assert!(err.to_string().contains(std::str::from_utf8(signature).unwrap()), "{err}");
    }
  }

  #[test]
  fn rejects_invalid_indexes() {
    assert!(parse_index(b"DIRC", 20).is_err());
    assert!(parse_index(&write_index(5, 20, &[], &[]), 20).is_err());
    let mut bytes = write_index(2, 20, &[TestEntry::file("a")], &[]);
    bytes[0] = b'X';
    assert!(parse_index(&bytes, 20).is_err());
    let truncated = write_index(2, 20, &[TestEntry::file("a")], &[]);
    assert!(parse_index(&truncated[..truncated.len() - 30], 20).is_err());
  }
}
