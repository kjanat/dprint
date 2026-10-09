#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternMatch {
  None,
  Positive,
  Negative,
}

#[derive(Debug, Clone, Default)]
pub struct PatternList {
  patterns: Vec<Pattern>,
  negative_count: usize,
  ignore_case: bool,
}

impl PatternList {
  pub fn new(ignore_case: bool) -> Self {
    Self {
      patterns: Vec::new(),
      negative_count: 0,
      ignore_case,
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

  pub fn add_buffer(&mut self, buffer: &[u8]) {
    let buffer = buffer.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(buffer);
    for line in buffer.split(|&byte| byte == b'\n') {
      self.add_line(line);
    }
  }

  pub fn add_line(&mut self, line: &[u8]) {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line.first() == Some(&b'#') {
      return;
    }
    let line = trim_trailing_spaces(line);
    if line.is_empty() {
      return;
    }
    let (negated, body) = match line.strip_prefix(b"!") {
      Some(body) => (true, body),
      None => (false, line),
    };
    if negated {
      self.negative_count += 1;
    }
    self.patterns.push(Pattern::parse(negated, body, self.ignore_case));
  }

  pub fn matched(&self, path: &[u8], is_dir: bool) -> PatternMatch {
    if path.is_empty() {
      return PatternMatch::None;
    }
    let name = match path.iter().rposition(|&byte| byte == b'/') {
      Some(index) => &path[index + 1..],
      None => path,
    };
    for pattern in self.patterns.iter().rev() {
      if pattern.dir_only && !is_dir {
        continue;
      }
      if pattern.matcher.matches(path, name, self.ignore_case) {
        return if pattern.negated { PatternMatch::Negative } else { PatternMatch::Positive };
      }
    }
    PatternMatch::None
  }
}

fn trim_trailing_spaces(line: &[u8]) -> &[u8] {
  let mut end = line.len();
  while end > 0 && line[end - 1] == b' ' {
    let backslashes = line[..end - 1].iter().rev().take_while(|&&byte| byte == b'\\').count();
    if backslashes % 2 == 1 {
      break;
    }
    end -= 1;
  }
  &line[..end]
}

#[derive(Debug, Clone)]
struct Pattern {
  negated: bool,
  dir_only: bool,
  matcher: Matcher,
}

impl Pattern {
  fn parse(negated: bool, body: &[u8], fold: bool) -> Self {
    let (dir_only, body) = match body.strip_suffix(b"/") {
      Some(body) => (true, body),
      None => (false, body),
    };
    Self {
      negated,
      dir_only,
      matcher: Matcher::parse(body, fold).unwrap_or(Matcher::Never),
    }
  }
}

#[derive(Debug, Clone)]
enum Matcher {
  Never,
  Name(Component),
  Path(PathMatcher),
}

impl Matcher {
  fn parse(body: &[u8], fold: bool) -> Option<Self> {
    let (anchored, body) = match body.strip_prefix(b"/") {
      Some(body) => (true, body),
      None => (body.contains(&b'/'), body),
    };
    if body.is_empty() {
      return None;
    }
    let tokens = tokenize(body, fold)?;
    if !anchored {
      return Some(Self::Name(Component::new(tokens)));
    }
    let mut segments = Vec::new();
    for part in tokens.split(|token| matches!(token, RawToken::Slash)) {
      match part {
        [RawToken::DoubleStar] => segments.push(Segment::Recursive),
        [RawToken::DoubleStarBeforeEscapedSlash] => segments.extend([Segment::One(Component::Anything), Segment::Recursive]),
        _ => segments.push(Segment::One(Component::new(part.to_vec()))),
      }
    }
    Some(match <[Segment; 2]>::try_from(segments) {
      Ok([Segment::Recursive, Segment::One(component)]) => Self::Name(component),
      Ok(pair) => Self::Path(PathMatcher::new(pair.into())),
      Err(segments) => Self::Path(PathMatcher::new(segments)),
    })
  }

  fn matches(&self, path: &[u8], name: &[u8], fold: bool) -> bool {
    match self {
      Self::Never => false,
      Self::Name(component) => component.matches(name, fold),
      Self::Path(matcher) => matcher.matches(path, fold),
    }
  }
}

#[derive(Debug, Clone)]
enum Segment {
  One(Component),
  Recursive,
}

#[derive(Debug, Clone)]
enum PathMatcher {
  Literal(Box<[u8]>),
  Under(Box<[u8]>),
  Segments(Box<[Segment]>),
}

impl PathMatcher {
  fn new(segments: Vec<Segment>) -> Self {
    let (recursive_tail, init) = match segments.split_last() {
      Some((Segment::Recursive, init)) => (true, init),
      _ => (false, segments.as_slice()),
    };
    let mut literal = Vec::new();
    for (index, segment) in init.iter().enumerate() {
      let Segment::One(Component::Literal(bytes)) = segment else {
        return Self::Segments(segments.into_boxed_slice());
      };
      if index > 0 {
        literal.push(b'/');
      }
      literal.extend_from_slice(bytes);
    }
    if !recursive_tail {
      return Self::Literal(literal.into_boxed_slice());
    }
    if !init.is_empty() {
      literal.push(b'/');
    }
    Self::Under(literal.into_boxed_slice())
  }

  fn matches(&self, path: &[u8], fold: bool) -> bool {
    match self {
      Self::Literal(literal) => eq_bytes(literal, path, fold),
      Self::Under(prefix) => path.len() > prefix.len() && eq_bytes(prefix, &path[..prefix.len()], fold),
      Self::Segments(segments) => match_segments(segments, path, fold),
    }
  }
}

fn match_segments(segments: &[Segment], path: &[u8], fold: bool) -> bool {
  let end = path.len();
  let mut index = 0;
  let mut position = 0;
  let mut resume: Option<(usize, usize)> = None;
  loop {
    match segments.get(index) {
      Some(Segment::Recursive) if index + 1 == segments.len() => return position < end,
      Some(Segment::Recursive) => {
        index += 1;
        resume = Some((index, position));
        continue;
      }
      Some(Segment::One(component)) if position < end => {
        let (text, next) = component_at(path, position);
        if component.matches(text, fold) {
          index += 1;
          position = next;
          continue;
        }
      }
      Some(Segment::One(_)) => {}
      None if position >= end => return true,
      None => {}
    }
    match resume {
      Some((resume_index, resume_position)) if resume_position < end => {
        let (_, next) = component_at(path, resume_position);
        resume = Some((resume_index, next));
        index = resume_index;
        position = next;
      }
      _ => return false,
    }
  }
}

fn component_at(path: &[u8], position: usize) -> (&[u8], usize) {
  let rest = &path[position..];
  match rest.iter().position(|&byte| byte == b'/') {
    Some(length) => (&rest[..length], position + length + 1),
    None => (rest, path.len()),
  }
}

#[derive(Debug, Clone)]
enum Component {
  Literal(Box<[u8]>),
  Prefix(Box<[u8]>),
  Suffix(Box<[u8]>),
  Anything,
  Glob(Box<[Token]>),
}

impl Component {
  fn new(raw: Vec<RawToken>) -> Self {
    let tokens = raw
      .into_iter()
      .map(|token| match token {
        RawToken::Byte(byte) => Token::Byte(byte),
        RawToken::AnyByte => Token::AnyByte,
        RawToken::Star | RawToken::DoubleStar | RawToken::DoubleStarBeforeEscapedSlash => Token::Star,
        RawToken::Class(set) => Token::Class(set),
        RawToken::Slash => Token::Byte(b'/'),
      })
      .collect::<Vec<_>>();
    let literal = |tokens: &[Token]| -> Option<Box<[u8]>> {
      tokens
        .iter()
        .map(|token| match token {
          Token::Byte(byte) => Some(*byte),
          _ => None,
        })
        .collect()
    };
    match tokens.as_slice() {
      [Token::Star] => Self::Anything,
      [Token::Star, rest @ ..] if let Some(bytes) = literal(rest) => Self::Suffix(bytes),
      [rest @ .., Token::Star] if let Some(bytes) = literal(rest) => Self::Prefix(bytes),
      _ => match literal(&tokens) {
        Some(bytes) => Self::Literal(bytes),
        None => Self::Glob(tokens.into_boxed_slice()),
      },
    }
  }

  fn matches(&self, text: &[u8], fold: bool) -> bool {
    match self {
      Self::Literal(literal) => eq_bytes(literal, text, fold),
      Self::Prefix(prefix) => text.len() >= prefix.len() && eq_bytes(prefix, &text[..prefix.len()], fold),
      Self::Suffix(suffix) => text.len() >= suffix.len() && eq_bytes(suffix, &text[text.len() - suffix.len()..], fold),
      Self::Anything => true,
      Self::Glob(tokens) => match_glob(tokens, text, fold),
    }
  }
}

#[derive(Debug, Clone)]
enum Token {
  Byte(u8),
  AnyByte,
  Star,
  Class(Box<ByteSet>),
}

impl Token {
  fn matches(&self, byte: u8, fold: bool) -> bool {
    match self {
      Self::Byte(expected) => eq_byte(*expected, byte, fold),
      Self::AnyByte => true,
      Self::Star => false,
      Self::Class(set) => set.contains(byte),
    }
  }
}

fn match_glob(tokens: &[Token], text: &[u8], fold: bool) -> bool {
  let mut index = 0;
  let mut position = 0;
  let mut resume: Option<(usize, usize)> = None;
  loop {
    match tokens.get(index) {
      Some(Token::Star) if index + 1 == tokens.len() => return true,
      Some(Token::Star) => {
        index += 1;
        resume = Some((index, position));
        continue;
      }
      Some(token) if position < text.len() && token.matches(text[position], fold) => {
        index += 1;
        position += 1;
        continue;
      }
      Some(_) => {}
      None if position == text.len() => return true,
      None => {}
    }
    match resume {
      Some((resume_index, resume_position)) if resume_position < text.len() => {
        resume = Some((resume_index, resume_position + 1));
        index = resume_index;
        position = resume_position + 1;
      }
      _ => return false,
    }
  }
}

fn eq_byte(expected: u8, byte: u8, fold: bool) -> bool {
  expected == if fold { byte.to_ascii_lowercase() } else { byte }
}

fn eq_bytes(expected: &[u8], text: &[u8], fold: bool) -> bool {
  if fold {
    expected.len() == text.len() && expected.iter().zip(text).all(|(&expected, &byte)| eq_byte(expected, byte, true))
  } else {
    expected == text
  }
}

#[derive(Debug, Clone)]
enum RawToken {
  Byte(u8),
  AnyByte,
  Star,
  DoubleStar,
  DoubleStarBeforeEscapedSlash,
  Class(Box<ByteSet>),
  Slash,
}

fn tokenize(pattern: &[u8], fold: bool) -> Option<Vec<RawToken>> {
  let fold_byte = |byte: u8| if fold { byte.to_ascii_lowercase() } else { byte };
  let mut tokens = Vec::with_capacity(pattern.len());
  let mut index = 0;
  while let Some(&byte) = pattern.get(index) {
    index += 1;
    tokens.push(match byte {
      b'\\' => {
        let escaped = *pattern.get(index)?;
        index += 1;
        match escaped {
          b'/' => RawToken::Slash,
          // With core.ignoreCase, git lowercases the path byte but not an escaped pattern byte.
          _ => RawToken::Byte(escaped),
        }
      }
      b'*' => {
        let start = index - 1;
        while pattern.get(index) == Some(&b'*') {
          index += 1;
        }
        let after_slash = start == 0 || pattern[start - 1] == b'/';
        let rest = &pattern[index..];
        if index - start < 2 || !after_slash {
          RawToken::Star
        } else if rest.is_empty() || rest[0] == b'/' {
          RawToken::DoubleStar
        } else if rest.starts_with(b"\\/") {
          RawToken::DoubleStarBeforeEscapedSlash
        } else {
          RawToken::Star
        }
      }
      b'?' => RawToken::AnyByte,
      b'[' => {
        let (set, next) = parse_class(pattern, index, fold)?;
        index = next;
        RawToken::Class(Box::new(set))
      }
      b'/' => RawToken::Slash,
      _ => RawToken::Byte(fold_byte(byte)),
    });
  }
  Some(tokens)
}

fn parse_class(pattern: &[u8], start: usize, fold: bool) -> Option<(ByteSet, usize)> {
  let mut set = ByteSet::default();
  let mut index = start;
  let negated = matches!(pattern.get(index), Some(b'!' | b'^'));
  if negated {
    index += 1;
  }
  let first = index;
  let mut previous: Option<u8> = None;
  loop {
    let byte = *pattern.get(index)?;
    match byte {
      b']' if index > first => {
        index += 1;
        break;
      }
      b'\\' => {
        let escaped = *pattern.get(index + 1)?;
        set.insert_single(escaped, fold);
        previous = Some(escaped);
        index += 2;
      }
      b'-'
        if let Some(low) = previous
          && let Some(&next) = pattern.get(index + 1)
          && next != b']' =>
      {
        let (high, after) = match next {
          b'\\' => (*pattern.get(index + 2)?, index + 3),
          _ => (next, index + 2),
        };
        set.insert_range(low, high, fold);
        previous = None;
        index = after;
      }
      b'['
        if pattern.get(index + 1) == Some(&b':')
          && let Some((name, after)) = class_name(pattern, index + 2) =>
      {
        if !set.insert_named(name, fold) {
          return None;
        }
        previous = None;
        index = after;
      }
      _ => {
        set.insert_single(byte, fold);
        previous = Some(byte);
        index += 1;
      }
    }
  }
  if negated {
    set.complement();
  }
  set.remove(b'/');
  Some((set, index))
}

fn class_name(pattern: &[u8], start: usize) -> Option<(&[u8], usize)> {
  let close = start + pattern[start..].iter().position(|&byte| byte == b']')?;
  (close > start && pattern[close - 1] == b':').then(|| (&pattern[start..close - 1], close + 1))
}

#[derive(Debug, Clone, Default)]
struct ByteSet([u64; 4]);

impl ByteSet {
  fn contains(&self, byte: u8) -> bool {
    self.0[usize::from(byte >> 6)] & (1 << (byte & 63)) != 0
  }

  fn insert(&mut self, byte: u8) {
    self.0[usize::from(byte >> 6)] |= 1 << (byte & 63);
  }

  fn remove(&mut self, byte: u8) {
    self.0[usize::from(byte >> 6)] &= !(1 << (byte & 63));
  }

  fn insert_either_case(&mut self, byte: u8, fold: bool) {
    self.insert(byte);
    if fold {
      self.insert(byte.to_ascii_lowercase());
      self.insert(byte.to_ascii_uppercase());
    }
  }

  fn insert_single(&mut self, byte: u8, fold: bool) {
    // With core.ignoreCase, git compares a bracket expression's single characters with the lowercased path byte only.
    if !(fold && byte.is_ascii_uppercase()) {
      self.insert_either_case(byte, fold);
    }
  }

  fn insert_range(&mut self, low: u8, high: u8, fold: bool) {
    for byte in low..=high {
      self.insert_either_case(byte, fold);
    }
  }

  fn insert_named(&mut self, name: &[u8], fold: bool) -> bool {
    let predicate: fn(u8) -> bool = match name {
      b"alnum" => |byte| byte.is_ascii_alphanumeric(),
      b"alpha" => |byte| byte.is_ascii_alphabetic(),
      b"blank" => |byte| matches!(byte, b' ' | b'\t'),
      b"cntrl" => |byte| byte.is_ascii_control(),
      b"digit" => |byte| byte.is_ascii_digit(),
      b"graph" => |byte| byte.is_ascii_graphic(),
      b"lower" => |byte| byte.is_ascii_lowercase(),
      b"print" => |byte| byte == b' ' || byte.is_ascii_graphic(),
      b"punct" => |byte| byte.is_ascii_punctuation(),
      b"space" => |byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r'),
      b"upper" => |byte| byte.is_ascii_uppercase(),
      b"xdigit" => |byte| byte.is_ascii_hexdigit(),
      _ => return false,
    };
    for byte in (0..=u8::MAX).filter(|&byte| predicate(byte)) {
      self.insert_either_case(byte, fold);
    }
    true
  }

  fn complement(&mut self) {
    for word in &mut self.0 {
      *word = !*word;
    }
  }
}

#[cfg(test)]
mod test {
  use std::path::Path;
  use std::process::Command;
  use std::process::Stdio;

  use super::PatternList;
  use super::PatternMatch;
  use super::PatternMatch::Negative;
  use super::PatternMatch::Positive;

  const NONE: PatternMatch = PatternMatch::None;
  const FILE: bool = false;
  const DIR: bool = true;

  fn list(buffer: &[u8], ignore_case: bool) -> PatternList {
    let mut patterns = PatternList::new(ignore_case);
    patterns.add_buffer(buffer);
    patterns
  }

  #[track_caller]
  fn check(buffer: &str, cases: &[(&str, bool, PatternMatch)]) {
    check_bytes(buffer.as_bytes(), false, cases);
  }

  #[track_caller]
  fn check_ignore_case(buffer: &str, cases: &[(&str, bool, PatternMatch)]) {
    check_bytes(buffer.as_bytes(), true, cases);
  }

  #[track_caller]
  fn check_bytes(buffer: &[u8], ignore_case: bool, cases: &[(&str, bool, PatternMatch)]) {
    let patterns = list(buffer, ignore_case);
    for &(path, is_dir, expected) in cases {
      assert_eq!(
        patterns.matched(path.as_bytes(), is_dir),
        expected,
        "{:?} (ignore case: {ignore_case}) against {path:?} (dir: {is_dir})",
        String::from_utf8_lossy(buffer),
      );
    }
  }

  #[test]
  fn counts_patterns() {
    let patterns = list(b"\n# comment\n   \n\\#a\n!b\n!\nc/\n\r\n", false);
    assert_eq!(patterns.len(), 4);
    assert_eq!(patterns.negative_count(), 2);
    assert!(!patterns.is_empty());
    let patterns = list(b"# only\n\n", false);
    assert_eq!(patterns.len(), 0);
    assert!(patterns.is_empty());
    assert!(PatternList::default().is_empty());
    assert_eq!(PatternList::new(true).matched(b"a", FILE), NONE);
  }

  #[test]
  fn matches_names_at_any_depth() {
    check(
      "foo",
      &[
        ("foo", FILE, Positive),
        ("foo", DIR, Positive),
        ("a/foo", FILE, Positive),
        ("a/b/foo", DIR, Positive),
        ("foobar", FILE, NONE),
        ("foo/bar", FILE, NONE),
        ("", FILE, NONE),
      ],
    );
  }

  #[test]
  fn anchors_patterns_with_a_slash() {
    check("/foo", &[("foo", FILE, Positive), ("a/foo", FILE, NONE)]);
    check(
      "a/b",
      &[("a/b", FILE, Positive), ("a/b", DIR, Positive), ("x/a/b", FILE, NONE), ("a/b/c", FILE, NONE)],
    );
    check("/a/b", &[("a/b", FILE, Positive), ("x/a/b", FILE, NONE)]);
    check("a[/x]b", &[("axb", FILE, Positive), ("q/axb", FILE, NONE)]);
    check("a\\/b", &[("a/b", FILE, Positive), ("x/a/b", FILE, NONE)]);
    check("//a", &[("a", FILE, NONE)]);
    check("a//b", &[("a/b", FILE, NONE)]);
    check("/", &[("a", DIR, NONE)]);
  }

  #[test]
  fn matches_directories_only_with_a_trailing_slash() {
    check(
      "foo/",
      &[("foo", DIR, Positive), ("foo", FILE, NONE), ("a/foo", DIR, Positive), ("foo/a", FILE, NONE)],
    );
    check(
      "doc/frotz/",
      &[("doc/frotz", DIR, Positive), ("doc/frotz", FILE, NONE), ("a/doc/frotz", DIR, NONE)],
    );
    check("a//", &[("a", DIR, NONE), ("x/a", DIR, NONE)]);
    check("a/ ", &[("a", DIR, Positive), ("x/a", DIR, Positive)]);
    check("a\\\\/", &[("a\\", DIR, Positive), ("a\\", FILE, NONE)]);
  }

  #[test]
  fn matches_single_asterisks_within_a_component() {
    check(
      "*.ts",
      &[
        ("a.ts", FILE, Positive),
        ("x/y/a.ts", FILE, Positive),
        (".ts", FILE, Positive),
        ("a.tsx", FILE, NONE),
        ("a.ts/b", FILE, NONE),
      ],
    );
    check("*", &[("a", FILE, Positive), ("a/b", DIR, Positive), (".hidden", FILE, Positive)]);
    check(
      "foo/*",
      &[("foo/a", FILE, Positive), ("foo/a", DIR, Positive), ("foo/a/b", FILE, NONE), ("foo", DIR, NONE)],
    );
    check("/*.c", &[("a.c", FILE, Positive), ("x/a.c", FILE, NONE)]);
    check(
      "a*",
      &[("a", FILE, Positive), ("abc", FILE, Positive), ("x/abc", FILE, Positive), ("ba", FILE, NONE)],
    );
    check("a*b*c", &[("abc", FILE, Positive), ("axxbyyc", FILE, Positive), ("acb", FILE, NONE)]);
    check("a/*b", &[("a/b", FILE, Positive), ("a/xb", FILE, Positive), ("a/x/b", FILE, NONE)]);
    check("x/*", &[("x/.a", FILE, Positive)]);
  }

  #[test]
  fn matches_question_marks_within_a_component() {
    check("?", &[("a", FILE, Positive), ("x/a", FILE, Positive), ("ab", FILE, NONE)]);
    check("a?b", &[("axb", FILE, Positive), ("a/b", FILE, NONE)]);
    check("?a", &[(".a", FILE, Positive)]);
  }

  #[test]
  fn matches_double_asterisks() {
    check(
      "**/foo",
      &[
        ("foo", FILE, Positive),
        ("a/foo", FILE, Positive),
        ("a/b/foo", DIR, Positive),
        ("afoo", FILE, NONE),
      ],
    );
    check(
      "**/foo/bar",
      &[("foo/bar", FILE, Positive), ("a/foo/bar", FILE, Positive), ("foo/x/bar", FILE, NONE)],
    );
    check(
      "abc/**",
      &[
        ("abc", DIR, NONE),
        ("abc/x", FILE, Positive),
        ("abc/x/y", FILE, Positive),
        ("x/abc/y", FILE, NONE),
      ],
    );
    check(
      "a/**/b",
      &[
        ("a/b", FILE, Positive),
        ("a/x/b", FILE, Positive),
        ("a/x/y/b", FILE, Positive),
        ("a/xb", FILE, NONE),
        ("ab", FILE, NONE),
        ("a/x/b/c", FILE, NONE),
      ],
    );
    check(
      "a/**/b/**/c",
      &[
        ("a/b/c", FILE, Positive),
        ("a/x/b/y/z/c", FILE, Positive),
        ("a/b/b/c", FILE, Positive),
        ("a/c", FILE, NONE),
      ],
    );
    check("a/**/", &[("a/b", DIR, Positive), ("a/b", FILE, NONE), ("a", DIR, NONE)]);
    check("**", &[("a", FILE, Positive), ("a/b", FILE, Positive)]);
    check("/**", &[("a", FILE, Positive), ("a/b", DIR, Positive)]);
    check("/**/", &[("a", DIR, Positive), ("a", FILE, NONE)]);
    check("**/", &[("a", DIR, Positive), ("a", FILE, NONE)]);
    check("**/**", &[("a", FILE, Positive), ("a/b", FILE, Positive)]);
    check("**/*", &[("a", FILE, Positive), ("x/y", FILE, Positive)]);
    check("**/a/**", &[("a/b", FILE, Positive), ("x/y/a/b/c", FILE, Positive), ("a", DIR, NONE)]);
    check(
      "x/**/*.ts",
      &[("x/a.ts", FILE, Positive), ("x/y/a.ts", FILE, Positive), ("y/x/a.ts", FILE, NONE)],
    );
  }

  #[test]
  fn treats_other_double_asterisks_as_single_ones() {
    check("a/**b", &[("a/b", FILE, Positive), ("a/xb", FILE, Positive), ("a/x/b", FILE, NONE)]);
    check("a**/b", &[("a/b", FILE, Positive), ("ax/b", FILE, Positive), ("ax/y/b", FILE, NONE)]);
    check("**a/b", &[("a/b", FILE, Positive), ("xa/b", FILE, Positive), ("x/a/b", FILE, NONE)]);
    check("a/b**/c", &[("a/bx/c", FILE, Positive), ("a/bx/y/c", FILE, NONE)]);
    check("**x", &[("y/zx", FILE, Positive), ("x", FILE, Positive)]);
    check("a***b", &[("ab", FILE, Positive), ("axyb", FILE, Positive)]);
    check("\\**/a", &[("*x/a", FILE, Positive), ("x/y/a", FILE, NONE), ("a", FILE, NONE)]);
    check("a/\\**/b", &[("a/*x/b", FILE, Positive), ("a/x/b", FILE, NONE)]);
  }

  #[test]
  fn treats_longer_asterisk_runs_between_slashes_as_double_asterisks() {
    check("***/a", &[("a", FILE, Positive), ("x/y/a", FILE, Positive)]);
    check("a/***", &[("a/b/c", FILE, Positive), ("a", DIR, NONE)]);
    check("a/****/b", &[("a/b", FILE, Positive), ("a/x/y/b", FILE, Positive)]);
  }

  #[test]
  fn matches_one_or_more_directories_with_double_asterisks_before_an_escaped_slash() {
    check(
      "a/**\\/b",
      &[
        ("a/b", FILE, NONE),
        ("a/x/b", FILE, Positive),
        ("a/x/y/b", FILE, Positive),
        ("a/xb", FILE, NONE),
      ],
    );
    check("**\\/b", &[("b", FILE, NONE), ("x/b", FILE, Positive), ("x/y/b", DIR, Positive)]);
    check("a\\/***\\/b", &[("a/b", FILE, NONE), ("a/x/y/b", FILE, Positive)]);
    check("**\\/**", &[("a", FILE, NONE), ("a/b", FILE, Positive)]);
    check("x**\\/b", &[("x/b", FILE, Positive), ("xy/b", FILE, Positive), ("xy/z/b", FILE, NONE)]);
    check("a/**\\\\/b", &[("a/x\\/b", FILE, Positive), ("a/x/b", FILE, NONE)]);
    check("a\\/**", &[("a/b/c", FILE, Positive), ("a", DIR, NONE)]);
    check("\\/**", &[("a", FILE, NONE), ("a/b", FILE, NONE)]);
    check("//**", &[("a", FILE, NONE), ("a/b", FILE, NONE)]);
    check("a/**\\/", &[("a/b/c", DIR, NONE)]);
  }

  #[test]
  fn matches_bracket_expressions() {
    check("[ab]", &[("a", FILE, Positive), ("x/b", FILE, Positive), ("c", FILE, NONE)]);
    check("[!ab]", &[("a", FILE, NONE), ("c", FILE, Positive)]);
    check("[^ab]", &[("a", FILE, NONE), ("c", FILE, Positive)]);
    check("[]a]", &[("]", FILE, Positive), ("a", FILE, Positive), ("b", FILE, NONE)]);
    check("[!]a]", &[("]", FILE, NONE), ("a", FILE, NONE), ("b", FILE, Positive)]);
    check("[a-c]", &[("b", FILE, Positive), ("d", FILE, NONE), ("-", FILE, NONE)]);
    check("[a-]", &[("a", FILE, Positive), ("-", FILE, Positive), ("b", FILE, NONE)]);
    check("[-a]", &[("a", FILE, Positive), ("-", FILE, Positive), ("b", FILE, NONE)]);
    check("[--0]", &[("-", FILE, Positive), ("0", FILE, Positive), ("1", FILE, NONE)]);
    check(
      "[]-a]",
      &[("]", FILE, Positive), ("^", FILE, Positive), ("a", FILE, Positive), ("b", FILE, NONE)],
    );
    check(
      "[a-c-e]",
      &[("b", FILE, Positive), ("-", FILE, Positive), ("e", FILE, Positive), ("d", FILE, NONE)],
    );
    check("[!a-c]", &[("a", FILE, NONE), ("d", FILE, Positive)]);
    check("[a!]", &[("!", FILE, Positive), ("b", FILE, NONE)]);
    check("[!!]", &[("!", FILE, NONE), ("a", FILE, Positive)]);
    check("a[!x]b", &[("ayb", FILE, Positive), ("axb", FILE, NONE), ("a/b", FILE, NONE)]);
    check("a[%-0]b", &[("a.b", FILE, Positive), ("a/b", FILE, NONE)]);
  }

  #[test]
  fn matches_reversed_ranges_by_their_start_only() {
    check("[c-a]", &[("c", FILE, Positive), ("a", FILE, NONE), ("b", FILE, NONE)]);
    check("[d-ax]", &[("d", FILE, Positive), ("x", FILE, Positive), ("b", FILE, NONE)]);
  }

  #[test]
  fn escapes_within_bracket_expressions() {
    check("[\\]]", &[("]", FILE, Positive), ("\\", FILE, NONE)]);
    check("[\\a]", &[("a", FILE, Positive), ("\\", FILE, NONE)]);
    check(
      "[a\\-c]",
      &[("a", FILE, Positive), ("-", FILE, Positive), ("c", FILE, Positive), ("b", FILE, NONE)],
    );
    check("[\\a-\\c]", &[("b", FILE, Positive), ("\\", FILE, NONE)]);
    check("[\\]-a]", &[("^", FILE, Positive), ("\\", FILE, NONE)]);
    check("[\\!]", &[("!", FILE, Positive), ("\\", FILE, NONE)]);
  }

  #[test]
  fn matches_character_classes() {
    check("[[:digit:]]", &[("1", FILE, Positive), ("a", FILE, NONE)]);
    check("[[:alpha:]x]", &[("a", FILE, Positive), ("x", FILE, Positive), ("1", FILE, NONE)]);
    check("[[:digit:][:alpha:]]", &[("1", FILE, Positive), ("a", FILE, Positive), ("-", FILE, NONE)]);
    check(
      "[[:digit:]-z]",
      &[("1", FILE, Positive), ("-", FILE, Positive), ("z", FILE, Positive), ("m", FILE, NONE)],
    );
    check("[[:alnum:]]", &[("Z", FILE, Positive), ("5", FILE, Positive), ("_", FILE, NONE)]);
    check("[[:blank:]]", &[(" ", FILE, Positive), ("\t", FILE, Positive), ("\n", FILE, NONE)]);
    check("[[:cntrl:]]", &[("\x01", FILE, Positive), ("\x7f", FILE, Positive), (" ", FILE, NONE)]);
    check("[[:graph:]]", &[("!", FILE, Positive), ("~", FILE, Positive), (" ", FILE, NONE)]);
    check("[[:lower:]]", &[("a", FILE, Positive), ("A", FILE, NONE)]);
    check("[[:upper:]]", &[("A", FILE, Positive), ("a", FILE, NONE)]);
    check("[[:print:]]", &[(" ", FILE, Positive), ("a", FILE, Positive), ("\x01", FILE, NONE)]);
    check("a[[:punct:]]", &[("a.", FILE, Positive), ("a_", FILE, Positive), ("aa", FILE, NONE)]);
    check(
      "[[:space:]]",
      &[
        (" ", FILE, Positive),
        ("\t", FILE, Positive),
        ("\n", FILE, Positive),
        ("\r", FILE, Positive),
        ("\x0b", FILE, NONE),
        ("\x0c", FILE, NONE),
      ],
    );
    check("[[:xdigit:]]", &[("f", FILE, Positive), ("F", FILE, Positive), ("g", FILE, NONE)]);
    check("[[:", &[("[", FILE, NONE)]);
    check("[[:]", &[(":", FILE, Positive), ("[", FILE, Positive)]);
    check("[[:a]", &[(":", FILE, Positive), ("a", FILE, Positive), ("[", FILE, Positive)]);
    check("[[.a.]]", &[("a]", FILE, Positive), (".]", FILE, Positive), ("a", FILE, NONE)]);
    check("[[=a=]]", &[("=]", FILE, Positive), ("a", FILE, NONE)]);
  }

  #[test]
  fn never_matches_invalid_patterns() {
    for pattern in [
      "[a",
      "x[",
      "[]",
      "[!]",
      "x*[a",
      "[a]*[b",
      "[a\\]",
      "[[:foo:]]",
      "[[:foo:]a]",
      "[[:ALPHA:]]",
      "[[:alpha:]",
      "a\\",
      "\\",
    ] {
      let patterns = list(pattern.as_bytes(), false);
      for path in ["a", "[a", "x", "x[", "xa", "[]", "f", "]", "a\\", "\\", "ab", "a[b"] {
        assert_eq!(patterns.matched(path.as_bytes(), FILE), NONE, "{pattern:?} against {path:?}");
      }
    }
  }

  #[test]
  fn escapes_special_characters() {
    check("\\*", &[("*", FILE, Positive), ("a", FILE, NONE)]);
    check("\\?", &[("?", FILE, Positive), ("a", FILE, NONE)]);
    check("\\[a]", &[("[a]", FILE, Positive), ("a", FILE, NONE)]);
    check("\\a", &[("a", FILE, Positive), ("\\a", FILE, NONE)]);
    check("a\\\\", &[("a\\", FILE, Positive)]);
    check("\\#a", &[("#a", FILE, Positive)]);
    check("\\!a", &[("!a", FILE, Positive), ("a", FILE, NONE)]);
    check("a/\\*", &[("a/*", FILE, Positive), ("a/b", FILE, NONE)]);
  }

  #[test]
  fn matches_braces_literally() {
    check("*.{ts,js}", &[("a.{ts,js}", FILE, Positive), ("a.ts", FILE, NONE)]);
  }

  #[test]
  fn skips_comments_and_blank_lines() {
    check("#a\n \n", &[("#a", FILE, NONE), (" ", FILE, NONE)]);
    check(" #a", &[(" #a", FILE, Positive), ("#a", FILE, NONE)]);
    check("a#b", &[("a#b", FILE, Positive)]);
  }

  #[test]
  fn trims_unescaped_trailing_spaces() {
    check("a  ", &[("a", FILE, Positive), ("a ", FILE, NONE)]);
    check("a\\  ", &[("a ", FILE, Positive), ("a", FILE, NONE)]);
    check("a \\ ", &[("a  ", FILE, Positive)]);
    check("a\\ \\ ", &[("a  ", FILE, Positive)]);
    check("a\\\\ ", &[("a\\", FILE, Positive)]);
    check("a\\\\\\ ", &[("a\\ ", FILE, Positive)]);
    check("\\ ", &[(" ", FILE, Positive)]);
    check(" a", &[(" a", FILE, Positive), ("a", FILE, NONE)]);
    check("a\t", &[("a\t", FILE, Positive), ("a", FILE, NONE)]);
    check("a b", &[("a b", FILE, Positive)]);
  }

  #[test]
  fn strips_one_trailing_carriage_return() {
    check("a\r\nb\r", &[("a", FILE, Positive), ("b", FILE, Positive)]);
    check("a\r\r\n", &[("a\r", FILE, Positive), ("a", FILE, NONE)]);
    check("a \r", &[("a", FILE, Positive)]);
    check("a\r \n", &[("a\r", FILE, Positive)]);
    check("a\rb", &[("a\rb", FILE, Positive), ("a", FILE, NONE)]);
    check("a\\\r", &[("a", FILE, NONE), ("a\\", FILE, NONE), ("a\r", FILE, NONE)]);
    check("\r\n", &[("\r", FILE, NONE)]);
  }

  #[test]
  fn ignores_a_leading_byte_order_mark() {
    check("\u{feff}a", &[("a", FILE, Positive)]);
    check("\u{feff}#a", &[("#a", FILE, NONE)]);
    check("x\n\u{feff}a", &[("\u{feff}a", FILE, Positive), ("a", FILE, NONE)]);
    check("\u{feff}\u{feff}a", &[("\u{feff}a", FILE, Positive)]);
    let mut patterns = PatternList::new(false);
    patterns.add_line("\u{feff}a".as_bytes());
    assert_eq!(patterns.matched("\u{feff}a".as_bytes(), FILE), Positive);
  }

  #[test]
  fn negates_patterns() {
    check("!a", &[("a", FILE, Negative), ("b", FILE, NONE)]);
    check("!!a", &[("!a", FILE, Negative), ("a", FILE, NONE)]);
    check("!\\ ", &[(" ", FILE, Negative)]);
    check("!", &[("a", FILE, NONE)]);
    check("!/", &[("a", DIR, NONE)]);
    check("*\n!a/", &[("a", DIR, Negative), ("a", FILE, Positive), ("b", DIR, Positive)]);
  }

  #[test]
  fn decides_by_the_last_matching_pattern() {
    check(
      "a\nb\n!a\n!*.c\na.c",
      &[
        ("a", FILE, Negative),
        ("b", FILE, Positive),
        ("a.c", FILE, Positive),
        ("b.c", FILE, Negative),
        ("c", FILE, NONE),
      ],
    );
    check(
      "/*\n!/foo\n/foo/*\n!/foo/bar",
      &[
        ("x", FILE, Positive),
        ("foo", DIR, Negative),
        ("foo/x", FILE, Positive),
        ("foo/bar", DIR, Negative),
      ],
    );
  }

  #[test]
  fn folds_ascii_case() {
    check_ignore_case(
      "ABC\nX/abc\n*.TS\nFoo/\nÉ",
      &[
        ("abc", FILE, Positive),
        ("aBc", FILE, Positive),
        ("x/ABC", FILE, Positive),
        ("a.ts", FILE, Positive),
        ("b/a.Ts", FILE, Positive),
        ("foo", DIR, Positive),
        ("é", FILE, NONE),
        ("É", FILE, Positive),
      ],
    );
    check_ignore_case("[[:upper:]]", &[("a", FILE, Positive), ("A", FILE, Positive)]);
    check_ignore_case("[A-C]", &[("b", FILE, Positive), ("d", FILE, NONE)]);
    check_ignore_case(
      "[Z-a]",
      &[("_", FILE, Positive), ("z", FILE, Positive), ("A", FILE, Positive), ("b", FILE, NONE)],
    );
    check_ignore_case("[!a]", &[("A", FILE, NONE), ("b", FILE, Positive)]);
    check_ignore_case("[!A-Z]", &[("a", FILE, NONE), ("1", FILE, Positive)]);
    check_ignore_case("/Src/**", &[("src/a", FILE, Positive), ("SRC/A/B", FILE, Positive)]);
    check_ignore_case("a/B/c", &[("A/b/C", FILE, Positive)]);
    check_ignore_case("a*B", &[("AxB", FILE, Positive), ("axb", FILE, Positive)]);
    check_ignore_case("AB*", &[("abc", FILE, Positive), ("x/aBc", FILE, Positive)]);
    check("ABC", &[("abc", FILE, NONE)]);
  }

  #[test]
  fn folds_only_the_path_byte_for_escaped_and_bracketed_characters() {
    check_ignore_case("\\a", &[("a", FILE, Positive), ("A", FILE, Positive)]);
    check_ignore_case("\\A", &[("a", FILE, NONE), ("A", FILE, NONE)]);
    check_ignore_case("a\\A", &[("aA", FILE, NONE), ("aa", FILE, NONE)]);
    check_ignore_case("\\x\\y", &[("XY", FILE, Positive)]);
    check_ignore_case("/\\A/b", &[("A/b", FILE, NONE), ("a/b", FILE, NONE)]);
    check_ignore_case("*\\Ab", &[("xAb", FILE, NONE)]);
    check_ignore_case("[\\a]", &[("a", FILE, Positive), ("A", FILE, Positive), ("b", FILE, NONE)]);
    check_ignore_case("[A]", &[("a", FILE, NONE), ("A", FILE, NONE)]);
    check_ignore_case("[\\A]", &[("a", FILE, NONE), ("A", FILE, NONE)]);
    check_ignore_case("[!A]", &[("a", FILE, Positive), ("A", FILE, Positive)]);
    check_ignore_case("[\\A-\\C]", &[("a", FILE, Positive), ("B", FILE, Positive), ("x", FILE, NONE)]);
    check_ignore_case("[a-\\C]", &[("a", FILE, Positive), ("A", FILE, Positive), ("b", FILE, NONE)]);
    check("\\A", &[("A", FILE, Positive), ("a", FILE, NONE)]);
    check("[A]", &[("A", FILE, Positive), ("a", FILE, NONE)]);
  }

  #[test]
  fn matches_bytes() {
    check("?", &[("é", FILE, NONE)]);
    check("??", &[("é", FILE, Positive)]);
    check("[!a][!a]", &[("é", FILE, Positive)]);
    check("[[:alpha:]]?", &[("é", FILE, NONE)]);
    check_bytes(b"[\xc3-\xc4]?", false, &[("é", FILE, Positive)]);
    let mut patterns = PatternList::new(false);
    patterns.add_line(b"a\xff*");
    assert_eq!(patterns.matched(b"x/a\xffb", FILE), Positive);
    assert_eq!(patterns.matched(b"a\xfe", FILE), NONE);
  }

  #[test]
  fn matches_dprint_globs() {
    check(
      "**/*.ts\n/src/**\n/sub/file.ts\n!**/vendor/**\nfoo/",
      &[
        ("a/b.ts", FILE, Positive),
        ("src/x/y.json", FILE, Positive),
        ("sub/file.ts", FILE, Positive),
        ("x/vendor/a.ts", FILE, Negative),
        ("vendor/a", DIR, Negative),
        ("vendor", DIR, NONE),
        ("x/foo", DIR, Positive),
        ("x/foo", FILE, NONE),
        ("sub/file.json", FILE, NONE),
      ],
    );
  }

  #[test]
  fn matches_in_polynomial_time() {
    let text = "a".repeat(2000);
    check(&format!("{}b", "*a".repeat(40)), &[(&text, FILE, NONE)]);
    check(&format!("{}*", "*a".repeat(40)), &[(&text, FILE, Positive)]);
    let deep = vec!["a"; 400].join("/");
    check(&format!("/{}b", "**/a/".repeat(30)), &[(&deep, FILE, NONE)]);
    check(&format!("/{}a", "**/a/".repeat(30)), &[(&deep, FILE, Positive)]);
  }

  struct Rng(u64);

  impl Rng {
    fn next(&mut self) -> u64 {
      self.0 ^= self.0 << 13;
      self.0 ^= self.0 >> 7;
      self.0 ^= self.0 << 17;
      self.0
    }

    fn below(&mut self, bound: usize) -> usize {
      usize::try_from(self.next() % u64::try_from(bound).unwrap()).unwrap()
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
      items[self.below(items.len())]
    }
  }

  const ATOMS: &[(&str, &[&str])] = &[
    ("a", &["a", "A"]),
    ("b", &["b"]),
    ("A", &["A", "a"]),
    ("x", &["x"]),
    (".", &["."]),
    ("-", &["-"]),
    ("*", &["", "a", "ab", "b.a"]),
    ("**", &["", "a", "a/b"]),
    ("***", &["", "a", "a/b"]),
    ("?", &["a", "."]),
    ("/", &["/"]),
    ("[ab]", &["a", "b"]),
    ("[!a]", &["b", "A"]),
    ("[^b]", &["a", "b"]),
    ("[a-b]", &["b", "B"]),
    ("[A-Z]", &["Q", "q"]),
    ("[]a]", &["]", "a"]),
    ("[a-]", &["-"]),
    ("[b-a]", &["b", "a"]),
    ("[[:upper:]]", &["A", "a"]),
    ("[[:alpha:]]", &["x"]),
    ("[[:punct:]]", &[".", "#"]),
    ("[\\A]", &["A", "a"]),
    ("[A]", &["A", "a"]),
    ("\\a", &["a", "A"]),
    ("\\A", &["A", "a"]),
    ("\\[", &["["]),
    ("\\/", &["/"]),
    ("\\*", &["x"]),
    ("[", &["["]),
    ("]", &["]"]),
    ("\\", &["x"]),
    (" ", &[" "]),
    ("\\ ", &[" "]),
    ("#", &["#"]),
    ("!", &["!"]),
    ("{a,b}", &["{a,b}", "a"]),
  ];

  const RARE_ATOMS: &[(&str, &[&str])] = &[
    ("[[:", &["["]),
    ("[!]a]", &["b", "]"]),
    ("[--0]", &["-", "0"]),
    ("[a-c-e]", &["e", "-", "d"]),
    ("[\\]]", &["]"]),
    ("[[:digit:]-z]", &["-", "z"]),
    ("[[.a.]]", &["a]"]),
    ("[[:foo:]]", &["f"]),
    ("**/", &["", "a/"]),
    ("/**/", &["/", "/a/"]),
    ("\\#", &["#"]),
    ("\\!", &["!"]),
    ("?*", &["a", "ab"]),
    ("\\\\", &["x"]),
  ];

  const NAMES: &[&str] = &[
    "a", "b", "A", "x", "ab", "ba", "aa", "aB", "Ab", "a.b", ".a", "b.a", "a-b", "[a]", "]", "{a,b}", "a b", "#", "!a",
  ];

  fn random_pattern(rng: &mut Rng) -> (String, Vec<String>) {
    let rare = rng.below(4) == 0;
    let prefix = if rare {
      rng.pick(&["\\/", "//"])
    } else {
      rng.pick(&["/", "**/", "", "", ""])
    };
    let suffix = if rare {
      rng.pick(&["\\ ", " \\ ", "\\", "//"])
    } else {
      rng.pick(&["/", "/**", "  ", "", "", ""])
    };
    let atoms = (0..=rng.below(4))
      .map(|_| {
        let atoms = if rare { RARE_ATOMS } else { ATOMS };
        atoms[rng.below(atoms.len())]
      })
      .collect::<Vec<_>>();
    let pattern = format!("{prefix}{}{suffix}", atoms.iter().map(|(atom, _)| *atom).collect::<String>());
    let samples = (0..3)
      .map(|_| {
        let mut sample = String::new();
        if prefix == "**/" {
          sample.push_str(rng.pick(&["", "x/", "a/b/"]));
        }
        for (_, samples) in &atoms {
          sample.push_str(rng.pick(samples));
        }
        if suffix == "/**" {
          sample.push_str(rng.pick(&["/a", "/b/A"]));
        }
        sample
          .split('/')
          .filter(|name| !name.is_empty() && *name != "." && !name.ends_with([' ', '.']))
          .collect::<Vec<_>>()
          .join("/")
      })
      .filter(|sample| !sample.is_empty())
      .collect();
    (pattern, samples)
  }

  fn random_path(rng: &mut Rng, max_depth: usize) -> String {
    let depth = 1 + rng.below(max_depth);
    (0..depth).map(|_| rng.pick(NAMES)).collect::<Vec<_>>().join("/")
  }

  fn line_ending(rng: &mut Rng) -> &'static str {
    if rng.below(4) == 0 { "\r\n" } else { "\n" }
  }

  struct Case {
    dir: String,
    buffer: String,
    patterns: PatternList,
    patterns_ignoring_case: PatternList,
    paths: Vec<(String, bool)>,
    check_ancestors: bool,
  }

  impl Case {
    fn new(dir: String, buffer: String, paths: Vec<(String, bool)>, check_ancestors: bool) -> Self {
      Self {
        dir,
        patterns: list(buffer.as_bytes(), false),
        patterns_ignoring_case: list(buffer.as_bytes(), true),
        buffer,
        paths,
        check_ancestors,
      }
    }

    fn dir(&self, is_dir: bool) -> String {
      format!("{}-{}", self.dir, if is_dir { "dir" } else { "file" })
    }

    fn expected(&self, path: &str, is_dir: bool, ignore_case: bool) -> PatternMatch {
      let patterns = if ignore_case { &self.patterns_ignoring_case } else { &self.patterns };
      if self.check_ancestors {
        for (index, _) in path.match_indices('/') {
          if patterns.matched(&path.as_bytes()[..index], DIR) == Positive {
            return Positive;
          }
        }
      }
      patterns.matched(path.as_bytes(), is_dir)
    }
  }

  fn git(root: &Path) -> Command {
    let mut command = Command::new("git");
    command
      .env_clear()
      .env("PATH", std::env::var_os("PATH").unwrap_or_default())
      .env("HOME", root)
      .env("XDG_CONFIG_HOME", root.join(".config"))
      .env("GIT_CONFIG_NOSYSTEM", "1")
      .env("LC_ALL", "C")
      .current_dir(root);
    command
  }

  #[test]
  fn agrees_with_git_check_ignore() {
    if Command::new("git").arg("--version").output().is_err() {
      return;
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let status = git(root).args(["init", "-q", "repo"]).status().unwrap();
    assert!(status.success());
    let repo = root.join("repo");

    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut cases = Vec::new();
    for index in 0..1500 {
      let mut buffer = String::new();
      if rng.below(10) == 0 {
        buffer.push('\u{feff}');
      }
      let (pattern, samples) = random_pattern(&mut rng);
      if pattern.starts_with('!') {
        buffer.push('\\');
      }
      buffer.push_str(&pattern);
      buffer.push_str(line_ending(&mut rng));
      let mut paths = (0..6).map(|_| (random_path(&mut rng, 4), rng.below(2) == 0)).collect::<Vec<_>>();
      for sample in samples {
        paths.extend([(sample.clone(), false), (sample, true)]);
      }
      cases.push(Case::new(format!("single{index}"), buffer, paths, true));
    }
    for index in 0..400 {
      let mut buffer = String::new();
      let mut paths = (0..6).map(|_| (random_path(&mut rng, 1), rng.below(2) == 0)).collect::<Vec<_>>();
      for _ in 0..2 + rng.below(4) {
        if rng.below(3) == 0 {
          buffer.push('!');
        }
        let (pattern, samples) = random_pattern(&mut rng);
        buffer.push_str(&pattern);
        buffer.push_str(line_ending(&mut rng));
        for sample in samples.into_iter().filter(|sample| !sample.contains('/')) {
          paths.push((sample, rng.below(2) == 0));
        }
      }
      cases.push(Case::new(format!("list{index}"), buffer, paths, false));
    }

    let mut queries = Vec::new();
    for case in &cases {
      for is_dir in [false, true] {
        let dir = repo.join(case.dir(is_dir));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".gitignore"), &case.buffer).unwrap();
      }
      for (path, is_dir) in &case.paths {
        if *is_dir {
          std::fs::create_dir_all(repo.join(case.dir(true)).join(path)).unwrap();
        }
        queries.extend_from_slice(format!("{}/{path}\0", case.dir(*is_dir)).as_bytes());
      }
    }
    let queries_path = root.join("queries");
    std::fs::write(&queries_path, &queries).unwrap();

    let mut mismatches = Vec::new();
    let mut matches = 0;
    let mut total = 0;
    for ignore_case in [false, true] {
      let output = git(&repo)
        .arg("-c")
        .arg(format!("core.ignoreCase={ignore_case}"))
        .args(["check-ignore", "--no-index", "--stdin", "-z", "--verbose", "--non-matching"])
        .stdin(Stdio::from(std::fs::File::open(&queries_path).unwrap()))
        .output()
        .unwrap();
      assert!(matches!(output.status.code(), Some(0 | 1)), "{}", String::from_utf8_lossy(&output.stderr));
      let mut fields = output.stdout.split(|&byte| byte == 0);
      for case in &cases {
        for (path, is_dir) in &case.paths {
          let (Some(source), Some(_), Some(pattern), Some(_)) = (fields.next(), fields.next(), fields.next(), fields.next()) else {
            panic!("git printed fewer records than queries");
          };
          let actual = match (source.is_empty(), pattern.first()) {
            (true, _) => NONE,
            (false, Some(b'!')) => Negative,
            (false, _) => Positive,
          };
          let expected = case.expected(path, *is_dir, ignore_case);
          total += 1;
          if actual != NONE {
            matches += 1;
          }
          if actual != expected {
            mismatches.push(format!(
              "{:?} (ignore case: {ignore_case}) against {path:?} (dir: {is_dir}): git {actual:?}, ours {expected:?}",
              case.buffer
            ));
          }
        }
      }
    }
    assert!(
      mismatches.is_empty(),
      "{} mismatches:\n{}",
      mismatches.len(),
      mismatches[..mismatches.len().min(40)].join("\n")
    );
    assert!(matches * 6 > total, "only {matches} of {total} queries matched");
  }
}
