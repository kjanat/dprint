//! A port of git's [`wildmatch.c`], with git's locale-independent ASCII classes from [`sane-ctype.h`].
//!
//! [`wildmatch.c`]: https://github.com/git/git/blob/6de20f6092dcf9bdb1c8efe03db4b70c82b423dd/wildmatch.c
//! [`sane-ctype.h`]: https://github.com/git/git/blob/6de20f6092dcf9bdb1c8efe03db4b70c82b423dd/sane-ctype.h

pub const WM_CASEFOLD: u32 = 1;
pub const WM_PATHNAME: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
  Match,
  NoMatch,
  AbortAll,
  AbortToStarStar,
}

/// Matches `text` against `pattern`. A NUL byte ends either, as in C.
pub fn wildmatch(pattern: &[u8], text: &[u8], flags: u32) -> bool {
  dowild(pattern, 0, text, 0, flags) == Outcome::Match
}

fn at(bytes: &[u8], index: usize) -> u8 {
  bytes.get(index).copied().unwrap_or(0)
}

fn fold(byte: u8, flags: u32) -> u8 {
  if flags & WM_CASEFOLD != 0 { byte.to_ascii_lowercase() } else { byte }
}

fn is_glob_special(byte: u8) -> bool {
  matches!(byte, b'*' | b'?' | b'[' | b'\\')
}

fn dowild(pattern: &[u8], mut p: usize, text: &[u8], mut t: usize, flags: u32) -> Outcome {
  let pathname = flags & WM_PATHNAME != 0;
  loop {
    let raw_p_ch = at(pattern, p);
    if raw_p_ch == 0 {
      break;
    }
    let raw_t_ch = at(text, t);
    if raw_t_ch == 0 && raw_p_ch != b'*' {
      return Outcome::AbortAll;
    }
    let mut t_ch = fold(raw_t_ch, flags);
    let p_ch = fold(raw_p_ch, flags);
    match p_ch {
      b'\\' => {
        p += 1;
        if t_ch != at(pattern, p) {
          return Outcome::NoMatch;
        }
      }
      b'?' => {
        if pathname && t_ch == b'/' {
          return Outcome::NoMatch;
        }
      }
      b'*' => {
        p += 1;
        let match_slash;
        if at(pattern, p) == b'*' {
          let prev_p = p;
          while at(pattern, p) == b'*' {
            p += 1;
          }
          if !pathname {
            match_slash = true;
          } else if (prev_p < 2 || at(pattern, prev_p - 2) == b'/')
            && (at(pattern, p) == 0 || at(pattern, p) == b'/' || (at(pattern, p) == b'\\' && at(pattern, p + 1) == b'/'))
          {
            if at(pattern, p) == b'/' && dowild(pattern, p + 1, text, t, flags) == Outcome::Match {
              return Outcome::Match;
            }
            match_slash = true;
          } else {
            match_slash = false;
          }
        } else {
          match_slash = !pathname;
        }
        if at(pattern, p) == 0 {
          if !match_slash && text_rest(text, t).contains(&b'/') {
            return Outcome::AbortToStarStar;
          }
          return Outcome::Match;
        } else if !match_slash && at(pattern, p) == b'/' {
          match text_rest(text, t).iter().position(|byte| *byte == b'/') {
            Some(offset) => t += offset,
            None => return Outcome::AbortAll,
          }
          t += 1;
          p += 1;
          continue;
        }
        loop {
          if t_ch == 0 {
            break;
          }
          let next_p_ch = at(pattern, p);
          if !is_glob_special(next_p_ch) {
            let literal = fold(next_p_ch, flags);
            loop {
              t_ch = at(text, t);
              if t_ch == 0 || (!match_slash && t_ch == b'/') {
                break;
              }
              t_ch = fold(t_ch, flags);
              if t_ch == literal {
                break;
              }
              t += 1;
            }
            if t_ch != literal {
              return if match_slash { Outcome::AbortAll } else { Outcome::AbortToStarStar };
            }
          }
          let matched = dowild(pattern, p, text, t, flags);
          if matched != Outcome::NoMatch {
            if !match_slash || matched != Outcome::AbortToStarStar {
              return matched;
            }
          } else if !match_slash && t_ch == b'/' {
            return Outcome::AbortToStarStar;
          }
          t += 1;
          t_ch = at(text, t);
        }
        return Outcome::AbortAll;
      }
      b'[' => {
        p += 1;
        let mut class_ch = at(pattern, p);
        if class_ch == b'^' {
          class_ch = b'!';
        }
        let negated = class_ch == b'!';
        if negated {
          p += 1;
          class_ch = at(pattern, p);
        }
        let mut prev_ch = 0u8;
        let mut matched = false;
        loop {
          if class_ch == 0 {
            return Outcome::AbortAll;
          }
          if class_ch == b'\\' {
            p += 1;
            class_ch = at(pattern, p);
            if class_ch == 0 {
              return Outcome::AbortAll;
            }
            if t_ch == class_ch {
              matched = true;
            }
          } else if class_ch == b'-' && prev_ch != 0 && at(pattern, p + 1) != 0 && at(pattern, p + 1) != b']' {
            p += 1;
            class_ch = at(pattern, p);
            if class_ch == b'\\' {
              p += 1;
              class_ch = at(pattern, p);
              if class_ch == 0 {
                return Outcome::AbortAll;
              }
            }
            if t_ch <= class_ch && t_ch >= prev_ch {
              matched = true;
            } else if flags & WM_CASEFOLD != 0 && t_ch.is_ascii_lowercase() {
              let upper = t_ch.to_ascii_uppercase();
              if upper <= class_ch && upper >= prev_ch {
                matched = true;
              }
            }
            class_ch = 0;
          } else if class_ch == b'[' && at(pattern, p + 1) == b':' {
            p += 2;
            let start = p;
            while at(pattern, p) != 0 && at(pattern, p) != b']' {
              p += 1;
            }
            if at(pattern, p) == 0 {
              return Outcome::AbortAll;
            }
            if p == start || at(pattern, p - 1) != b':' {
              p = start - 2;
              class_ch = b'[';
              if t_ch == class_ch {
                matched = true;
              }
            } else {
              match matches_class(&pattern[start..p - 1], t_ch, flags) {
                Some(true) => matched = true,
                Some(false) => {}
                None => return Outcome::AbortAll,
              }
              class_ch = 0;
            }
          } else if t_ch == class_ch {
            matched = true;
          }
          prev_ch = class_ch;
          p += 1;
          class_ch = at(pattern, p);
          if class_ch == b']' {
            break;
          }
        }
        if matched == negated || (pathname && t_ch == b'/') {
          return Outcome::NoMatch;
        }
      }
      _ => {
        if t_ch != p_ch {
          return Outcome::NoMatch;
        }
      }
    }
    t += 1;
    p += 1;
  }
  if at(text, t) != 0 { Outcome::NoMatch } else { Outcome::Match }
}

fn text_rest(text: &[u8], t: usize) -> &[u8] {
  let rest = text.get(t..).unwrap_or_default();
  match rest.iter().position(|byte| *byte == 0) {
    Some(end) => &rest[..end],
    None => rest,
  }
}

/// `None` for an unknown class name.
fn matches_class(name: &[u8], byte: u8, flags: u32) -> Option<bool> {
  Some(match name {
    b"alnum" => byte.is_ascii_alphanumeric(),
    b"alpha" => byte.is_ascii_alphabetic(),
    b"blank" => matches!(byte, b' ' | b'\t'),
    b"cntrl" => byte < 0x20 || byte == 0x7f,
    b"digit" => byte.is_ascii_digit(),
    b"graph" => (0x21..=0x7e).contains(&byte),
    b"lower" => byte.is_ascii_lowercase(),
    b"print" => (0x20..=0x7e).contains(&byte),
    b"punct" => byte.is_ascii_punctuation(),
    b"space" => matches!(byte, b' ' | b'\t' | b'\n' | b'\r'),
    b"upper" => byte.is_ascii_uppercase() || (flags & WM_CASEFOLD != 0 && byte.is_ascii_lowercase()),
    b"xdigit" => byte.is_ascii_hexdigit(),
    _ => return None,
  })
}

#[cfg(test)]
mod test {
  use super::*;

  // Cases from git's `t/t3070-wildmatch.sh`, with the results for
  // `WM_PATHNAME`, `WM_PATHNAME | WM_CASEFOLD`, no flags and `WM_CASEFOLD`.
  const VECTORS: &[([bool; 4], &str, &str)] = &[
    ([true, true, true, true], "foo", "foo"),
    ([false, false, false, false], "foo", "bar"),
    ([true, true, true, true], "", ""),
    ([true, true, true, true], "foo", "???"),
    ([false, false, false, false], "foo", "??"),
    ([true, true, true, true], "foo", "*"),
    ([true, true, true, true], "foo", "f*"),
    ([false, false, false, false], "foo", "*f"),
    ([true, true, true, true], "foo", "*foo*"),
    ([true, true, true, true], "foobar", "*ob*a*r*"),
    ([true, true, true, true], "aaaaaaabababab", "*ab"),
    ([true, true, true, true], "foo*", "foo\\*"),
    ([false, false, false, false], "foobar", "foo\\*bar"),
    ([true, true, true, true], "f\\oo", "f\\\\oo"),
    ([false, false, false, false], "foo\\", "foo\\"),
    ([true, true, true, true], "ball", "*[al]?"),
    ([false, false, false, false], "ten", "[ten]"),
    ([true, true, true, true], "ten", "**[!te]"),
    ([false, false, false, false], "ten", "**[!ten]"),
    ([true, true, true, true], "ten", "t[a-g]n"),
    ([false, false, false, false], "ten", "t[!a-g]n"),
    ([true, true, true, true], "ton", "t[!a-g]n"),
    ([true, true, true, true], "ton", "t[^a-g]n"),
    ([true, true, true, true], "a]b", "a[]]b"),
    ([true, true, true, true], "a-b", "a[]-]b"),
    ([true, true, true, true], "a]b", "a[]-]b"),
    ([false, false, false, false], "aab", "a[]-]b"),
    ([true, true, true, true], "aab", "a[]a-]b"),
    ([true, true, true, true], "]", "]"),
    ([false, false, true, true], "foo/baz/bar", "foo*bar"),
    ([false, false, true, true], "foo/baz/bar", "foo**bar"),
    ([true, true, true, true], "foobazbar", "foo**bar"),
    ([true, true, true, true], "foo/baz/bar", "foo/**/bar"),
    ([true, true, false, false], "foo/baz/bar", "foo/**/**/bar"),
    ([true, true, true, true], "foo/b/a/z/bar", "foo/**/bar"),
    ([true, true, true, true], "foo/b/a/z/bar", "foo/**/**/bar"),
    ([true, true, false, false], "foo/bar", "foo/**/bar"),
    ([true, true, false, false], "foo/bar", "foo/**/**/bar"),
    ([false, false, true, true], "foo/bar", "foo?bar"),
    ([false, false, true, true], "foo/bar", "foo[/]bar"),
    ([false, false, true, true], "foo/bar", "foo[^a-z]bar"),
    ([false, false, true, true], "foo/bar", "f[^eiu][^eiu][^eiu][^eiu][^eiu]r"),
    ([true, true, true, true], "foo-bar", "f[^eiu][^eiu][^eiu][^eiu][^eiu]r"),
    ([true, true, false, false], "foo", "**/foo"),
    ([true, true, true, true], "XXX/foo", "**/foo"),
    ([true, true, true, true], "bar/baz/foo", "**/foo"),
    ([false, false, true, true], "bar/baz/foo", "*/foo"),
    ([false, false, true, true], "foo/bar/baz", "**/bar*"),
    ([true, true, true, true], "deep/foo/bar/baz", "**/bar/*"),
    ([false, false, true, true], "deep/foo/bar/baz/", "**/bar/*"),
    ([true, true, true, true], "deep/foo/bar/baz/", "**/bar/**"),
    ([false, false, false, false], "deep/foo/bar", "**/bar/*"),
    ([true, true, true, true], "deep/foo/bar/", "**/bar/**"),
    ([false, false, true, true], "foo/bar/baz", "**/bar**"),
    ([true, true, true, true], "foo/bar/baz/x", "*/bar/**"),
    ([false, false, true, true], "deep/foo/bar/baz/x", "*/bar/**"),
    ([true, true, true, true], "deep/foo/bar/baz/x", "**/bar/*/*"),
    ([false, false, false, false], "acrt", "a[c-c]st"),
    ([true, true, true, true], "acrt", "a[c-c]rt"),
    ([false, false, false, false], "]", "[!]-]"),
    ([true, true, true, true], "a", "[!]-]"),
    ([false, false, false, false], "", "\\"),
    ([false, false, false, false], "\\", "\\"),
    ([false, false, false, false], "XXX/\\", "*/\\"),
    ([true, true, true, true], "XXX/\\", "*/\\\\"),
    ([true, true, true, true], "foo", "foo"),
    ([true, true, true, true], "@foo", "@foo"),
    ([false, false, false, false], "foo", "@foo"),
    ([true, true, true, true], "[ab]", "\\[ab]"),
    ([true, true, true, true], "[ab]", "[[]ab]"),
    ([true, true, true, true], "[ab]", "[[:]ab]"),
    ([false, false, false, false], "[ab]", "[[::]ab]"),
    ([true, true, true, true], "[ab]", "[[:digit]ab]"),
    ([true, true, true, true], "[ab]", "[\\[:]ab]"),
    ([true, true, true, true], "?a?b", "\\??\\?b"),
    ([true, true, true, true], "abc", "\\a\\b\\c"),
    ([false, false, false, false], "foo", ""),
    ([true, true, true, true], "foo/bar/baz/to", "**/t[o]"),
    ([true, true, true, true], "a1B", "[[:alpha:]][[:digit:]][[:upper:]]"),
    ([false, true, false, true], "a", "[[:digit:][:upper:][:space:]]"),
    ([true, true, true, true], "A", "[[:digit:][:upper:][:space:]]"),
    ([true, true, true, true], "1", "[[:digit:][:upper:][:space:]]"),
    ([false, false, false, false], "1", "[[:digit:][:upper:][:spaci:]]"),
    ([true, true, true, true], " ", "[[:digit:][:upper:][:space:]]"),
    ([false, false, false, false], ".", "[[:digit:][:upper:][:space:]]"),
    ([true, true, true, true], ".", "[[:digit:][:punct:][:space:]]"),
    ([true, true, true, true], "5", "[[:xdigit:]]"),
    ([true, true, true, true], "f", "[[:xdigit:]]"),
    ([true, true, true, true], "D", "[[:xdigit:]]"),
    (
      [true, true, true, true],
      "_",
      "[[:alnum:][:alpha:][:blank:][:cntrl:][:digit:][:graph:][:lower:][:print:][:punct:][:space:][:upper:][:xdigit:]]",
    ),
    (
      [true, true, true, true],
      ".",
      "[^[:alnum:][:alpha:][:blank:][:cntrl:][:digit:][:lower:][:space:][:upper:][:xdigit:]]",
    ),
    ([true, true, true, true], "5", "[a-c[:digit:]x-z]"),
    ([true, true, true, true], "b", "[a-c[:digit:]x-z]"),
    ([true, true, true, true], "y", "[a-c[:digit:]x-z]"),
    ([false, false, false, false], "q", "[a-c[:digit:]x-z]"),
    ([true, true, true, true], "]", "[\\\\-^]"),
    ([false, false, false, false], "[", "[\\\\-^]"),
    ([true, true, true, true], "-", "[\\-_]"),
    ([true, true, true, true], "]", "[\\]]"),
    ([false, false, false, false], "\\]", "[\\]]"),
    ([false, false, false, false], "\\", "[\\]]"),
    ([false, false, false, false], "ab", "a[]b"),
    ([false, false, false, false], "a[]b", "a[]b"),
    ([false, false, false, false], "ab[", "ab["),
    ([false, false, false, false], "ab", "[!"),
    ([false, false, false, false], "ab", "[-"),
    ([true, true, true, true], "-", "[-]"),
    ([false, false, false, false], "-", "[a-"),
    ([false, false, false, false], "-", "[!a-"),
    ([true, true, true, true], "-", "[--A]"),
    ([true, true, true, true], "5", "[--A]"),
    ([true, true, true, true], " ", "[ --]"),
    ([true, true, true, true], "$", "[ --]"),
    ([true, true, true, true], "-", "[ --]"),
    ([false, false, false, false], "0", "[ --]"),
    ([true, true, true, true], "-", "[---]"),
    ([true, true, true, true], "-", "[------]"),
    ([false, false, false, false], "j", "[a-e-n]"),
    ([true, true, true, true], "-", "[a-e-n]"),
    ([true, true, true, true], "a", "[!------]"),
    ([false, false, false, false], "[", "[]-a]"),
    ([true, true, true, true], "^", "[]-a]"),
    ([false, false, false, false], "^", "[!]-a]"),
    ([true, true, true, true], "[", "[!]-a]"),
    ([true, true, true, true], "^", "[a^bc]"),
    ([true, true, true, true], "-b]", "[a-]b]"),
    ([false, false, false, false], "\\", "[\\]"),
    ([true, true, true, true], "\\", "[\\\\]"),
    ([false, false, false, false], "\\", "[!\\\\]"),
    ([true, true, true, true], "G", "[A-\\\\]"),
    ([false, false, false, false], "aaabbb", "b*a"),
    ([false, false, false, false], "aabcaa", "*ba*"),
    ([true, true, true, true], ",", "[,]"),
    ([true, true, true, true], ",", "[\\\\,]"),
    ([true, true, true, true], "\\", "[\\\\,]"),
    ([true, true, true, true], "-", "[,-.]"),
    ([false, false, false, false], "+", "[,-.]"),
    ([false, false, false, false], "-.]", "[,-.]"),
    ([true, true, true, true], "2", "[\\1-\\3]"),
    ([true, true, true, true], "3", "[\\1-\\3]"),
    ([false, false, false, false], "4", "[\\1-\\3]"),
    ([true, true, true, true], "\\", "[[-\\]]"),
    ([true, true, true, true], "[", "[[-\\]]"),
    ([true, true, true, true], "]", "[[-\\]]"),
    ([false, false, false, false], "-", "[[-\\]]"),
    (
      [true, true, true, true],
      "-adobe-courier-bold-o-normal--12-120-75-75-m-70-iso8859-1",
      "-*-*-*-*-*-*-12-*-*-*-m-*-*-*",
    ),
    (
      [false, false, false, false],
      "-adobe-courier-bold-o-normal--12-120-75-75-X-70-iso8859-1",
      "-*-*-*-*-*-*-12-*-*-*-m-*-*-*",
    ),
    (
      [false, false, false, false],
      "-adobe-courier-bold-o-normal--12-120-75-75-/-70-iso8859-1",
      "-*-*-*-*-*-*-12-*-*-*-m-*-*-*",
    ),
    (
      [true, true, true, true],
      "XXX/adobe/courier/bold/o/normal//12/120/75/75/m/70/iso8859/1",
      "XXX/*/*/*/*/*/*/12/*/*/*/m/*/*/*",
    ),
    (
      [false, false, false, false],
      "XXX/adobe/courier/bold/o/normal//12/120/75/75/X/70/iso8859/1",
      "XXX/*/*/*/*/*/*/12/*/*/*/m/*/*/*",
    ),
    ([true, true, true, true], "abcd/abcdefg/abcdefghijk/abcdefghijklmnop.txt", "**/*a*b*g*n*t"),
    ([false, false, false, false], "abcd/abcdefg/abcdefghijk/abcdefghijklmnop.txtz", "**/*a*b*g*n*t"),
    ([false, false, false, false], "foo", "*/*/*"),
    ([false, false, false, false], "foo/bar", "*/*/*"),
    ([true, true, true, true], "foo/bba/arr", "*/*/*"),
    ([false, false, true, true], "foo/bb/aa/rr", "*/*/*"),
    ([true, true, true, true], "foo/bb/aa/rr", "**/**/**"),
    ([true, true, true, true], "abcXdefXghi", "*X*i"),
    ([false, false, true, true], "ab/cXd/efXg/hi", "*X*i"),
    ([true, true, true, true], "ab/cXd/efXg/hi", "*/*X*/*/*i"),
    ([true, true, true, true], "ab/cXd/efXg/hi", "**/*X*/**/*i"),
    ([false, false, false, false], "foo", "fo"),
    ([true, true, true, true], "foo/bar", "foo/bar"),
    ([true, true, true, true], "foo/bar", "foo/*"),
    ([false, false, true, true], "foo/bba/arr", "foo/*"),
    ([true, true, true, true], "foo/bba/arr", "foo/**"),
    ([false, false, true, true], "foo/bba/arr", "foo*"),
    ([false, false, true, true], "foo/bba/arr", "foo**"),
    ([false, false, true, true], "foo/bba/arr", "foo/*arr"),
    ([false, false, true, true], "foo/bba/arr", "foo/**arr"),
    ([false, false, false, false], "foo/bba/arr", "foo/*z"),
    ([false, false, false, false], "foo/bba/arr", "foo/**z"),
    ([false, false, true, true], "foo/bar", "foo?bar"),
    ([false, false, true, true], "foo/bar", "foo[/]bar"),
    ([false, false, true, true], "foo/bar", "foo[^a-z]bar"),
    ([false, false, true, true], "ab/cXd/efXg/hi", "*Xg*i"),
    ([false, true, false, true], "a", "[A-Z]"),
    ([true, true, true, true], "A", "[A-Z]"),
    ([false, true, false, true], "A", "[a-z]"),
    ([true, true, true, true], "a", "[a-z]"),
    ([false, true, false, true], "a", "[[:upper:]]"),
    ([true, true, true, true], "A", "[[:upper:]]"),
    ([false, true, false, true], "A", "[[:lower:]]"),
    ([true, true, true, true], "a", "[[:lower:]]"),
    ([false, true, false, true], "A", "[B-Za]"),
    ([true, true, true, true], "a", "[B-Za]"),
    ([false, true, false, true], "A", "[B-a]"),
    ([true, true, true, true], "a", "[B-a]"),
    ([false, true, false, true], "z", "[Z-y]"),
    ([true, true, true, true], "Z", "[Z-y]"),
  ];

  #[test]
  fn matches_git_test_vectors() {
    let modes = [WM_PATHNAME, WM_PATHNAME | WM_CASEFOLD, 0, WM_CASEFOLD];
    let mut failures = Vec::new();
    for (expected, text, pattern) in VECTORS {
      for (mode, expected) in modes.iter().zip(expected) {
        if wildmatch(pattern.as_bytes(), text.as_bytes(), *mode) != *expected {
          failures.push(format!("{text:?} {pattern:?} flags {mode}: expected {expected}"));
        }
      }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
  }

  #[test]
  fn does_not_backtrack_exponentially() {
    let start = std::time::Instant::now();
    assert!(!wildmatch(
      b"*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a",
      b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaab",
      0
    ));
    assert!(start.elapsed() < std::time::Duration::from_secs(1));
  }

  #[test]
  fn treats_nul_as_the_end() {
    assert!(wildmatch(b"foo\0bar", b"foo", 0));
    assert!(wildmatch(b"foo", b"foo\0bar", 0));
  }
}
