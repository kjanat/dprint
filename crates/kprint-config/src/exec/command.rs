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
