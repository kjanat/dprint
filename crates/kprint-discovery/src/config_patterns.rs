pub fn process_config_patterns(file_patterns: &[String]) -> impl Iterator<Item = String> + '_ {
  file_patterns.iter().map(|p| process_config_pattern(p))
}

pub fn process_config_pattern(file_pattern: &str) -> String {
  let file_pattern = file_pattern.replace('\\', "/");
  // make config patterns that start with `/` be relative
  if file_pattern.starts_with('/') {
    format!(".{}", file_pattern)
  } else if file_pattern.starts_with("!/") {
    format!("!.{}", &file_pattern[1..])
  } else {
    file_pattern
  }
}
