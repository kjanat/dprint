#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogLevel {
  Error,
  Warn,
  Info,
  Debug,
  Silent,
}

impl LogLevel {
  #[inline]
  pub fn is_debug(&self) -> bool {
    use LogLevel::*;
    matches!(self, Debug)
  }

  #[inline]
  pub fn is_info(&self) -> bool {
    use LogLevel::*;
    matches!(self, Debug | Info)
  }

  #[inline]
  pub fn is_warn(&self) -> bool {
    use LogLevel::*;
    matches!(self, Debug | Info | Warn)
  }

  #[inline]
  pub fn is_error(&self) -> bool {
    use LogLevel::*;
    matches!(self, Debug | Info | Warn | Error)
  }
}

pub trait ShowConfirmStrategy {
  fn render(&self, selected: Option<bool>) -> String;
  fn default_value(&self) -> bool;
}

pub struct BasicShowConfirmStrategy<'a> {
  pub prompt: &'a str,
  pub default_value: bool,
}

impl ShowConfirmStrategy for BasicShowConfirmStrategy<'_> {
  fn render(&self, selected: Option<bool>) -> String {
    match selected {
      Some(value) => {
        format!("{} {}", self.prompt, if value { "Y" } else { "N" })
      }
      None => {
        format!(
          "{} ({}) \u{2588}", // show a cursor (block character)
          self.prompt,
          if self.default_value { "Y/n" } else { "y/N" }
        )
      }
    }
  }

  fn default_value(&self) -> bool {
    self.default_value
  }
}

/// An item in a multi-select prompt.
pub struct MultiSelectItem {
  pub text: String,
  /// Whether the item starts out selected.
  pub is_selected: bool,
  /// Whether the user can toggle the item. A non-selectable item is shown for
  /// context (ex. a plugin that's already in the config file) and is never
  /// part of the result.
  pub is_selectable: bool,
}

impl MultiSelectItem {
  pub fn new(text: String, is_selected: bool) -> Self {
    MultiSelectItem {
      text,
      is_selected,
      is_selectable: true,
    }
  }

  /// An item shown as selected that the user can't toggle.
  pub fn non_selectable(text: String) -> Self {
    MultiSelectItem {
      text,
      is_selected: true,
      is_selectable: false,
    }
  }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ProgressBarStyle {
  Download,
  Action,
}

/// Progress callbacks implemented by a frontend; dropping a handle ends it.
pub trait ProgressHandle: Send + Sync {
  fn set_position(&self, position: usize);
  fn finish(&self);
}
pub trait ProgressReporter: Send + Sync {
  fn add_progress(&self, message: String, style: ProgressBarStyle, total_size: usize) -> Box<dyn ProgressHandle>;
}
