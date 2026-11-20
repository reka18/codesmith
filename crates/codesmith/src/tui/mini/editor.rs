//! `/editor` with text after it, as OpenCode's `resolveEditorSlashValue`
//! (`mini/prompt.editor`): the external editor opens on that text, not on
//! an empty prompt (sending the command cleared it).

use crate::tui::app::App;

impl App {
    /// The prompt `/editor ARGS` leaves for the editor to open on.
    pub fn editor_slash(&mut self, args: &str) {
        if !args.is_empty() {
            self.input.set(args);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::tui::app::{Action, App};
    use ratatui::crossterm::event::{KeyCode, KeyEvent};

    fn run(text: &str) -> String {
        let mut a = App::new();
        a.input.set(text);
        let Some(Action::Slash { name, args }) = a.on_key(KeyEvent::from(KeyCode::Enter)) else {
            panic!("{text:?} is not a command");
        };
        assert_eq!(name, "editor");
        a.editor_slash(&args);
        a.input.text().to_string()
    }

    // mini/prompt.editor.test.ts: "strips the local /editor command from
    // the initial editor text".
    #[test]
    fn oc_editor_opens_on_the_text_after_the_command() {
        assert_eq!(run("/editor"), "");
        assert_eq!(run("/editor draft message"), "draft message");
        assert_eq!(
            run("/editor first line\nsecond line"),
            "first line\nsecond line"
        );
    }
}
