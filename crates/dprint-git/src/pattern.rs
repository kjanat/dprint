//! Git's gitignore pattern matching from [`dir.c`].
//!
//! [`dir.c`]: https://github.com/git/git/blob/6de20f6092dcf9bdb1c8efe03db4b70c82b423dd/dir.c

use crate::wildmatch::WM_CASEFOLD;
use crate::wildmatch::WM_PATHNAME;
use crate::wildmatch::wildmatch;

/// The outcome of git's `last_matching_pattern_from_list`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternMatch {
  None,
  Positive,
  Negative,
}

#[derive(Debug, Clone)]
struct Pattern {
  text: Vec<u8>,
  no_wildcard_len: usize,
  negative: bool,
  must_be_dir: bool,
  no_dir: bool,
  ends_with: bool,
}

/// Gitignore patterns for the paths below one directory. The last matching
/// pattern decides.
#[derive(Debug, Clone, Default)]
pub struct PatternList {
  patterns: Vec<Pattern>,
  ignore_case: bool,
  negative_count: usize,
}

impl PatternList {
  pub fn new(ignore_case: bool) -> Self {
    Self {
      ignore_case,
      ..Default::default()
    }
  }

  pub fn is_empty(&self) -> bool {
    self.patterns.is_empty()
  }

  pub fn len(&self) -> usize {
    self.patterns.len()
  }

  pub fn negative_count(&self) -> usize {
    self.negative_count
  }

  /// Adds the lines of a gitignore file (`add_patterns_from_buffer`).
  pub fn add_buffer(&mut self, buffer: &[u8]) {
    let buffer = buffer.strip_prefix(b"\xef\xbb\xbf").unwrap_or(buffer);
    for line in buffer.split(|byte| *byte == b'\n') {
      self.add_line(line);
    }
  }

  /// Adds one gitignore line. Blank lines and comments add nothing.
  pub fn add_line(&mut self, line: &[u8]) {
    if line.is_empty() || line[0] == b'#' {
      return;
    }
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let line = trim_trailing_spaces(line);
    if !line.is_empty() {
      self.add_pattern(line);
    }
  }

  /// `parse_path_pattern` and `add_pattern`.
  fn add_pattern(&mut self, line: &[u8]) {
    let (negative, line) = match line.strip_prefix(b"!") {
      Some(rest) => (true, rest),
      None => (false, line),
    };
    let (must_be_dir, mut text) = match line.strip_suffix(b"/") {
      Some(rest) => (true, rest),
      None => (false, line),
    };
    // `**/name` and `name` match the same paths (`Documentation/gitignore.adoc`).
    // Without the prefix, git's basename fast paths apply.
    if let Some(rest) = text.strip_prefix(b"**/")
      && !rest.is_empty()
      && !rest.contains(&b'/')
    {
      text = rest;
    }
    let no_dir = !text.contains(&b'/');
    let no_wildcard_len = simple_length(text);
    let ends_with = text.first() == Some(&b'*') && simple_length(&text[1..]) == text.len() - 1;
    if negative {
      self.negative_count += 1;
    }
    self.patterns.push(Pattern {
      text: text.to_vec(),
      no_wildcard_len,
      negative,
      must_be_dir,
      no_dir,
      ends_with,
    });
  }

  /// Matches a `/` separated path relative to the list's directory.
  pub fn matched(&self, path: &[u8], is_dir: bool) -> PatternMatch {
    let basename = match path.iter().rposition(|byte| *byte == b'/') {
      Some(index) => &path[index + 1..],
      None => path,
    };
    for pattern in self.patterns.iter().rev() {
      if pattern.must_be_dir && !is_dir {
        continue;
      }
      let is_match = if pattern.no_dir {
        self.match_basename(basename, pattern)
      } else {
        self.match_pathname(path, pattern)
      };
      if is_match {
        return if pattern.negative { PatternMatch::Negative } else { PatternMatch::Positive };
      }
    }
    PatternMatch::None
  }

  fn flags(&self) -> u32 {
    if self.ignore_case { WM_CASEFOLD } else { 0 }
  }

  fn eq(&self, a: &[u8], b: &[u8]) -> bool {
    if self.ignore_case { a.eq_ignore_ascii_case(b) } else { a == b }
  }

  fn match_basename(&self, basename: &[u8], pattern: &Pattern) -> bool {
    let text = pattern.text.as_slice();
    if pattern.no_wildcard_len == text.len() {
      self.eq(text, basename)
    } else if pattern.ends_with {
      let suffix = &text[1..];
      suffix.len() <= basename.len() && self.eq(suffix, &basename[basename.len() - suffix.len()..])
    } else {
      wildmatch(text, basename, self.flags())
    }
  }

  fn match_pathname(&self, path: &[u8], pattern: &Pattern) -> bool {
    let mut text = pattern.text.as_slice();
    let mut prefix = pattern.no_wildcard_len;
    if let Some(rest) = text.strip_prefix(b"/") {
      text = rest;
      prefix -= 1;
    }
    if path.is_empty() {
      return false;
    }
    let mut name = path;
    if prefix > 0 {
      if prefix > name.len() || !self.eq(&text[..prefix], &name[..prefix]) {
        return false;
      }
      if text.len() == prefix && name.len() == prefix {
        return true;
      }
      // keep one character so wildmatch sees where a component starts
      text = &text[prefix - 1..];
      name = &name[prefix - 1..];
    }
    wildmatch(text, name, WM_PATHNAME | self.flags())
  }
}

/// The length of the start of a pattern without glob characters.
fn simple_length(text: &[u8]) -> usize {
  text.iter().position(|byte| matches!(byte, b'*' | b'?' | b'[' | b'\\')).unwrap_or(text.len())
}

/// Git's `trim_trailing_spaces`.
fn trim_trailing_spaces(line: &[u8]) -> &[u8] {
  let mut last_space = None;
  let mut index = 0;
  while index < line.len() {
    match line[index] {
      b' ' => {
        if last_space.is_none() {
          last_space = Some(index);
        }
      }
      b'\\' => {
        index += 1;
        if index == line.len() {
          return line;
        }
        last_space = None;
      }
      _ => last_space = None,
    }
    index += 1;
  }
  match last_space {
    Some(end) => &line[..end],
    None => line,
  }
}

#[cfg(test)]
mod test {
  use super::*;

  fn list(text: &str) -> PatternList {
    let mut list = PatternList::new(false);
    list.add_buffer(text.as_bytes());
    list
  }

  fn check(list: &PatternList, path: &str, is_dir: bool) -> PatternMatch {
    list.matched(path.as_bytes(), is_dir)
  }

  #[test]
  fn matches_basenames_at_any_depth() {
    let list = list("*.log\nbuild\n");
    assert_eq!(check(&list, "a.log", false), PatternMatch::Positive);
    assert_eq!(check(&list, "x/y/a.log", false), PatternMatch::Positive);
    assert_eq!(check(&list, "x/build", true), PatternMatch::Positive);
    assert_eq!(check(&list, "x/build.rs", false), PatternMatch::None);
  }

  #[test]
  fn anchors_patterns_with_a_slash() {
    let list = list("/root.txt\ndoc/*.md\n");
    assert_eq!(check(&list, "root.txt", false), PatternMatch::Positive);
    assert_eq!(check(&list, "sub/root.txt", false), PatternMatch::None);
    assert_eq!(check(&list, "doc/a.md", false), PatternMatch::Positive);
    assert_eq!(check(&list, "doc/sub/a.md", false), PatternMatch::None);
    assert_eq!(check(&list, "x/doc/a.md", false), PatternMatch::None);
  }

  #[test]
  fn matches_directories_only_with_a_trailing_slash() {
    let list = list("out/\n");
    assert_eq!(check(&list, "out", true), PatternMatch::Positive);
    assert_eq!(check(&list, "out", false), PatternMatch::None);
    assert_eq!(check(&list, "a/out", true), PatternMatch::Positive);
  }

  #[test]
  fn lets_the_last_matching_pattern_decide() {
    let list = list("*.txt\n!keep.txt\n");
    assert_eq!(check(&list, "a.txt", false), PatternMatch::Positive);
    assert_eq!(check(&list, "keep.txt", false), PatternMatch::Negative);
    assert_eq!(list.negative_count(), 1);
    let list = self::list("!keep.txt\n*.txt\n");
    assert_eq!(check(&list, "keep.txt", false), PatternMatch::Positive);
  }

  #[test]
  fn handles_double_asterisks() {
    let list = list("**/logs\nfoo/**/bar\nabc/**\n");
    assert_eq!(check(&list, "logs", true), PatternMatch::Positive);
    assert_eq!(check(&list, "a/b/logs", true), PatternMatch::Positive);
    assert_eq!(check(&list, "foo/bar", false), PatternMatch::Positive);
    assert_eq!(check(&list, "foo/a/b/bar", false), PatternMatch::Positive);
    assert_eq!(check(&list, "abc/x/y", false), PatternMatch::Positive);
    assert_eq!(check(&list, "abc", true), PatternMatch::None);
  }

  #[test]
  fn skips_comments_and_blank_lines_and_trims_unescaped_spaces() {
    let list = list("\u{feff}# comment\n\n\\#hash\nspace  \nkept\\ \r\n");
    assert_eq!(list.len(), 3);
    assert_eq!(check(&list, "#hash", false), PatternMatch::Positive);
    assert_eq!(check(&list, "space", false), PatternMatch::Positive);
    assert_eq!(check(&list, "kept ", false), PatternMatch::Positive);
    assert_eq!(check(&list, "kept", false), PatternMatch::None);
  }

  #[test]
  fn treats_braces_literally() {
    let list = list("*.{js,ts}\n");
    assert_eq!(check(&list, "a.js", false), PatternMatch::None);
    assert_eq!(check(&list, "a.{js,ts}", false), PatternMatch::Positive);
  }

  #[test]
  fn ignores_case_when_asked() {
    let mut list = PatternList::new(true);
    list.add_buffer(b"/Readme.MD\n*.TXT\nDocs/*.rs\n");
    assert_eq!(check(&list, "README.md", false), PatternMatch::Positive);
    assert_eq!(check(&list, "a.txt", false), PatternMatch::Positive);
    assert_eq!(check(&list, "docs/a.RS", false), PatternMatch::Positive);
  }

  #[test]
  fn trims_trailing_spaces_like_git() {
    assert_eq!(trim_trailing_spaces(b"a  "), b"a");
    assert_eq!(trim_trailing_spaces(b"a\\  "), b"a\\ ");
    assert_eq!(trim_trailing_spaces(b"a \\"), b"a \\");
    assert_eq!(trim_trailing_spaces(b"   "), b"");
  }

  #[test]
  fn matches_classes_and_negations() {
    let list = list("a?c\n[ab]x\n!bx\nd/**/e\n/f/g\n");
    assert_eq!(check(&list, "abc", false), PatternMatch::Positive);
    assert_eq!(check(&list, "a/c", false), PatternMatch::None);
    assert_eq!(check(&list, "ax", false), PatternMatch::Positive);
    assert_eq!(check(&list, "bx", false), PatternMatch::Negative);
    assert_eq!(check(&list, "d/e", false), PatternMatch::Positive);
    assert_eq!(check(&list, "d/x/y/e", false), PatternMatch::Positive);
    assert_eq!(check(&list, "f/g", false), PatternMatch::Positive);
    assert_eq!(check(&list, "x/f/g", false), PatternMatch::None);
  }
}
