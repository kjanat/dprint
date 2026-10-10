use crate::hash::blob_oid;

pub fn exclude_file_oid(bytes: Option<&[u8]>, hash_len: usize) -> Vec<u8> {
  let Some(bytes) = bytes else {
    return vec![0; hash_len];
  };
  // Git hashes a non-empty exclude file as a blob with a newline appended.
  let newline: &[u8] = if bytes.is_empty() { b"" } else { b"\n" };
  blob_oid(&[bytes, newline], hash_len).unwrap_or_default()
}

#[cfg(test)]
mod test {
  use super::*;
  use crate::index_file::parse_index;
  use crate::test_git::TempRepo;

  fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
  }

  const CASES: &[(&str, Option<&[u8]>, &str, &str)] = &[
    (
      "missing",
      None,
      "0000000000000000000000000000000000000000",
      "0000000000000000000000000000000000000000000000000000000000000000",
    ),
    (
      "empty",
      Some(b""),
      "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
      "473a0f4c3be8a93681a267e3b1e9a7dcda1185436fe141f7749120a303721813",
    ),
    (
      "line without newline",
      Some(b"foo"),
      "257cc5642cb1a054f08cc83f2d943e56fd3ebe99",
      "47d6aca82756ff2e61e53520bfdf1faa6c86d933be4854eb34840c57d12e0c85",
    ),
    (
      "line with newline",
      Some(b"foo\n"),
      "75d7bfb873a6171cee61de87d81e0f21df1f8d65",
      "e39519a967a890bc8e0dd64e2ddf0fccbfb3e3fe929de90a67eaa52c5d21099c",
    ),
    (
      "several lines",
      Some(b"# comment\n*.log\n!keep.log\nbuild/\n"),
      "6d68e718af389f610e009ce35cbc0d225a0464be",
      "4b1086dbdf3fbd74c18f1864a1b6889ce2c8ca7fc50075b143f214bca911c801",
    ),
    (
      "crlf lines",
      Some(b"foo\r\nbar\r\n"),
      "453446278e4a2298e80d79e035546d0336de1598",
      "19c6cd7c4544510457d56d6ee1c96f0b2d461f355917751a7a4d08f9d104a2da",
    ),
    (
      "only a newline",
      Some(b"\n"),
      "139597f9cb07c5d48bed18984ec4747f4b4f3438",
      "581f170cd27d4c1d6911175fb6ae926495f534b3a532f51ed13eab9d36e91a14",
    ),
    (
      "spaces",
      Some(b"  "),
      "1a4baf536d705b9c814847cb7a708a0e63d5b976",
      "12c96b018df978f45b09fb53bf633c7c0eb33e0c75cccf1583b2be957282ffe3",
    ),
    (
      "nul byte",
      Some(b"a\0b"),
      "1a23e4be731d2f539deeea324686d000ccdfbfcd",
      "0a8c8e4bb4f39e0f9acced70a1118127afbd4258918950cc3e9a68719f1005ab",
    ),
  ];

  #[test]
  fn matches_the_oids_git_recorded() {
    for (name, bytes, sha1, sha256) in CASES {
      assert_eq!(hex(&exclude_file_oid(*bytes, 20)), *sha1, "{name}");
      assert_eq!(hex(&exclude_file_oid(*bytes, 32)), *sha256, "{name}");
    }
  }

  #[test]
  fn hashes_blobs_like_git() {
    assert_eq!(hex(&blob_oid(&[b"foo\n"], 20).unwrap()), "257cc5642cb1a054f08cc83f2d943e56fd3ebe99");
    assert_eq!(hex(&blob_oid(&[b"fo", b"o\n"], 20).unwrap()), "257cc5642cb1a054f08cc83f2d943e56fd3ebe99");
    assert_eq!(blob_oid(&[b""], 21), None);
    assert!(exclude_file_oid(Some(b"x"), 21).is_empty());
  }

  fn recorded_oids(repo: &TempRepo, hash_len: usize) -> (Vec<u8>, Vec<u8>) {
    repo.git(&["status", "--porcelain"]);
    let index = parse_index(&repo.read(".git/index"), hash_len).unwrap();
    let cache = index.untracked_cache.unwrap();
    (cache.info_exclude_oid, cache.excludes_file_oid)
  }

  #[test]
  fn matches_git_for_info_exclude_and_core_excludes_file() {
    for (format, hash_len) in [("sha1", 20), ("sha256", 32)] {
      for (name, bytes, _, _) in CASES {
        let Some(repo) = TempRepo::new(&[&format!("--object-format={format}")]) else {
          return;
        };
        repo.git(&["config", "core.untrackedCache", "true"]);
        repo.write("tracked", b"x\n");
        repo.git(&["add", "tracked"]);
        let global = repo.path("../global-ignore");
        repo.git(&["config", "core.excludesFile", global.to_str().unwrap()]);
        match bytes {
          Some(bytes) => {
            repo.write(".git/info/exclude", bytes);
            std::fs::write(&global, bytes).unwrap();
          }
          None => std::fs::remove_file(repo.path(".git/info/exclude")).unwrap(),
        }
        let (info_exclude, excludes_file) = recorded_oids(&repo, hash_len);
        assert_eq!(info_exclude, exclude_file_oid(*bytes, hash_len), "{format} {name}");
        assert_eq!(excludes_file, exclude_file_oid(*bytes, hash_len), "{format} {name}");
      }
    }
  }
}
