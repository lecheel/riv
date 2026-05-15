//! LLM extension trait for Editor.

use crate::buffer::{BufferId, BufferKind};
use crate::editor::{CommandResult, Editor, Mode};
use crate::llm::{LlmPreset, LlmRole};
use crate::llm_client::{Cancelled, LlmClient};
use crate::llm_session::SessionManager;
use ropey::Rope;
use std::path::PathBuf;

pub trait LlmExt {
    fn llm_open(&mut self) -> CommandResult;
    fn llm_close(&mut self) -> CommandResult;
    fn llm_send(&mut self) -> CommandResult;
    fn llm_cancel(&mut self) -> CommandResult;
    fn llm_clear_history(&mut self) -> CommandResult;
    fn llm_next_preset(&mut self) -> CommandResult;
    fn llm_prev_preset(&mut self) -> CommandResult;
    fn llm_send_from_prompt(&mut self, input: String) -> CommandResult;
    fn ensure_llm_buffer(&mut self) -> BufferId;
    fn poll_llm_responses(&mut self);
    fn spawn_llm_request(&mut self, messages: Vec<(String, String)>) -> CommandResult;
    fn sync_llm_to_buffer(&mut self);

    // Session persistence
    fn llm_session_save(&mut self) -> CommandResult;
    fn llm_session_load(&mut self) -> CommandResult;
    fn llm_session_new(&mut self) -> CommandResult;
    fn llm_session_list(&mut self) -> CommandResult;
    fn llm_session_delete(&mut self) -> CommandResult;
    fn llm_session_switch(&mut self, name: String) -> CommandResult;
    fn auto_save_session(&mut self);
    fn auto_load_session(&mut self);
}

impl LlmExt for Editor {
    fn ensure_llm_buffer(&mut self) -> BufferId {
        if let Some(id) = self.llm_buffer_id {
            if self.buffers.get(&id).is_some() {
                return id;
            }
        }
        let id = self.buffers.new_llm_buffer();
        self.llm_buffer_id = Some(id);
        id
    }

    fn llm_open(&mut self) -> CommandResult {
        let llm_id = self.ensure_llm_buffer();
        if let Some(window) = self.windows.active_window_mut() {
            window.set_buffer(llm_id);
        }
        // Auto-load session on first open
        if self.llm_buffer.messages().is_empty() {
            self.auto_load_session();
        }
        self.sync_llm_to_buffer();
        CommandResult::ViewChanged
    }

    fn llm_close(&mut self) -> CommandResult {
        if self.llm_buffer.state().is_active() {
            self.llm_cancel();
        }
        // Auto-save on close
        self.auto_save_session();

        if let Some(window) = self.windows.active_window() {
            let current_id = window.buffer_id;
            let llm_id = self.llm_buffer_id.unwrap_or(0);
            if current_id == llm_id {
                if let Some(other_id) = self
                    .buffers
                    .iter()
                    .find(|b| b.id != current_id && b.kind != BufferKind::Llm)
                    .map(|b| b.id)
                {
                    if let Some(w) = self.windows.active_window_mut() {
                        w.set_buffer(other_id);
                    }
                }
            }
        }
        self.mode = Mode::Normal;
        self.dirty.mark_all();
        CommandResult::ModeChanged(Mode::Normal)
    }

    fn llm_send(&mut self) -> CommandResult {
        let input = self.llm_buffer.take_input();
        if input.trim().is_empty() {
            return CommandResult::NoOp;
        }

        if !self.config.llm.enabled {
            self.llm_buffer.add_message(
                LlmRole::Error,
                "LLM is not enabled. Set `llm.enabled = true` in config.toml",
            );
            self.sync_llm_to_buffer();
            self.dirty.mark_all();
            return CommandResult::Error("LLM not enabled".to_string());
        }

        // Session-based chat — NOT single-shot
        self.llm_single_shot = false;

        self.llm_buffer.add_message(LlmRole::User, input);
        self.sync_llm_to_buffer();

        let api_messages = self.llm_buffer.build_api_messages();
        self.spawn_llm_request(api_messages)
    }

    fn llm_cancel(&mut self) -> CommandResult {
        if let Some(handle) = self.llm_task_handle.take() {
            handle.abort();
        }
        self.llm_buffer.cancel();
        self.llm_single_shot = false;
        self.llm_infobar_response = false;
        self.sync_llm_to_buffer();
        self.dirty.mark_all();
        CommandResult::Message("Cancelled".to_string())
    }

    fn llm_clear_history(&mut self) -> CommandResult {
        self.llm_buffer.clear_history();
        self.sync_llm_to_buffer();
        self.dirty.mark_all();
        CommandResult::Message("History cleared".to_string())
    }

    fn llm_next_preset(&mut self) -> CommandResult {
        let presets = LlmPreset::all();
        let current = self.llm_buffer.preset();
        let idx = presets.iter().position(|&p| p == current).unwrap_or(0);
        let next = presets[(idx + 1) % presets.len()];
        self.llm_buffer.set_preset(next);
        self.set_status(format!("Preset: {}", next));
        self.dirty.mark_all();
        CommandResult::ViewChanged
    }

    fn llm_prev_preset(&mut self) -> CommandResult {
        let presets = LlmPreset::all();
        let current = self.llm_buffer.preset();
        let idx = presets.iter().position(|&p| p == current).unwrap_or(0);
        let prev = if idx == 0 { presets.len() - 1 } else { idx - 1 };
        self.llm_buffer.set_preset(presets[prev]);
        self.set_status(format!("Preset: {}", presets[prev]));
        self.dirty.mark_all();
        CommandResult::ViewChanged
    }

    /// Session-based prompt: adds user message to conversation history,
    /// builds API payload from full history, and sends.
    fn llm_send_from_prompt(&mut self, input: String) -> CommandResult {
        if input.trim().is_empty() && self.llm_active_context.is_none() {
            return CommandResult::NoOp;
        }

        if !self.config.llm.enabled {
            return CommandResult::Error("LLM not enabled in config".to_string());
        }

        // ── This is a SESSION request, NOT single-shot ──
        self.llm_single_shot = false;

        let _llm_id = self.ensure_llm_buffer();

        if let Some(preset) = self.llm_active_preset.take() {
            self.llm_buffer.set_preset(preset);
        }

        if let Some(ctx) = self.llm_active_context.take() {
            self.llm_buffer.set_selection_context(Some(ctx.clone()));

            let final_msg = if input.trim().is_empty() {
                ctx
            } else if self.llm_todo_prefix {
                format!("{}\n\n##TODO {}", ctx, input)
            } else {
                format!("{}\n\n{}", ctx, input)
            };

            self.llm_todo_prefix = false;
            self.llm_buffer.add_message(LlmRole::User, final_msg);
        } else {
            self.llm_buffer.add_message(LlmRole::User, &input);
        }

        self.sync_llm_to_buffer();

        let api_messages = self.llm_buffer.build_api_messages();
        self.spawn_llm_request(api_messages)
    }

    fn spawn_llm_request(&mut self, messages: Vec<(String, String)>) -> CommandResult {
        let client = match LlmClient::new(&self.config.llm) {
            Ok(c) => c,
            Err(e) => {
                self.llm_single_shot = false;
                self.llm_infobar_response = false;
                self.llm_buffer
                    .set_infobar_message(format!("Failed to create LLM client: {}", e));
                self.sync_llm_to_buffer();
                self.dirty.mark_all();
                return CommandResult::Error("LLM client error".to_string());
            }
        };

        let cancel_flag = self.llm_buffer.cancel_flag();
        let tx = self.llm_response_tx.clone();

        // Track request state in the buffer (used by both paths)
        self.llm_buffer.start_sending();

        // Only sync buffer view for session-based requests
        if !self.llm_single_shot {
            self.sync_llm_to_buffer();
        }

        self.dirty.mark_all();

        let handle = self.llm_runtime.spawn(async move {
            let result = client.chat_with_cancel(messages, cancel_flag).await;
            let _ = tx.send(match result {
                Ok(response) => Ok(response),
                Err(e) => {
                    if Cancelled::is_cancelled(&e) {
                        Err("[cancelled]".to_string())
                    } else {
                        Err(e.to_string())
                    }
                }
            });
        });

        self.llm_task_handle = Some(handle);

        if self.llm_single_shot {
            self.set_status("Sending…".to_string());
        } else {
            self.set_status(format!("Sending... [{}]", self.llm_buffer.session_name()));
        }
        CommandResult::Message("Sending to LLM...".to_string())
    }

    fn poll_llm_responses(&mut self) {
        while let Ok(result) = self.llm_response_rx.try_recv() {
            match result {
                Ok(response) => {
                    self.llm_task_handle = None;

                    // ── Single-shot path: popup display ──
                    if self.llm_infobar_response || self.llm_single_shot {
                        let preset_label = self
                            .llm_active_preset
                            .map(|p| format!("{}", p))
                            .unwrap_or_default();

                        // Reset all single-shot flags
                        self.llm_infobar_response = false;
                        self.llm_single_shot = false;
                        self.llm_active_context = None;
                        self.llm_todo_prefix = false;
                        self.llm_active_preset = None;

                        // Reset buffer state
                        if self.llm_buffer.state().is_active() {
                            self.llm_buffer.finish_streaming();
                        }

                        // Store in named register 'e' for pasting
                        self.named_registers.insert('e', response.clone());
                        self.llm_infobar_accumulator = response.clone();

                        // Show in register-style popup (multiline)
                        let popup_lines: Vec<String> =
                            response.lines().map(|l| l.to_string()).collect();
                        self.register_popup = if popup_lines.is_empty() {
                            Some(vec!["(empty response)".to_string()])
                        } else {
                            Some(popup_lines)
                        };
                        self.register_popup_title = if preset_label.is_empty() {
                            "LLM Response".to_string()
                        } else {
                            preset_label
                        };

                        self.set_status("✓ [\"e to paste]".to_string());
                        self.dirty.mark_all();
                        return;
                    }

                    // ── Session-based buffer path (unchanged) ──
                    if self.llm_buffer.state().is_active() {
                        self.llm_buffer.finish_streaming();
                    }

                    let last_is_assistant = self
                        .llm_buffer
                        .messages()
                        .last()
                        .map(|m| m.role == LlmRole::Assistant)
                        .unwrap_or(false);

                    if !last_is_assistant {
                        self.llm_buffer.add_message(LlmRole::Assistant, response);
                    }

                    self.auto_save_session();
                    self.sync_llm_to_buffer();

                    let viewing_llm =
                        self.windows.active_window().map(|w| w.buffer_id) == self.llm_buffer_id;

                    if viewing_llm {
                        self.set_status(format!("✓ [{}]", self.llm_buffer.session_name()));
                    } else {
                        self.set_status("✓ LLM response (ga i to view)".to_string());
                    }
                    self.dirty.mark_all();
                }
                Err(err) => {
                    self.llm_task_handle = None;

                    // ── Single-shot infobar error path ──
                    if self.llm_infobar_response || self.llm_single_shot {
                        self.llm_infobar_response = false;
                        self.llm_single_shot = false;
                        self.llm_active_context = None;
                        self.llm_todo_prefix = false;
                        self.llm_active_preset = None;
                        self.llm_infobar_accumulator.clear();

                        // Reset buffer state
                        if self.llm_buffer.state().is_active() {
                            self.llm_buffer.set_idle();
                        }

                        if err != "[cancelled]" {
                            self.set_infobar_message(format!("LLM: {}", err));
                        }
                        self.dirty.status_infobar = true;
                        self.dirty.status_cmdline = true;
                        self.dirty.cursor = true;
                        return;
                    }

                    // ── Session error path ──
                    if err == "[cancelled]" {
                        self.llm_buffer.cancel();
                    } else {
                        self.llm_buffer.set_infobar_message(&err);
                        self.set_infobar_message(format!("LLM: {}", err));
                    }

                    self.sync_llm_to_buffer();
                    self.dirty.mark_all();
                }
            }
        }
    }

    // ═══════════════════════════════════════════════════════════════════
    // Session Persistence
    // ═══════════════════════════════════════════════════════════════════

    fn llm_session_save(&mut self) -> CommandResult {
        let mgr = self.session_manager();
        match mgr.save(
            self.llm_buffer.session_name(),
            self.llm_buffer.preset(),
            self.llm_buffer.messages(),
        ) {
            Ok(()) => {
                self.set_status(format!("Saved session: {}", self.llm_buffer.session_name()));
                CommandResult::Message(format!("Saved: {}", self.llm_buffer.session_name()))
            }
            Err(e) => {
                self.set_infobar_message(format!("Save failed: {}", e));
                CommandResult::Error(e)
            }
        }
    }

    fn llm_session_load(&mut self) -> CommandResult {
        self.llm_session_list()
    }

    fn llm_session_new(&mut self) -> CommandResult {
        self.auto_save_session();

        let name = format!("session_{}", Self::timestamp_short());
        self.llm_buffer.clear_history();
        self.llm_buffer.set_session_name(&name);
        self.sync_llm_to_buffer();
        self.set_status(format!("New session: {}", name));
        CommandResult::Message(format!("New session: {}", name))
    }

    fn llm_session_list(&mut self) -> CommandResult {
        let mgr = self.session_manager();
        let sessions = mgr.list();

        if sessions.is_empty() {
            self.set_status("No saved sessions".to_string());
            return CommandResult::Message("No saved sessions".to_string());
        }

        let current = self.llm_buffer.session_name().to_string();
        let mut lines = Vec::new();

        for s in &sessions {
            let marker = if s.name == current { " ●" } else { "  " };
            let preset_str = s.preset.map(|p| format!(" [{p}]")).unwrap_or_default();
            lines.push(format!(
                "{} {}{} ({} msgs)",
                marker, s.name, preset_str, s.message_count
            ));
        }

        let msg = lines.join("\n");
        self.set_status(format!("Sessions (ga ss <name> to switch):\n{}", msg));
        CommandResult::Message(msg)
    }

    fn llm_session_delete(&mut self) -> CommandResult {
        let name = self.llm_buffer.session_name().to_string();
        let mgr = self.session_manager();

        if !self.llm_buffer.messages().is_empty() {
            self.set_infobar_message(format!(
                "Session '{}' has messages. Clear history first, or use a different name.",
                name
            ));
            return CommandResult::NoOp;
        }

        match mgr.delete(&name) {
            Ok(()) => {
                self.llm_buffer.set_session_name("default");
                self.set_status(format!("Deleted session: {}", name));
                CommandResult::Message(format!("Deleted: {}", name))
            }
            Err(e) => {
                self.set_infobar_message(format!("Delete failed: {}", e));
                CommandResult::Error(e)
            }
        }
    }

    fn llm_session_switch(&mut self, name: String) -> CommandResult {
        if name.trim().is_empty() {
            return self.llm_session_list();
        }

        self.auto_save_session();

        let mgr = self.session_manager();
        match mgr.load(&name) {
            Ok((messages, preset)) => {
                self.llm_buffer.clear_history();
                self.llm_buffer.set_preset(preset);
                self.llm_buffer.set_session_name(&name);

                for msg in messages {
                    self.llm_buffer.add_message(msg.role, msg.content);
                }

                self.sync_llm_to_buffer();
                self.set_status(format!("Switched to: {}", name));
                CommandResult::Message(format!("Switched to: {}", name))
            }
            Err(e) => {
                self.set_infobar_message(format!("Load failed: {}", e));
                CommandResult::Error(e)
            }
        }
    }

    fn auto_save_session(&mut self) {
        // Never auto-save single-shot (stateless) requests
        if self.llm_single_shot {
            return;
        }
        if self.llm_buffer.messages().is_empty() {
            return;
        }

        let mgr = self.session_manager();
        if let Err(_e) = mgr.save(
            self.llm_buffer.session_name(),
            self.llm_buffer.preset(),
            self.llm_buffer.messages(),
        ) {}
    }

    fn auto_load_session(&mut self) {
        let mgr = self.session_manager();
        if let Some((name, messages, preset)) = mgr.load_active() {
            self.llm_buffer.set_session_name(&name);
            self.llm_buffer.set_preset(preset);

            for msg in messages {
                self.llm_buffer.add_message(msg.role, msg.content);
            }

            self.sync_llm_to_buffer();
        }
    }

    fn sync_llm_to_buffer(&mut self) {
        let llm_id = match self.llm_buffer_id {
            Some(id) if self.buffers.get(&id).is_some() => id,
            _ => return,
        };

        let mut text = String::new();

        for msg in self.llm_buffer.messages() {
            let header = match msg.role {
                LlmRole::User => "▸ You:",
                LlmRole::Assistant => "◇ AI:",
                LlmRole::System => "⚙ System:",
                LlmRole::Error => "✗ Error:",
            };
            text.push_str(header);
            text.push('\n');
            text.push_str(&msg.content);
            text.push_str("\n\n");
        }

        let streaming = self.llm_buffer.streaming_content();
        if !streaming.is_empty() {
            text.push_str("◇ AI:\n");
            text.push_str(streaming);
            text.push('▊');
            text.push('\n');
        }

        if !text.ends_with('\n') {
            text.push('\n');
        }

        if let Some(buffer) = self.buffers.get_mut(&llm_id) {
            buffer.rope = Rope::from_str(&text);
            buffer.dirty = false;

            if let Some(w) = self.windows.active_window_mut() {
                if w.buffer_id == llm_id {
                    let last_line = buffer.line_count().saturating_sub(1);
                    w.cursor.position.line = last_line;
                    w.cursor.position.col = buffer.line_len(last_line);
                    w.ensure_cursor_visible(last_line + 1);
                }
            }
        }

        self.dirty.mark_all();
    }
}

/// Helper for generating short timestamps
impl Editor {
    fn session_manager(&self) -> SessionManager {
        let config_dir = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("riv");
        SessionManager::new(&config_dir)
    }

    fn timestamp_short() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let days = (secs / 86400) % 365;
        let hours = (secs / 3600) % 24;
        let mins = (secs / 60) % 60;
        format!(
            "{:02}{:02}_{:02}{:02}",
            days % 30 + 1,
            (days / 30) + 1,
            hours,
            mins
        )
    }
}
