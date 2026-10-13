/// Splits a command into its program and arguments the way the exec plugin
/// always has (with splitty 1.0.1):
///
/// - Parts are separated by spaces. Other whitespace (ex. a tab) is part of
///   a part.
/// - A part that starts with a quote (`"`) goes on to a quote followed by a
///   space or the end, and doesn't include those quotes, so it may contain
///   spaces. Without such a quote, it's the rest of the command as is.
/// - Quotes anywhere else are kept, and empty parts (ex. `""`) are left out.
pub fn split_command(command: &str) -> Vec<String> {
  let mut parts = Vec::new();
  let mut rest = command.trim_start_matches(' ');
  while !rest.is_empty() {
    let (part, after) = split_first_part(rest);
    if !part.is_empty() {
      parts.push(part.to_string());
    }
    rest = after.trim_start_matches(' ');
  }
  parts
}

/// The first part of a command that doesn't start with a space, and what's
/// after it.
fn split_first_part(text: &str) -> (&str, &str) {
  if let Some(quoted) = text.strip_prefix('"') {
    return match quoted.find("\" ") {
      Some(end) => (&quoted[..end], &quoted[end + 1..]),
      None => match quoted.strip_suffix('"') {
        Some(part) => (part, ""),
        // an opening quote without a closing one is kept, as is what follows
        None => (text, ""),
      },
    };
  }
  match text.find(' ') {
    Some(end) => (&text[..end], &text[end..]),
    None => (text, ""),
  }
}
