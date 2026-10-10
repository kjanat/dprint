#[derive(Debug, Default, PartialEq, Eq)]
pub struct RepoConfig {
  pub ignore_case: bool,
  pub bare: bool,
  pub has_work_tree_setting: bool,
  pub has_includes: bool,
  pub has_worktree_config: bool,
  pub object_format: Option<String>,
}

pub fn parse_repo_config(text: &str) -> RepoConfig {
  let mut config = RepoConfig::default();
  let bytes = text.strip_prefix('\u{feff}').unwrap_or(text).as_bytes();
  let mut parser = Parser { bytes, position: 0 };
  let mut section: Option<Section> = None;
  while let Some(byte) = parser.peek() {
    match byte {
      b' ' | b'\t' | b'\n' => parser.position += 1,
      b'\r' if parser.peek_at(1) == Some(b'\n') => parser.position += 2,
      b'#' | b';' => parser.skip_line(),
      b'[' => {
        section = parser.section_header();
        if section.is_none() {
          parser.skip_line();
        }
      }
      byte if byte.is_ascii_alphabetic() => match parser.variable() {
        Some((name, value)) => {
          if let Some(section) = &section {
            apply(&mut config, section, &name, value.as_deref());
          }
        }
        None => parser.skip_line(),
      },
      _ => parser.skip_line(),
    }
  }
  config
}

struct Section {
  name: String,
  has_subsection: bool,
}

fn apply(config: &mut RepoConfig, section: &Section, name: &str, value: Option<&[u8]>) {
  match (section.name.as_str(), section.has_subsection, name) {
    ("core", false, "ignorecase") => config.ignore_case = parse_bool(value),
    ("core", false, "bare") => config.bare = parse_bool(value),
    ("core", false, "worktree") => config.has_work_tree_setting = true,
    ("include" | "includeif", _, "path") => config.has_includes = true,
    ("extensions", false, "worktreeconfig") => config.has_worktree_config = parse_bool(value),
    ("extensions", false, "objectformat") => {
      config.object_format = Some(String::from_utf8_lossy(value.unwrap_or_default()).into_owned());
    }
    _ => {}
  }
}

/// Git refuses to run with an invalid boolean.
fn parse_bool(value: Option<&[u8]>) -> bool {
  let Some(value) = value else {
    return true;
  };
  match value.to_ascii_lowercase().as_slice() {
    b"true" | b"yes" | b"on" => true,
    b"false" | b"no" | b"off" | b"" => false,
    _ => parse_integer(value).is_none_or(|integer| integer != 0),
  }
}

fn parse_integer(value: &[u8]) -> Option<i64> {
  let (negative, value) = match value.split_first()? {
    (b'-', rest) => (true, rest),
    (b'+', rest) => (false, rest),
    _ => (false, value),
  };
  let (radix, value) = if let Some(hex) = value.strip_prefix(b"0x").or_else(|| value.strip_prefix(b"0X")) {
    (16, hex)
  } else if value.first() == Some(&b'0') {
    (8, value)
  } else {
    (10, value)
  };
  let digit_count = value.iter().take_while(|byte| char::from(**byte).is_digit(radix)).count();
  let (digits, unit) = value.split_at(digit_count);
  if digits.is_empty() {
    return None;
  }
  let magnitude = digits.iter().try_fold(0i64, |number, byte| {
    number.checked_mul(i64::from(radix))?.checked_add(i64::from(char::from(*byte).to_digit(radix)?))
  })?;
  let scale: i64 = match unit {
    b"" => 1,
    b"k" | b"K" => 1 << 10,
    b"m" | b"M" => 1 << 20,
    b"g" | b"G" => 1 << 30,
    _ => return None,
  };
  let integer = magnitude.checked_mul(scale)?;
  Some(if negative { -integer } else { integer })
}

struct Parser<'a> {
  bytes: &'a [u8],
  position: usize,
}

impl Parser<'_> {
  fn peek(&self) -> Option<u8> {
    self.peek_at(0)
  }

  fn peek_at(&self, offset: usize) -> Option<u8> {
    self.bytes.get(self.position + offset).copied()
  }

  fn next(&mut self) -> Option<u8> {
    let byte = self.peek()?;
    self.position += 1;
    Some(byte)
  }

  fn next_in_line(&mut self) -> Option<u8> {
    if self.peek()? == b'\n' {
      return None;
    }
    self.next()
  }

  fn skip_line(&mut self) {
    while let Some(byte) = self.next() {
      if byte == b'\n' {
        break;
      }
    }
  }

  fn skip_blanks(&mut self) {
    while matches!(self.peek(), Some(b' ' | b'\t')) {
      self.position += 1;
    }
  }

  fn at_line_end(&self) -> bool {
    match self.peek() {
      None | Some(b'\n') => true,
      Some(b'\r') => self.peek_at(1) == Some(b'\n'),
      _ => false,
    }
  }

  fn take_while(&mut self, accept: impl Fn(u8) -> bool) -> &[u8] {
    let start = self.position;
    while self.peek().is_some_and(&accept) {
      self.position += 1;
    }
    &self.bytes[start..self.position]
  }

  fn section_header(&mut self) -> Option<Section> {
    self.position += 1;
    let name = self.take_while(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'.');
    if name.is_empty() {
      return None;
    }
    let name = String::from_utf8_lossy(name).to_ascii_lowercase();
    match self.next_in_line()? {
      b']' => Some(match name.split_once('.') {
        Some((name, _)) => Section {
          name: name.to_string(),
          has_subsection: true,
        },
        None => Section { name, has_subsection: false },
      }),
      b' ' | b'\t' if !name.contains('.') => {
        self.skip_blanks();
        if self.next_in_line()? != b'"' {
          return None;
        }
        loop {
          match self.next_in_line()? {
            b'"' => break,
            b'\\' => {
              self.next_in_line()?;
            }
            _ => {}
          }
        }
        (self.next_in_line()? == b']').then_some(Section { name, has_subsection: true })
      }
      _ => None,
    }
  }

  fn variable(&mut self) -> Option<(String, Option<Vec<u8>>)> {
    let name = self.take_while(|byte| byte.is_ascii_alphanumeric() || byte == b'-').to_ascii_lowercase();
    let name = String::from_utf8(name).ok()?;
    self.skip_blanks();
    if self.at_line_end() {
      self.skip_line();
      return Some((name, None));
    }
    match self.next()? {
      b'#' | b';' => {
        self.skip_line();
        Some((name, None))
      }
      b'=' => {
        let value = self.value()?;
        Some((name, Some(value)))
      }
      _ => None,
    }
  }

  fn value(&mut self) -> Option<Vec<u8>> {
    self.skip_blanks();
    let mut value = Vec::new();
    let mut kept_len = 0;
    let mut quoted = false;
    loop {
      if self.at_line_end() {
        if quoted {
          return None;
        }
        self.skip_line();
        break;
      }
      let Some(byte) = self.next() else {
        break;
      };
      match byte {
        b'\\' => {
          if self.at_line_end() {
            self.skip_line();
            continue;
          }
          value.push(match self.next()? {
            b'\\' => b'\\',
            b'"' => b'"',
            b'n' => b'\n',
            b't' => b'\t',
            b'b' => 0x08,
            _ => return None,
          });
        }
        b'"' => {
          quoted = !quoted;
          continue;
        }
        b'#' | b';' if !quoted => {
          self.skip_line();
          break;
        }
        b' ' | b'\t' if !quoted => {
          value.push(byte);
          continue;
        }
        _ => value.push(byte),
      }
      kept_len = value.len();
    }
    value.truncate(kept_len);
    Some(value)
  }
}

#[cfg(test)]
mod test {
  use super::*;
  use crate::test_git::TempRepo;

  #[test]
  fn reads_the_settings_dprint_needs() {
    let config = parse_repo_config(
      "[core]\n\trepositoryformatversion = 1\n\tignoreCase = true\n\tbare = false\n\tworktree = ../wt\n[extensions]\n\tobjectFormat = sha256\n\tworktreeConfig = true\n[include]\n\tpath = other.inc\n",
    );
    assert_eq!(
      config,
      RepoConfig {
        ignore_case: true,
        bare: false,
        has_work_tree_setting: true,
        has_includes: true,
        has_worktree_config: true,
        object_format: Some("sha256".to_string()),
      }
    );
    assert_eq!(parse_repo_config(""), RepoConfig::default());
  }

  #[test]
  fn reads_what_git_init_writes() {
    let config = parse_repo_config("[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n\tlogallrefupdates = true\n");
    assert_eq!(config, RepoConfig::default());
  }

  #[test]
  fn ignores_case_in_section_and_variable_names() {
    let config = parse_repo_config("[CORE]\n\tBARE = true\n\tIgnoreCase\n[Extensions]\n\tOBJECTFORMAT = sha1\n");
    assert!(config.bare);
    assert!(config.ignore_case);
    assert_eq!(config.object_format.as_deref(), Some("sha1"));
  }

  #[test]
  fn uses_the_last_value() {
    assert!(!parse_repo_config("[core]\n\tbare = true\n[core]\n\tbare = off\n").bare);
    assert!(parse_repo_config("[core]\n\tbare = no\n\tbare\n").bare);
    let config = parse_repo_config("[extensions]\n\tobjectFormat = sha256\n\tobjectFormat = sha1\n");
    assert_eq!(config.object_format.as_deref(), Some("sha1"));
  }

  #[test]
  fn reads_booleans() {
    for (value, expected) in [
      ("true", true),
      ("TRUE", true),
      ("yes", true),
      ("On", true),
      ("1", true),
      ("false", false),
      ("No", false),
      ("oFf", false),
      ("0", false),
      ("", false),
      ("\"\"", false),
      ("\"1\"", true),
      ("2", true),
      ("-1", true),
      ("+1", true),
      ("-0", false),
      ("010", true),
      ("00", false),
      ("0x1", true),
      ("0x0", false),
      ("1k", true),
      ("0K", false),
      ("1g", true),
      ("0 # comment", false),
      ("maybe", true),
      ("\" yes\"", true),
      ("1x", true),
      ("1t", true),
      ("0x", true),
      ("99999999999999999999", true),
      ("8589934592g", true),
    ] {
      let config = parse_repo_config(&format!("[core]\n\tbare = {value}\n"));
      assert_eq!(config.bare, expected, "{value:?}");
    }
    assert!(parse_repo_config("[core]\n\tbare\n").bare);
    assert!(parse_repo_config("[core]\n\tbare ; comment\n").bare);
  }

  #[test]
  fn needs_the_exact_section() {
    for text in [
      "[core \"x\"]\n\tbare = true\n",
      "[core \"\"]\n\tbare = true\n",
      "[core.x]\n\tbare = true\n",
      "[cores]\n\tbare = true\n",
      "bare = true\n",
      "[extensions \"x\"]\n\tworktreeConfig = true\n",
    ] {
      assert_eq!(parse_repo_config(text), RepoConfig::default(), "{text:?}");
    }
  }

  #[test]
  fn detects_includes() {
    for text in [
      "[include]\n\tpath = a\n",
      "[includeIf \"gitdir:/x/\"]\n\tpath = a\n",
      "[IncludeIf \"onbranch:main\"]\n\tPATH = a\n",
    ] {
      assert!(parse_repo_config(text).has_includes, "{text:?}");
    }
    for text in ["[include]\n\tother = a\n", "[includes]\n\tpath = a\n", "[core]\n\tpath = a\n"] {
      assert!(!parse_repo_config(text).has_includes, "{text:?}");
    }
  }

  #[test]
  fn reads_the_rest_of_a_section_header_line() {
    assert!(parse_repo_config("[core] bare = true\n").bare);
    assert!(parse_repo_config("[core]bare\n").bare);
    assert!(parse_repo_config("[core] ; comment\n\tbare = true # comment\n").bare);
  }

  #[test]
  fn handles_crlf_line_ends() {
    let config = parse_repo_config("[core]\r\n\tbare = false\r\n\tignorecase = true\r\n[extensions]\r\n\tobjectformat = sha256\r\n");
    assert!(!config.bare);
    assert!(config.ignore_case);
    assert_eq!(config.object_format.as_deref(), Some("sha256"));
  }

  #[test]
  fn parses_quotes_escapes_and_continuations() {
    let value = |text: &str| parse_repo_config(&format!("[extensions]\n\tobjectFormat = {text}\n")).object_format;
    assert_eq!(value("a\\\n b").as_deref(), Some("a b"));
    assert_eq!(value("a\\\r\n b").as_deref(), Some("a b"));
    assert_eq!(value("\"a ; b\" c\\t\\\"d\\\\ ").as_deref(), Some("a ; b c\t\"d\\"));
    assert_eq!(value("a b   ").as_deref(), Some("a b"));
    assert_eq!(value("\"a b   \"  ").as_deref(), Some("a b   "));
    assert_eq!(value("a\rb").as_deref(), Some("a\rb"));
    assert_eq!(value("\"a\\nb\"").as_deref(), Some("a\nb"));
    assert_eq!(value("x\\b").as_deref(), Some("x\u{8}"));
    assert_eq!(value("true [x]").as_deref(), Some("true [x]"));
    assert_eq!(value("sha1#comment").as_deref(), Some("sha1"));
    assert_eq!(parse_repo_config("[extensions]\n\tobjectFormat = a\\").object_format.as_deref(), Some("a"));
    assert_eq!(parse_repo_config("[extensions]\n\tobjectFormat").object_format.as_deref(), Some(""));
  }

  #[test]
  fn ignores_lines_it_cannot_parse() {
    let config = parse_repo_config("[core]\n\tbare = true\n\tbare = a\\qb\n\tbare = \"open\n!!!\n\tba_re = false\n\t1bare = false\n");
    assert!(config.bare);
    let config = parse_repo_config("[ core ]\n\tbare = true\n[core  \"x\" ]\n\tignorecase = true\n[core\n\tbare = true\n");
    assert_eq!(config, RepoConfig::default());
    assert!(parse_repo_config("[core]\n\tbare = false\n[bad\n[core]\n\tbare = true\n").bare);
  }

  #[test]
  fn reads_subsection_escapes() {
    let config = parse_repo_config("[includeIf \"gitdir:/a\\\"b\\\\c\"]\n\tpath = x\n[core]\n\tbare = true\n");
    assert!(config.has_includes);
    assert!(config.bare);
  }

  #[test]
  fn skips_a_byte_order_mark() {
    assert!(parse_repo_config("\u{feff}[core]\n\tbare = true\n").bare);
  }

  #[test]
  fn never_panics() {
    let text = "[core \"a\\\"b\"]\n\tbare = \"x\\\" ; y\" # z\\\n[extensions]\r\n\tobjectFormat = sha256\\\r\n";
    for end in 0..=text.len() {
      if text.is_char_boundary(end) {
        parse_repo_config(&text[..end]);
      }
    }
    for text in ["[", "[\"", "[a \"", "[a \"\\", "a", "a =", "a = \"", "a = \\", "\r", "[a]\r", "[a]\na\r"] {
      parse_repo_config(text);
    }
  }

  fn git_bool(repo: &TempRepo, key: &str) -> Option<bool> {
    let output = repo.try_git(&["config", "-f", "probe", "--type=bool", "--get", key]);
    match output.stdout.as_slice() {
      b"true\n" => Some(true),
      b"false\n" => Some(false),
      _ => None,
    }
  }

  #[test]
  fn agrees_with_git_config() {
    let Some(repo) = TempRepo::new(&[]) else {
      return;
    };
    for text in [
      "[core]\n\tbare = true\n",
      "[core]\n\tbare = 2\n",
      "[core]\n\tbare = 0x0\n",
      "[core]\n\tbare = 1k\n",
      "[core]\n\tbare = \"0\"\n",
      "[core]\n\tbare\n",
      "[core]\n\tbare =\n",
      "[core]\r\n\tbare = false\r\n",
      "[CORE]\n\tBare = off\n",
      "[core \"x\"]\n\tbare = true\n",
      "[core.x]\n\tbare = true\n",
      "[core] bare = yes ; comment\n",
      "[core]\n\tbare = tr\\\nue\n",
      "[core]\n\tbare = \"tr\"ue\n",
      "[core]\n\tbare = true\n[core]\n\tbare = false\n",
      "[core]\n\tignorecase = true\n\tignorecase = no\n",
    ] {
      repo.write("probe", text.as_bytes());
      let config = parse_repo_config(text);
      assert_eq!(git_bool(&repo, "core.bare").unwrap_or(false), config.bare, "{text:?}");
      assert_eq!(git_bool(&repo, "core.ignorecase").unwrap_or(false), config.ignore_case, "{text:?}");
    }
  }
}
