// src/codeium.rs
//! Codeium AI completion integration.
//!
//! Spawns the Codeium language server binary, communicates over HTTP,
//! and provides inline ghost-text completions.

use crate::buffer::Language;
use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

// ── Public types ───────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct CodeiumResult {
    pub text: String,
}

/// Convert a (line, col) position to a byte/char offset within `text`.
///
/// Codeium expects a UTF-8 byte offset for `cursor_offset`, but since
/// we work with char indices internally we count Unicode scalar values.
pub fn cursor_to_offset(text: &str, line: usize, col: usize) -> usize {
    let mut offset = 0;
    let mut current_line = 0;
    let mut col_count = 0;
    for (i, ch) in text.char_indices() {
        if current_line == line && col_count == col {
            return i;
        }
        if ch == '\n' {
            current_line += 1;
            col_count = 0;
        } else if current_line == line {
            col_count += 1;
        }
        offset = i;
    }
    text.len()
}

// ── API key persistence ────────────────────────────────────────────

/// Load API key from `~/.codeium/config.toml`.
///
/// Performs a simple line-by-line parse (avoids pulling in a full TOML
/// crate) looking for `api_key = "..."`.
pub fn load_api_key_from_config() -> Option<String> {
    let home = dirs::home_dir()?;
    let config_path = home.join(".codeium").join("config.toml");
    if !config_path.exists() {
        return None;
    }
    let content = fs::read_to_string(&config_path).ok()?;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("api_key") || trimmed.starts_with("apikey") {
            if let Some(value) = trimmed.split('=').nth(1) {
                let value = value.trim().trim_matches('"').trim_matches('\'');
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    None
}

/// Save API key to `~/.codeium/config.toml`.
pub fn save_api_key_to_config(api_key: &str) -> std::io::Result<()> {
    let home = dirs::home_dir()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "No home directory"))?;
    let config_dir = home.join(".codeium");
    fs::create_dir_all(&config_dir)?;
    let config_path = config_dir.join("config.toml");
    fs::write(&config_path, format!("api_key = \"{}\"\n", api_key))
}

/// Exchange a Firebase auth token for a Codeium API key.
///
/// Mirrors what `key.py` does:
///   POST https://api.codeium.com/register_user/
///   Body: { "firebase_id_token": "<token>" }
///   Response: { "api_key": "...", "name": "..." }
pub fn exchange_token_for_api_key(token: &str) -> Result<String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .context("Failed to build HTTP client")?;

    let resp = client
        .post("https://api.codeium.com/register_user/")
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "firebase_id_token": token
        }))
        .send()
        .context("Failed to contact Codeium API")?;

    let status = resp.status();
    let body = resp.text().unwrap_or_default();

    if !status.is_success() {
        anyhow::bail!("Codeium API error (HTTP {}): {}", status, body);
    }

    let json: serde_json::Value =
        serde_json::from_str(&body).context("Invalid JSON response from Codeium")?;

    let api_key = json
        .get("api_key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("No api_key in Codeium response"))?
        .to_string();

    // Best-effort save
    let _ = save_api_key_to_config(&api_key);

    Ok(api_key)
}

/// Open the Codeium auth URL in the user's browser.
pub fn open_codeium_auth_browser() -> String {
    let state = uuid::Uuid::new_v4().to_string();

    let url = format!(
        "https://www.codeium.com/profile\
         ?response_type=token\
         &redirect_uri=show-auth-token\
         &state={state}\
         &scope=openid%20profile%20email\
         &redirect_parameters_type=query"
    );

    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(&url).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", &url])
        .spawn();

    state
}

// ── Internal: Server process ──────────────────────────────────────

struct CodeiumServer {
    _process: Child,
    port: u16,
    client: reqwest::blocking::Client,
    api_key: String,
    session_id: String,
    request_counter: AtomicU64,
}

impl CodeiumServer {
    /// Platform-specific binary name.
    #[cfg(target_os = "linux")]
    const BINARY_NAME: &'static str = "language_server_linux_x64";
    #[cfg(target_os = "macos")]
    const BINARY_NAME: &'static str = "language_server_macos_arm64";
    #[cfg(target_os = "windows")]
    const BINARY_NAME: &'static str = "language_server_windows_x64.exe";

    /// Search for the Codeium language server binary.
    ///
    /// Priority:
    ///   1. `~/.codeium/bin/<BINARY_NAME>`          (direct)
    ///   2. `~/.codeium/bin/<version>/<BINARY_NAME>` (versioned, newest first)
    ///   3. `~/.cache/nvim/codeium/bin/<version>/...` (Neovim install)
    fn find_binary() -> Result<String> {
        let home = dirs::home_dir().context("No home directory")?;

        let search_dirs = [
            home.join(".codeium").join("bin"),
            home.join(".cache").join("nvim").join("codeium").join("bin"),
        ];

        for dir in &search_dirs {
            if !dir.exists() {
                continue;
            }

            // Direct binary (no version subdirectory)
            let direct = dir.join(Self::BINARY_NAME);
            if direct.exists() {
                return Ok(direct.to_string_lossy().to_string());
            }

            // Versioned subdirectories — sort newest-first
            if let Ok(entries) = fs::read_dir(dir) {
                let mut versions: Vec<_> = entries.filter_map(|e| e.ok()).collect();
                versions.sort_by(|a, b| b.file_name().cmp(&a.file_name()));

                for entry in versions {
                    let path = entry.path();
                    if path.is_dir() {
                        let bin = path.join(Self::BINARY_NAME);
                        if bin.exists() {
                            return Ok(bin.to_string_lossy().to_string());
                        }
                    }
                }
            }
        }

        anyhow::bail!(
            "Codeium binary not found.\n\
             Searched:\n  {:?}\n  {:?}\n\
             Install via: https://codeium.com/download\n\
             Or the Neovim plugin will install it to \
             ~/.cache/nvim/codeium/bin/",
            search_dirs[0],
            search_dirs[1]
        )
    }

    /// Spawn the Codeium language server and wait for it to become ready.
    fn new(api_key: String) -> Result<Self> {
        let binary_path = Self::find_binary()?;

        let home = dirs::home_dir().context("No home directory")?;
        let database_dir = home.join(".codeium").join("database");
        fs::create_dir_all(&database_dir)?;

        let manager_dir = std::env::temp_dir().join(format!("codeium_cli_{}", std::process::id()));
        let _ = fs::remove_dir_all(&manager_dir);
        fs::create_dir_all(&manager_dir)?;

        let process = Command::new(&binary_path)
            .arg("--api_server_url")
            .arg("https://server.codeium.com")
            .arg("--manager_dir")
            .arg(&manager_dir)
            .arg("--database_dir")
            .arg(&database_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null()) // .stderr(Stdio::inherit())
            .spawn()
            .context("Failed to spawn Codeium server")?;

        let port = Self::wait_for_port(&manager_dir, Duration::from_secs(30))?;

        // Give the server a moment to fully initialize
        std::thread::sleep(Duration::from_millis(500));

        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;

        Ok(Self {
            _process: process,
            port,
            client,
            api_key,
            session_id: uuid::Uuid::new_v4().to_string(),
            request_counter: AtomicU64::new(1),
        })
    }

    /// Wait for the server to write its port file in `manager_dir`.
    fn wait_for_port(dir: &PathBuf, timeout: Duration) -> Result<u16> {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_file() {
                        // Port file might be named just the port number
                        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                            if let Ok(port) = name.parse::<u16>() {
                                return Ok(port);
                            }
                        }
                        // Or it might contain the port as text
                        if let Ok(content) = fs::read_to_string(&path) {
                            if let Ok(port) = content.trim().parse::<u16>() {
                                return Ok(port);
                            }
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        anyhow::bail!(
            "Timeout ({:?}) waiting for Codeium port file in {:?}\n\
             The server may have failed to start. Check stderr above.",
            timeout,
            dir
        )
    }

    /// Send a GetCompletions request to the Codeium server.
    fn get_completion(
        &self,
        full_text: &str,
        cursor_offset: usize,
        language: &str,
        absolute_path: Option<&str>,
    ) -> Result<Option<String>> {
        let url = format!(
            "http://127.0.0.1:{}/exa.language_server_pb.LanguageServerService/GetCompletions",
            self.port
        );

        let req_id = self.request_counter.fetch_add(1, Ordering::SeqCst);

        // absolute_path is critical — without it Codeium cannot index
        // the project or provide cross-file context.
        let abs_path = absolute_path.unwrap_or("");

        let body = serde_json::json!({
            "metadata": {
                "api_key": self.api_key,
                "ide_name": "vscode",
                "ide_version": "1.0.0",
                "extension_version": "1.46.3",
                "request_id": req_id,
                "session_id": self.session_id,
            },
            "document": {
                "text": full_text,
                "cursor_offset": cursor_offset,
                "editor_language": language,
                "language": Self::language_id(language),
                "absolute_path": abs_path,
            },
            "editor_options": {
                "tab_size": 4,
                "insert_spaces": true,
            }
        });

        let response = self
            .client
            .post(&url)
            .header("Connect-Protocol-Version", "1")
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .context("Codeium request failed")?;

        let status = response.status();
        let text = response.text().unwrap_or_default();

        if !status.is_success() {
            anyhow::bail!("Codeium HTTP {}: {}", status, text);
        }

        let json: serde_json::Value = serde_json::from_str(&text)?;

        // Extract the first completion's text
        json.get("completionItems")
            .and_then(|items| items.as_array())
            .and_then(|arr| arr.first())
            .and_then(|item| item.get("completion"))
            .and_then(|c| c.get("text"))
            .and_then(|t| t.as_str())
            .filter(|t| !t.is_empty())
            .map(|t| t.to_string())
            .ok_or_else(|| anyhow::anyhow!("No completion in response"))
            .map(Some)
    }

    /// Map editor language name to Codeium's numeric language ID.
    fn language_id(lang: &str) -> u32 {
        match lang {
            "rust" => 36,
            "javascript" => 17,
            "typescript" => 45,
            "python" => 33,
            "go" => 9,
            "c" => 1,
            "cpp" => 4,
            "java" => 16,
            "ruby" => 35,
            "php" => 29,
            "html" => 14,
            "css" => 6,
            "sh" | "bash" => 40,
            "sql" => 38,
            "swift" => 42,
            "kotlin" => 18,
            "dart" => 8,
            "scala" => 37,
            "r" => 34,
            "lua" => 21,
            "perl" => 28,
            "markdown" => 22,
            "yaml" => 48,
            "toml" => 44,
            "json" => 15,
            "xml" => 47,
            "dockerfile" => 7,
            _ => 0,
        }
    }
}

impl Drop for CodeiumServer {
    fn drop(&mut self) {
        let dir = std::env::temp_dir().join(format!("codeium_cli_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
    }
}

// ── Internal: Request/Response types ───────────────────────────────

struct CompletionRequest {
    full_text: String,
    cursor_offset: usize,
    language: String,
    absolute_path: Option<String>,
    response_tx: oneshot::Sender<Result<Option<CodeiumResult>>>,
}

// ── Public: Manager (async interface) ─────────────────────────────

pub struct CodeiumManager {
    request_tx: Option<mpsc::UnboundedSender<CompletionRequest>>,
    pending_rx: Option<oneshot::Receiver<Result<Option<CodeiumResult>>>>,
    last_request: Instant,
    debounce_ms: u64,
    pub is_connected: bool,
    /// Channel to receive server startup result.
    startup_rx: Option<oneshot::Receiver<Result<()>>>,
}

impl CodeiumManager {
    pub fn new(debounce_ms: u64) -> Self {
        Self {
            request_tx: None,
            pending_rx: None,
            last_request: Instant::now() - Duration::from_millis(debounce_ms + 1),
            debounce_ms,
            is_connected: false,
            startup_rx: None,
        }
    }

    /// Start the Codeium server in a background thread.
    ///
    /// Uses `runtime.spawn()` instead of `block_on()`, and only
    /// sets `is_connected = true` after `poll_startup()` confirms
    /// success.
    pub fn start(&mut self, api_key: String, runtime: &tokio::runtime::Runtime) -> Result<()> {
        if self.is_connected {
            return Ok(());
        }

        let (tx, rx) = mpsc::unbounded_channel::<CompletionRequest>();
        let (startup_tx, startup_rx) = oneshot::channel::<Result<()>>();

        runtime.spawn(async move {
            tokio::task::spawn_blocking(move || match CodeiumServer::new(api_key) {
                Ok(server) => {
                    let _ = startup_tx.send(Ok(()));
                    server_loop(server, rx);
                }
                Err(e) => {
                    let _ = startup_tx.send(Err(e));
                }
            })
            .await
            .ok();
        });

        self.request_tx = Some(tx);
        self.startup_rx = Some(startup_rx);
        // Do NOT set is_connected = true here.
        // Wait for poll_startup() to confirm.
        Ok(())
    }

    /// Poll for server startup result.
    ///
    /// Call this from the editor tick loop.
    /// Returns `Some(Ok(()))` when server is ready,
    /// `Some(Err(...))` on failure, `None` if still starting.
    pub fn poll_startup(&mut self) -> Option<Result<()>> {
        let mut rx = self.startup_rx.take()?;
        match rx.try_recv() {
            Ok(result) => {
                if result.is_ok() {
                    self.is_connected = true;
                }
                Some(result)
            }
            Err(oneshot::error::TryRecvError::Empty) => {
                self.startup_rx = Some(rx);
                None
            }
            Err(oneshot::error::TryRecvError::Closed) => {
                Some(Err(anyhow::anyhow!("Codeium: startup channel closed")))
            }
        }
    }

    /// Request a completion (auto-trigger, debounced).
    ///
    /// `absolute_path` should be the canonical path to the file on disk.
    /// This is **critical** — Codeium uses it to index the project and
    /// provide cross-file context. Without it, completions are single-file
    /// only and much less useful.
    pub fn request(
        &mut self,
        full_text: String,
        cursor_offset: usize,
        language: &str,
        absolute_path: Option<String>,
    ) -> bool {
        let tx = match &self.request_tx {
            Some(tx) => tx.clone(),
            None => return false,
        };

        if self.last_request.elapsed().as_millis() < self.debounce_ms as u128 {
            return false;
        }
        self.last_request = Instant::now();
        self.pending_rx = None;

        let (resp_tx, resp_rx) = oneshot::channel();
        self.pending_rx = Some(resp_rx);

        tx.send(CompletionRequest {
            full_text,
            cursor_offset,
            language: language.to_string(),
            absolute_path,
            response_tx: resp_tx,
        })
        .is_ok()
    }

    /// Force a completion request, ignoring debounce timer.
    ///
    /// Used for manual Alt+/ triggers.
    pub fn request_force(
        &mut self,
        full_text: String,
        cursor_offset: usize,
        language: &str,
        absolute_path: Option<String>,
    ) -> bool {
        let tx = match &self.request_tx {
            Some(tx) => tx.clone(),
            None => {
                return false;
            }
        };

        // Reset debounce timer so next auto-request still debounces
        self.last_request = Instant::now();
        self.pending_rx = None;

        let (resp_tx, resp_rx) = oneshot::channel();
        self.pending_rx = Some(resp_rx);

        tx.send(CompletionRequest {
            full_text,
            cursor_offset,
            language: language.to_string(),
            absolute_path,
            response_tx: resp_tx,
        })
        .is_ok()
    }

    /// Convenience: request with Language enum.
    pub fn request_with_language(
        &mut self,
        full_text: String,
        cursor_offset: usize,
        language: Language,
        absolute_path: Option<String>,
    ) -> bool {
        self.request(full_text, cursor_offset, language.as_str(), absolute_path)
    }

    /// Poll for a pending completion result (non-blocking).
    ///
    /// Returns `Some(result)` when a response is available,
    /// `None` if still pending.
    pub fn poll(&mut self) -> Option<Result<Option<CodeiumResult>>> {
        let mut rx = self.pending_rx.take()?;
        match rx.try_recv() {
            Ok(result) => Some(result),
            Err(oneshot::error::TryRecvError::Empty) => {
                self.pending_rx = Some(rx);
                None
            }
            Err(oneshot::error::TryRecvError::Closed) => None,
        }
    }

    /// Cancel any in-flight request.
    pub fn cancel(&mut self) {
        self.pending_rx = None;
    }

    /// Whether a request is in flight.
    pub fn is_pending(&self) -> bool {
        self.pending_rx.is_some()
    }

    /// Whether the server is currently starting up.
    pub fn is_starting(&self) -> bool {
        self.startup_rx.is_some()
    }

    /// Stop the Codeium server.
    pub fn stop(&mut self) {
        self.request_tx = None;
        self.pending_rx = None;
        self.startup_rx = None;
        self.is_connected = false;
    }
}

impl Default for CodeiumManager {
    fn default() -> Self {
        Self::new(150)
    }
}

// ── Server loop (runs on a background thread) ─────────────────────

fn server_loop(server: CodeiumServer, mut rx: mpsc::UnboundedReceiver<CompletionRequest>) {
    loop {
        match rx.blocking_recv() {
            Some(req) => {
                let result = server
                    .get_completion(
                        &req.full_text,
                        req.cursor_offset,
                        &req.language,
                        req.absolute_path.as_deref(),
                    )
                    .map(|opt| opt.map(|text| CodeiumResult { text }));
                let _ = req.response_tx.send(result);
            }
            None => break,
        }
    }
}
