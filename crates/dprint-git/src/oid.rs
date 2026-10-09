use sha1::Digest;

/// Git's blob id for `bytes`, or the null id for `None`.
pub fn blob_oid(bytes: Option<&[u8]>, hash_len: usize) -> Vec<u8> {
  let Some(bytes) = bytes else {
    return vec![0; hash_len];
  };
  let header = format!("blob {}\0", bytes.len());
  if hash_len == 32 {
    let mut hasher = sha2::Sha256::new();
    hasher.update(header.as_bytes());
    hasher.update(bytes);
    hasher.finalize().to_vec()
  } else {
    let mut hasher = sha1::Sha1::new();
    hasher.update(header.as_bytes());
    hasher.update(bytes);
    hasher.finalize().to_vec()
  }
}

/// The untracked cache's id for `.git/info/exclude` and the global excludes
/// file. Git's `add_patterns` appends a newline before it hashes a non-empty
/// file.
pub fn exclude_file_oid(bytes: Option<&[u8]>, hash_len: usize) -> Vec<u8> {
  match bytes {
    None => vec![0; hash_len],
    Some([]) => blob_oid(Some(b""), hash_len),
    Some(bytes) => {
      let mut contents = Vec::with_capacity(bytes.len() + 1);
      contents.extend_from_slice(bytes);
      contents.push(b'\n');
      blob_oid(Some(&contents), hash_len)
    }
  }
}

#[cfg(test)]
mod test {
  use super::*;

  fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
  }

  #[test]
  fn hashes_like_git_hash_object() {
    // `git hash-object --stdin` in a SHA-1 and in a SHA-256 repository
    assert_eq!(hex(&blob_oid(Some(b""), 20)), "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
    assert_eq!(hex(&blob_oid(Some(b"hello\n"), 20)), "ce013625030ba8dba906f756967f9e9ca394464a");
    assert_eq!(
      hex(&blob_oid(Some(b"hello\n"), 32)),
      "2cf8d83d9ee29543b34a87727421fdecb7e3f3a183d337639025de576db9ebb4"
    );
    assert_eq!(blob_oid(None, 32), vec![0; 32]);
  }

  #[test]
  fn hashes_exclude_files_with_an_extra_newline() {
    assert_eq!(exclude_file_oid(Some(b"*.log\n"), 20), blob_oid(Some(b"*.log\n\n"), 20));
    assert_eq!(exclude_file_oid(Some(b""), 20), blob_oid(Some(b""), 20));
    assert_eq!(exclude_file_oid(None, 20), vec![0; 20]);
  }
}
