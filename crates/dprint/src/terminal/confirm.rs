use anyhow::Result;
use anyhow::bail;
use crossterm::event::Event;
use crossterm::event::KeyCode;

use super::Logger;
use super::LoggerRefreshItemKind;
use super::LoggerTextItem;
use crate::terminal::read_terminal_key_press;

pub use dprint_platform::utils::BasicShowConfirmStrategy;
pub use dprint_platform::utils::ShowConfirmStrategy;

pub fn show_confirm(logger: &Logger, context_name: &str, strategy: &dyn ShowConfirmStrategy) -> Result<bool> {
  let result = loop {
    let text_items = vec![LoggerTextItem::Text(strategy.render(None))];
    logger.set_refresh_item(LoggerRefreshItemKind::Selection, text_items);

    if let Event::Key(key_event) = read_terminal_key_press()? {
      match &key_event.code {
        KeyCode::Char(c) if *c == 'Y' || *c == 'y' => {
          break true;
        }
        KeyCode::Char(c) if *c == 'N' || *c == 'n' => {
          break false;
        }
        KeyCode::Enter => {
          break strategy.default_value();
        }
        KeyCode::Esc => {
          logger.remove_refresh_item(LoggerRefreshItemKind::Selection);
          bail!("Confirmation cancelled.");
        }
        _ => {}
      }
    } else {
      // cause a refresh anyway
    }
  };
  logger.remove_refresh_item(LoggerRefreshItemKind::Selection);

  logger.log_text_items(&[LoggerTextItem::Text(strategy.render(Some(result)))], context_name);

  Ok(result)
}
