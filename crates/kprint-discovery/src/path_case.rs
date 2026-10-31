use std::cmp::Ordering;

/// How a repository compares paths. Git compares them ignoring case with
/// `core.ignoreCase` (its `fspathcmp`), which it sets for a case insensitive
/// file system (ex. macOS' and Windows' by default). Like git, only ASCII
/// letters are folded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PathCase {
  #[default]
  Exact,
  Folded,
}

impl PathCase {
  pub fn from_ignore_case(ignore_case: bool) -> Self {
    if ignore_case { PathCase::Folded } else { PathCase::Exact }
  }

  /// Compares two paths (or parts of them) the way the repository does.
  pub fn cmp(self, a: &[u8], b: &[u8]) -> Ordering {
    match self {
      PathCase::Exact => a.cmp(b),
      PathCase::Folded => a.iter().map(u8::to_ascii_lowercase).cmp(b.iter().map(u8::to_ascii_lowercase)),
    }
  }

  pub fn eq(self, a: &[u8], b: &[u8]) -> bool {
    match self {
      PathCase::Exact => a == b,
      PathCase::Folded => a.eq_ignore_ascii_case(b),
    }
  }

  /// The order to keep a repository's paths in so that paths that are the
  /// same to it are next to each other: by the repository's comparison, then
  /// exactly.
  pub fn sort_cmp(self, a: &[u8], b: &[u8]) -> Ordering {
    self.cmp(a, b).then_with(|| a.cmp(b))
  }
}

#[cfg(test)]
mod test {
  use super::*;

  #[test]
  fn compares_like_git() {
    assert_eq!(PathCase::Exact.cmp(b"A", b"a"), Ordering::Less);
    assert!(!PathCase::Exact.eq(b"Foo.ts", b"foo.ts"));
    assert_eq!(PathCase::Folded.cmp(b"Foo.ts", b"foo.ts"), Ordering::Equal);
    assert!(PathCase::Folded.eq(b"SRC/Foo.TS", b"src/foo.ts"));
    // folded letters sort among the lowercase ones (`_` is between `Z` and `a`)
    assert_eq!(PathCase::Folded.cmp(b"Z", b"_"), Ordering::Greater);
    assert_eq!(PathCase::Exact.cmp(b"Z", b"_"), Ordering::Less);
    // only ASCII is folded, like git
    assert!(!PathCase::Folded.eq("É".as_bytes(), "é".as_bytes()));
    assert_eq!(PathCase::Folded.sort_cmp(b"Foo", b"foo"), Ordering::Less);
  }
}
