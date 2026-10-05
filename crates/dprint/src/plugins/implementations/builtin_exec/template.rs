//! The variables in an exec command's arguments, ex. `{{file_path}}`.
//!
//! The exec plugin rendered arguments as Handlebars templates, which escaped
//! the values for HTML (ex. a `&` in a file path became `&amp;`). An argument
//! only ever has these variables, so they're substituted as they are instead.

use std::path::Path;

/// What the variables are when a file is formatted.
pub struct TemplateValues<'a> {
  pub file_path: &'a Path,
  pub line_width: u32,
  pub use_tabs: bool,
  pub indent_width: u8,
  pub cwd: &'a Path,
  pub timeout: u32,
}

/// The variables, by name.
const VARIABLES: [&str; 6] = ["file_path", "line_width", "use_tabs", "indent_width", "cwd", "timeout"];

/// Says what's wrong with the variables in an argument, if anything.
pub fn validate_template(argument: &str) -> Result<(), String> {
  substitute(argument, |_| String::new()).map(|_| ())
}

/// The argument with its variables replaced by their values.
pub fn render_template(argument: &str, values: &TemplateValues) -> Result<String, String> {
  substitute(argument, |name| match name {
    "file_path" => values.file_path.to_string_lossy().into_owned(),
    "line_width" => values.line_width.to_string(),
    "use_tabs" => values.use_tabs.to_string(),
    "indent_width" => values.indent_width.to_string(),
    "cwd" => values.cwd.to_string_lossy().into_owned(),
    "timeout" => values.timeout.to_string(),
    _ => unreachable!("only known variables are substituted"),
  })
}

/// Replaces each variable (`{{name}}`, also written `{{ name }}` or
/// `{{{name}}}`) with `value(name)`. `\{{` is a literal `{{`. Anything else
/// in `{{` and `}}` is an error, as the exec plugin's other Handlebars
/// features (ex. helpers) aren't supported.
fn substitute(argument: &str, mut value: impl FnMut(&str) -> String) -> Result<String, String> {
  let mut result = String::with_capacity(argument.len());
  let mut rest = argument;
  while let Some(start) = rest.find("{{") {
    let (text, variable) = rest.split_at(start);
    if let Some(text) = text.strip_suffix('\\') {
      result.push_str(text);
      result.push_str("{{");
      rest = &variable[2..];
      continue;
    }
    result.push_str(text);
    let (opening, closing) = if variable.starts_with("{{{") { ("{{{", "}}}") } else { ("{{", "}}") };
    let inner = &variable[opening.len()..];
    let Some(end) = inner.find(closing) else {
      return Err(format!("Expected a '{}' to close the '{}'.", closing, opening));
    };
    let name = inner[..end].trim();
    if !VARIABLES.contains(&name) {
      return Err(format!(
        "Unknown variable '{}{}{}'. Expected one of: {}.",
        opening,
        &inner[..end],
        closing,
        VARIABLES.join(", ")
      ));
    }
    result.push_str(&value(name));
    rest = &inner[end + closing.len()..];
  }
  result.push_str(rest);
  Ok(result)
}

#[cfg(test)]
mod test {
  use std::path::Path;

  use super::*;

  fn render(argument: &str, file_path: &str) -> Result<String, String> {
    render_template(
      argument,
      &TemplateValues {
        file_path: Path::new(file_path),
        line_width: 120,
        use_tabs: false,
        indent_width: 2,
        cwd: Path::new("/project"),
        timeout: 30,
      },
    )
  }

  #[test]
  fn substitutes_each_variable() {
    assert_eq!(
      render("{{file_path}} {{line_width}} {{use_tabs}} {{indent_width}} {{cwd}} {{timeout}}", "/file.ts").unwrap(),
      "/file.ts 120 false 2 /project 30"
    );
    assert_eq!(render("--stdin-filepath={{file_path}}", "/file.ts").unwrap(), "--stdin-filepath=/file.ts");
    // as Handlebars allowed
    assert_eq!(render("{{ file_path }}", "/file.ts").unwrap(), "/file.ts");
    assert_eq!(render("{{{file_path}}}", "/file.ts").unwrap(), "/file.ts");
    assert_eq!(render("no variables } }} {", "/file.ts").unwrap(), "no variables } }} {");
  }

  #[test]
  fn uses_values_as_they_are() {
    // Handlebars escaped them for HTML
    let file_path = r#"/a&b <"c"> 'd' `e` =f.ts"#;
    assert_eq!(render("{{file_path}}", file_path).unwrap(), file_path);
    assert_eq!(render("{{file_path}}", "/{{cwd}}.ts").unwrap(), "/{{cwd}}.ts");
  }

  #[test]
  fn keeps_escaped_braces() {
    assert_eq!(render(r"\{{file_path}}", "/file.ts").unwrap(), "{{file_path}}");
    assert_eq!(render(r"\{{x}} {{file_path}}", "/file.ts").unwrap(), "{{x}} /file.ts");
  }

  #[test]
  fn errors_for_what_isnt_a_variable() {
    let expected = "Expected one of: file_path, line_width, use_tabs, indent_width, cwd, timeout.";
    for (argument, error) in [
      ("{{filePath}}", format!("Unknown variable '{{{{filePath}}}}'. {}", expected)),
      ("{{ }}", format!("Unknown variable '{{{{ }}}}'. {}", expected)),
      ("{{#if use_tabs}}", format!("Unknown variable '{{{{#if use_tabs}}}}'. {}", expected)),
      ("{{file_path.length}}", format!("Unknown variable '{{{{file_path.length}}}}'. {}", expected)),
      ("{{file_path", "Expected a '}}' to close the '{{'.".to_string()),
      ("{{{file_path}}", "Expected a '}}}' to close the '{{{'.".to_string()),
    ] {
      assert_eq!(validate_template(argument), Err(error.clone()), "{}", argument);
      assert_eq!(render(argument, "/file.ts"), Err(error), "{}", argument);
    }
    assert_eq!(validate_template("{{file_path}}"), Ok(()));
  }
}
