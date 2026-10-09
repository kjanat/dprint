use std::borrow::Cow;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Result;
use anyhow::bail;
use dprint_git::PatternList;
use dprint_git::PatternMatch;

pub fn is_negated_glob(pattern: &str) -> bool {
  let mut chars = pattern.chars();
  let first_char = chars.next();
  let second_char = chars.next();

  first_char == Some('!') && second_char != Some('(')
}

pub fn non_negated_glob(pattern: &str) -> &str {
  if is_negated_glob(pattern) { &pattern[1..] } else { pattern }
}

pub fn is_pattern(pattern: &str) -> bool {
  if pattern.starts_with('!') {
    return true;
  }

  let mut was_last_escape = false;
  for c in pattern.chars() {
    if !was_last_escape && matches!(c, '*' | '{' | '?' | '[') {
      return true;
    }

    // consume backslashes in pairs like `unescape_glob_text`, so an escaped
    // backslash doesn't escape the next character (ex. the `*` in `a\\*b` is a
    // glob star)
    was_last_escape = c == '\\' && !was_last_escape;
  }
  false
}

/// Whether a single path component pattern (ex. `dist`, `su*`, `[sd]ist`)
/// names the given directory.
///
/// The exclude matcher's gitignore engine does the matching, so a wildcard means
/// the same here as in the final match.
pub fn pattern_names_dir(pattern: &str, dir_name: &str) -> bool {
  if !is_pattern(pattern) {
    return unescape_glob_text(pattern) == dir_name;
  }
  // a component pattern can't match a name containing a separator
  if dir_name.contains('/') || dir_name.contains('\\') {
    return false;
  }
  let mut patterns = PatternList::new(/* ignore case */ false);
  if add_glob_line(&mut patterns, pattern).is_err() {
    return false;
  }
  patterns.matched(dir_name.as_bytes(), /* is dir */ true) == PatternMatch::Positive
}

/// A path's bytes with `/` separators, like git's paths.
pub fn path_to_slash_bytes(path: &Path) -> Cow<'_, [u8]> {
  #[cfg(unix)]
  {
    Cow::Borrowed(std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str()))
  }
  #[cfg(not(unix))]
  {
    Cow::Owned(path.to_string_lossy().replace('\\', "/").into_bytes())
  }
}

/// A directory to strip from paths, like `Path::strip_prefix`. Paths with
/// plain separators skip parsing components.
#[derive(Debug, Clone)]
pub(crate) struct DirPrefix {
  dir: PathBuf,
  is_plain: bool,
}

impl DirPrefix {
  pub fn new(dir: PathBuf) -> Self {
    #[cfg(unix)]
    let is_plain = has_plain_separators(std::os::unix::ffi::OsStrExt::as_bytes(dir.as_os_str()), /* is relative */ false);
    #[cfg(not(unix))]
    let is_plain = false;
    Self { dir, is_plain }
  }

  pub fn strip<'a>(&self, path: &'a Path) -> Option<&'a Path> {
    #[cfg(unix)]
    if self.is_plain {
      use std::os::unix::ffi::OsStrExt;
      let path_bytes = path.as_os_str().as_bytes();
      let dir_bytes = self.dir.as_os_str().as_bytes();
      match path_bytes.strip_prefix(dir_bytes) {
        Some([]) => return Some(Path::new("")),
        Some(rest) => {
          let rest = match rest {
            _ if dir_bytes == b"/" => rest,
            [b'/', rest @ ..] => rest,
            // the path's component only starts like the directory's last one
            _ => return None,
          };
          if has_plain_separators(rest, /* is relative */ true) {
            return Some(Path::new(std::ffi::OsStr::from_bytes(rest)));
          }
        }
        None if has_plain_separators(path_bytes, /* is relative */ false) => return None,
        None => {}
      }
    }
    path.strip_prefix(&self.dir).ok()
  }
}

/// No empty or `.` components and no trailing separator.
#[cfg(unix)]
fn has_plain_separators(bytes: &[u8], is_relative: bool) -> bool {
  if bytes == b"/" {
    return !is_relative;
  }
  let mut components = bytes.split(|byte| *byte == b'/');
  let first = components.next().unwrap_or_default();
  let first_is_plain = if first.is_empty() { !is_relative && bytes.len() > 1 } else { first != b"." };
  first_is_plain && components.all(|component| !component.is_empty() && component != b".")
}

/// The most patterns one glob may expand to (ex. `{a,b}/{c,d}` expands to 4).
const MAX_BRACE_EXPANSIONS: usize = 4096;

/// Adds a dprint glob to a list of gitignore patterns. Unlike gitignore
/// files, dprint globs support alternatives (ex. `**/*.{ts,tsx}`).
pub fn add_glob_line(patterns: &mut PatternList, glob: &str) -> Result<()> {
  for pattern in expand_braces(glob)? {
    patterns.add_line(pattern.as_bytes());
  }
  Ok(())
}

/// Expands the alternative groups of a glob (ex. `*.{ts,js}` to `*.ts` and
/// `*.js`). Escaped braces and braces in character classes stay as they are.
pub fn expand_braces(glob: &str) -> Result<Vec<String>> {
  let mut expanded = Vec::new();
  let mut pending = vec![glob.to_string()];
  while let Some(pattern) = pending.pop() {
    let Some(open) = find_brace_group(&pattern) else {
      expanded.push(pattern);
      if expanded.len() > MAX_BRACE_EXPANSIONS {
        bail!("The glob {} expands to more than {} patterns.", glob, MAX_BRACE_EXPANSIONS);
      }
      continue;
    };
    let Some((alternatives, close)) = split_brace_group(&pattern, open) else {
      bail!("The glob {} has an unclosed alternate group.", glob);
    };
    if expanded.len() + pending.len() + alternatives.len() > MAX_BRACE_EXPANSIONS {
      bail!("The glob {} expands to more than {} patterns.", glob, MAX_BRACE_EXPANSIONS);
    }
    for alternative in alternatives.into_iter().rev() {
      pending.push(format!("{}{}{}", &pattern[..open], alternative, &pattern[close + 1..]));
    }
  }
  Ok(expanded)
}

/// The byte index of the first alternate group's `{`.
fn find_brace_group(pattern: &str) -> Option<usize> {
  let bytes = pattern.as_bytes();
  let mut index = 0;
  while index < bytes.len() {
    match bytes[index] {
      b'\\' => index += 1,
      b'[' => index = class_end(bytes, index).unwrap_or(index),
      b'{' => return Some(index),
      _ => {}
    }
    index += 1;
  }
  None
}

/// The alternatives of the group at `open`, and the index of its `}`.
fn split_brace_group(pattern: &str, open: usize) -> Option<(Vec<&str>, usize)> {
  let bytes = pattern.as_bytes();
  let mut alternatives = Vec::new();
  let mut depth = 0;
  let mut start = open + 1;
  let mut index = open + 1;
  while index < bytes.len() {
    match bytes[index] {
      b'\\' => index += 1,
      b'[' => index = class_end(bytes, index).unwrap_or(index),
      b'{' => depth += 1,
      b'}' if depth > 0 => depth -= 1,
      b'}' => {
        alternatives.push(&pattern[start..index]);
        return Some((alternatives, index));
      }
      b',' if depth == 0 => {
        alternatives.push(&pattern[start..index]);
        start = index + 1;
      }
      _ => {}
    }
    index += 1;
  }
  None
}

/// The index of the `]` of the character class at `open`, by wildmatch's rules.
fn class_end(bytes: &[u8], open: usize) -> Option<usize> {
  let mut index = open + 1;
  if matches!(bytes.get(index), Some(b'!' | b'^')) {
    index += 1;
  }
  if bytes.get(index) == Some(&b']') {
    index += 1;
  }
  while index < bytes.len() {
    match bytes[index] {
      b'\\' => index += 1,
      b']' => return Some(index),
      _ => {}
    }
    index += 1;
  }
  None
}

/// Escapes glob metacharacters so the text matches literally
/// (ex. `routes/[id].svelte` -> `routes/\[id\].svelte`).
pub fn escape_glob_text(text: &str) -> String {
  let mut result = String::with_capacity(text.len());
  for c in text.chars() {
    if matches!(c, '\\' | '*' | '{' | '}' | '?' | '[' | ']' | '!') {
      result.push('\\');
    }
    result.push(c);
  }
  result
}

/// Escapes glob metacharacters using character classes (ex. `[` -> `[[]`) so
/// the result contains no backslashes and survives CLI pattern processing,
/// which converts backslashes to forward slashes.
pub fn escape_glob_text_for_cli(text: &str) -> String {
  let mut result = String::with_capacity(text.len());
  for c in text.chars() {
    match c {
      '[' | ']' | '{' | '}' | '*' | '?' => {
        result.push('[');
        result.push(c);
        result.push(']');
      }
      _ => result.push(c),
    }
  }
  result
}

/// Removes glob escapes (ex. `routes/\[id\].svelte` -> `routes/[id].svelte`).
pub fn unescape_glob_text(text: &str) -> Cow<'_, str> {
  if !text.contains('\\') {
    return Cow::Borrowed(text);
  }
  let mut result = String::with_capacity(text.len());
  let mut chars = text.chars();
  while let Some(c) = chars.next() {
    if c == '\\' {
      match chars.next() {
        Some(next) => result.push(next),
        None => result.push(c),
      }
    } else {
      result.push(c);
    }
  }
  Cow::Owned(result)
}

pub fn is_absolute_pattern(pattern: &str) -> bool {
  let pattern = if is_negated_glob(pattern) { &pattern[1..] } else { pattern };
  pattern.starts_with('/') || is_windows_absolute_pattern(pattern)
}

fn is_windows_absolute_pattern(pattern: &str) -> bool {
  // ex. D:/
  let mut chars = pattern.chars();

  // ensure the first character is alphabetic
  let next_char = chars.next();
  if next_char.is_none() || !next_char.unwrap().is_ascii_alphabetic() {
    return false;
  }

  // skip over the remaining alphabetic characters
  let mut next_char = chars.next();
  while next_char.is_some() && next_char.unwrap().is_ascii_alphabetic() {
    next_char = chars.next();
  }

  // ensure colon
  if next_char != Some(':') {
    return false;
  }

  // now check for the last slash
  let next_char = chars.next();
  matches!(next_char, Some('/'))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn strips_dir_prefixes_like_path() {
    let paths = [
      "", "/", ".", "./a", "a", "a/b", "/a", "/a/", "/a/b", "/a/b/", "/a/b/.", "/a/bc", "/a//b", "/a/./b", "/a/b/c/d", "/a/../b", "/ab", "//a", "/a/b//c",
      "/a/b/./c", "a/./b", "a//b", "/./a",
    ];
    for path in paths {
      for dir in paths {
        assert_eq!(
          DirPrefix::new(PathBuf::from(dir)).strip(Path::new(path)),
          Path::new(path).strip_prefix(dir).ok(),
          "{path:?} without {dir:?}"
        );
      }
    }
  }

  #[test]
  fn should_escape_and_unescape_glob_text() {
    assert_eq!(escape_glob_text("routes/[id].svelte"), "routes/\\[id\\].svelte");
    assert_eq!(escape_glob_text("{{myfile}}.yaml"), "\\{\\{myfile\\}\\}.yaml");
    assert_eq!(escape_glob_text("a*b?c!d\\e"), "a\\*b\\?c\\!d\\\\e");
    assert_eq!(escape_glob_text("plain/file.txt"), "plain/file.txt");

    assert_eq!(escape_glob_text_for_cli("/[app]/dir"), "/[[]app[]]/dir");
    assert_eq!(escape_glob_text_for_cli("/plain/dir"), "/plain/dir");

    assert_eq!(unescape_glob_text("routes/\\[id\\].svelte"), "routes/[id].svelte");
    assert_eq!(unescape_glob_text("plain/file.txt"), "plain/file.txt");
    assert_eq!(unescape_glob_text("a\\\\b"), "a\\b");
    // a trailing lone backslash stays as-is
    assert_eq!(unescape_glob_text("a\\"), "a\\");

    // escaped text is not considered a pattern and round trips
    assert!(is_pattern("routes/[id].svelte"));
    assert!(!is_pattern(&escape_glob_text("routes/[id].svelte")));
    assert_eq!(unescape_glob_text(&escape_glob_text("a*b?c!d\\e[]{}")), "a*b?c!d\\e[]{}");

    // an escaped backslash doesn't escape the character after it, so the
    // star in `a\\*b` is a glob star (matching `unescape_glob_text`)
    assert!(is_pattern("a\\\\*b"));
    assert_eq!(unescape_glob_text("a\\\\*b"), "a\\*b");
    // ...while `a\*b` is an escaped star and so not a pattern
    assert!(!is_pattern("a\\*b"));
    assert_eq!(unescape_glob_text("a\\*b"), "a*b");
  }

  #[test]
  fn should_get_if_pattern_names_dir() {
    assert!(pattern_names_dir("dist", "dist"));
    assert!(!pattern_names_dir("dist", "other"));
    // wildcards mean the same here as in the final match
    assert!(pattern_names_dir("su*", "sub"));
    assert!(!pattern_names_dir("su*", "other"));
    assert!(pattern_names_dir("?ub", "sub"));
    assert!(pattern_names_dir("[sd]ist", "dist"));
    assert!(!pattern_names_dir("[sd]ist", "list"));
    assert!(pattern_names_dir("*", "anything"));
    assert!(pattern_names_dir("*.min.js", "a.min.js"));
    // both ways of escaping a glob character match the literal name
    assert!(pattern_names_dir("\\[id\\]", "[id]"));
    assert!(pattern_names_dir("[[]id[]]", "[id]"));
    // a single component pattern never matches across a separator
    assert!(!pattern_names_dir("dist", "dist/sub"));
    assert!(!pattern_names_dir("*", "dist/sub"));
  }

  #[test]
  fn should_expand_braces() {
    assert_eq!(expand_braces("**/*.{ts,tsx}").unwrap(), ["**/*.ts", "**/*.tsx"]);
    assert_eq!(expand_braces("{a,b}/{c,d}").unwrap(), ["a/c", "a/d", "b/c", "b/d"]);
    assert_eq!(expand_braces("x{a,{b,c}}y").unwrap(), ["xay", "xby", "xcy"]);
    assert_eq!(expand_braces("!{a,b}").unwrap(), ["!a", "!b"]);
    assert_eq!(expand_braces("plain").unwrap(), ["plain"]);
    assert_eq!(expand_braces("{,a}").unwrap(), ["", "a"]);
    // escaped braces and braces in character classes are literal
    assert_eq!(expand_braces("\\{a,b\\}").unwrap(), ["\\{a,b\\}"]);
    assert_eq!(expand_braces("[{]x").unwrap(), ["[{]x"]);
    assert_eq!(expand_braces("[]{]{a,b}").unwrap(), ["[]{]a", "[]{]b"]);
    assert!(expand_braces("{a,b").unwrap_err().to_string().contains("unclosed"));
    let too_many = "{a,b}".repeat(13);
    assert!(expand_braces(&too_many).unwrap_err().to_string().contains("more than"));
  }

  #[test]
  fn should_match_globs_with_alternatives() {
    let mut patterns = PatternList::new(false);
    add_glob_line(&mut patterns, "**/*.{ts,js}").unwrap();
    add_glob_line(&mut patterns, "!vendor/*.{js,}").unwrap();
    assert_eq!(patterns.matched(b"a/b.ts", false), PatternMatch::Positive);
    assert_eq!(patterns.matched(b"b.js", false), PatternMatch::Positive);
    assert_eq!(patterns.matched(b"vendor/b.js", false), PatternMatch::Negative);
    assert_eq!(patterns.matched(b"vendor/b.ts", false), PatternMatch::Positive);
    assert_eq!(patterns.matched(b"b.rs", false), PatternMatch::None);
  }

  #[test]
  fn should_get_if_absolute_pattern() {
    assert!(!is_absolute_pattern("test.ts"));
    assert!(!is_absolute_pattern("!test.ts"));
    assert!(is_absolute_pattern("/test.ts"));
    assert!(is_absolute_pattern("!/test.ts"));
    assert!(is_absolute_pattern("D:/test.ts"));
    assert!(is_absolute_pattern("!D:/test.ts"));
  }
}
