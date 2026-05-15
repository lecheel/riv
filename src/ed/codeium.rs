use crate::ed::ghost_text::GhostTextExt;
use crate::CommandResult;
use crate::Editor;
use crate::Mode;

impl Editor {
    pub fn poll_codeium_ghost(&mut self) {
        if !self.ghost_text.is_pending() {
            return;
        }

        if let Some(result) = self.codeium.poll() {
            match result {
                Ok(completion) => {
                    self.process_codeium_ghost(completion);
                }
                Err(_e) => {
                    self.ghost_text.clear();
                    self.codeium.is_connected = false;
                }
            }
        }
    }

    fn handle_codeium_auth_submit(&mut self) -> CommandResult {
        self.codeium_auth_pending = false;
        let input = self.command_prompt.text().trim().to_string();
        self.command_prompt.clear();
        self.mode = Mode::Normal;
        self.dirty.mark_all();

        if input.is_empty() {
            return CommandResult::Message("Codeium auth cancelled".to_string());
        }

        self.set_status("Codeium: exchanging token for API key...".to_string());

        match crate::codeium::exchange_token_for_api_key(&input) {
            Ok(api_key) => {
                self.config.codeium.api_key = Some(api_key);
                self.set_status(
                    "Codeium: API key saved ✓. Run :codeium to start the server.".to_string(),
                );
                CommandResult::Message("Codeium: authenticated successfully!".to_string())
            }
            Err(e) => {
                self.set_infobar_message(format!("Codeium auth failed: {}", e));
                CommandResult::Error(format!("Codeium auth failed: {}", e))
            }
        }
    }
}
