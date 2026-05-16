// src/health.rs
//! `--healthy` startup check — validates config, keybindings, and environment.

use crate::config::{Config, ConfigError, HistoryData, LlmBackend};
use crossterm::style::Stylize;

// ── Types ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Ok,
    Warn,
    Err,
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Severity::Ok => write!(f, "  OK"),
            Severity::Warn => write!(f, "WARN"),
            Severity::Err => write!(f, "ERR!"),
        }
    }
}

impl Severity {
    fn styled(&self) -> String {
        match self {
            Severity::Ok => "  OK".green().to_string(),
            Severity::Warn => "WARN".yellow().to_string(),
            Severity::Err => "ERR!".red().to_string(),
        }
    }
}

#[derive(Debug)]
pub struct HealthIssue {
    pub severity: Severity,
    pub category: &'static str,
    pub message: String,
}

pub struct HealthReport {
    pub issues: Vec<HealthIssue>,
}

impl HealthReport {
    pub fn has_errors(&self) -> bool {
        self.issues.iter().any(|i| i.severity == Severity::Err)
    }

    pub fn print(&self) {
        if self.issues.is_empty() {
            println!("{}", "(no checks run)".grey());
            return;
        }

        // Group by category for cleaner output
        let mut categories: Vec<(&str, Vec<&HealthIssue>)> = Vec::new();
        for issue in &self.issues {
            if let Some(slot) = categories
                .iter_mut()
                .find(|(cat, _)| *cat == issue.category)
            {
                slot.1.push(issue);
            } else {
                categories.push((issue.category, vec![issue]));
            }
        }

        for (category, items) in &categories {
            println!(
                "\n{} {} {}",
                "──".dark_grey(),
                category.to_uppercase().bold().cyan(),
                "──".dark_grey(),
            );
            for issue in items {
                let (icon, styled_severity) = match issue.severity {
                    Severity::Ok => ("✓".green().to_string(), Severity::Ok.styled()),
                    Severity::Warn => ("⚠".yellow().to_string(), Severity::Warn.styled()),
                    Severity::Err => ("✗".red().to_string(), Severity::Err.styled()),
                };
                println!(
                    "  {} {} {}",
                    icon,
                    format!("[{}]", styled_severity),
                    colorize_message(&issue.message),
                );
            }
        }

        let errors = self
            .issues
            .iter()
            .filter(|i| i.severity == Severity::Err)
            .count();
        let warns = self
            .issues
            .iter()
            .filter(|i| i.severity == Severity::Warn)
            .count();
        let oks = self
            .issues
            .iter()
            .filter(|i| i.severity == Severity::Ok)
            .count();

        println!();
        if errors > 0 {
            println!(
                "{} {} {}{} {} {}{} {}",
                "✗".red(),
                errors.to_string().red().bold(),
                "error".red(),
                if errors != 1 { "s" } else { "" }.red(),
                warns.to_string().yellow().bold(),
                "warning".yellow(),
                if warns != 1 { "s" } else { "" }.yellow(),
                format!("{}, {} passed", oks, oks + warns + errors).grey(),
            );
        } else if warns > 0 {
            println!(
                "{} {} {}{} {} {}",
                "⚠".yellow(),
                "0 errors,".green(),
                warns.to_string().yellow().bold(),
                " warning".yellow(),
                if warns != 1 { "s" } else { "" }.yellow(),
                format!("{}, {} passed", oks, oks + warns).grey(),
            );
        } else {
            println!(
                "{} {}",
                "✓".green(),
                format!("All {} check(s) passed", oks).green().bold(),
            );
        }
    }
}

/// Apply mild colorization to message content:
///   - Base text is rendered in dark white (grey)
///   - Paths inside `⟨…⟩` or `"…"` → magenta
///   - Quoted values like `'foo'` → cyan
///   - Numbers attached to key names → yellow
fn colorize_message(msg: &str) -> String {
    use crossterm::style::Color;

    // Normal text is rendered as dark white (grey)
    let base = Color::Grey;

    let mut out = String::with_capacity(msg.len() + 32);
    let chars: Vec<char> = msg.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        // ── Quoted string: "…" or '…' → cyan ──
        if chars[i] == '"' || chars[i] == '\'' {
            let quote = chars[i];
            let start = i;
            i += 1;
            while i < chars.len() && chars[i] != quote {
                i += 1;
            }
            if i < chars.len() {
                i += 1; // consume closing quote
            }
            let slice: String = chars[start..i].iter().collect();
            out.push_str(&slice.to_string().cyan().to_string());
            continue;
        }

        // ── Backtick: `…` → magenta (code / command) ──
        if chars[i] == '`' {
            let start = i;
            i += 1;
            while i < chars.len() && chars[i] != '`' {
                i += 1;
            }
            if i < chars.len() {
                i += 1;
            }
            let slice: String = chars[start..i].iter().collect();
            out.push_str(&slice.to_string().magenta().to_string());
            continue;
        }

        // ── Path-like segments (contain / or .json / .toml etc.) → dark magenta ──
        // Heuristic: if we see a word containing '/' or ending in a known extension
        if chars[i] == '/' || (chars[i].is_alphanumeric() && looks_like_path_start(&chars, i)) {
            let start = i;
            while i < chars.len()
                && !chars[i].is_whitespace()
                && chars[i] != ')'
                && chars[i] != ':'
                && chars[i] != ','
            {
                i += 1;
            }
            let slice: String = chars[start..i].iter().collect();
            if slice.contains('/')
                || slice.contains(".json")
                || slice.contains(".toml")
                || slice.contains(".rs")
            {
                out.push_str(&slice.to_string().dark_magenta().to_string());
            } else {
                out.push_str(&slice.to_string().stylize().with(base).to_string());
            }
            continue;
        }

        // ── Numbers → yellow ──
        if chars[i].is_ascii_digit()
            || (chars[i] == '-' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit())
        {
            let start = i;
            if chars[i] == '-' {
                i += 1;
            }
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                i += 1;
            }
            // Also consume trailing units like "s", "ms", etc.
            if i < chars.len() && (chars[i] == 's' || chars[i] == 'm') {
                i += 1;
                if i < chars.len() && chars[i] == 's' {
                    i += 1;
                }
            }
            let slice: String = chars[start..i].iter().collect();
            out.push_str(&slice.to_string().yellow().to_string());
            continue;
        }

        // ── Default: base grey color ──
        let ch = chars[i];
        out.push_str(&ch.to_string().stylize().with(base).to_string());
        i += 1;
    }

    out
}

/// Check if position looks like the start of a path token.
fn looks_like_path_start(chars: &[char], i: usize) -> bool {
    let rest: String = chars[i..].iter().take(60).collect();
    let lower = rest.to_lowercase();
    lower.contains('/')
        || lower.contains(".json")
        || lower.contains(".toml")
        || lower.contains(".rs")
        || lower.contains(".txt")
        || lower.contains(".md")
        || lower.contains(".lock")
}

// ── Entry point ─────────────────────────────────────────────────────

/// Run all health checks. Returns exit code (0 = healthy, 1 = errors).
pub fn run_health_check() -> i32 {
    let mut issues = Vec::new();

    check_config_dir(&mut issues);
    check_config_file(&mut issues);
    check_config_values(&mut issues);
    check_keybindings(&mut issues);
    check_history_file(&mut issues);
    check_mru_file(&mut issues);
    check_session_file(&mut issues);
    check_tool_availability(&mut issues);
    check_terminal_env(&mut issues);
    check_clipboard(&mut issues);
    check_locale(&mut issues);

    let report = HealthReport { issues };
    report.print();

    if report.has_errors() {
        1
    } else {
        0
    }
}

// ── Individual checks ──────────────────────────────────────────────

fn check_config_dir(issues: &mut Vec<HealthIssue>) {
    let dir = match Config::config_dir() {
        Ok(d) => d,
        Err(e) => {
            issues.push(err(
                "config",
                format!("Cannot determine config directory: {}", e),
            ));
            return;
        }
    };

    if !dir.exists() {
        match std::fs::create_dir_all(&dir) {
            Ok(()) => {
                issues.push(ok("config", format!("Created config directory {:?}", dir)));
            }
            Err(e) => {
                issues.push(err(
                    "config",
                    format!("Cannot create config directory {:?}: {}", dir, e),
                ));
                return;
            }
        }
    }

    // Check writability
    let test_file = dir.join(".riv-health-check");
    match std::fs::write(&test_file, b"test") {
        Ok(()) => {
            std::fs::remove_file(&test_file).ok();
            issues.push(ok(
                "config",
                format!("Config directory {:?} is writable", dir),
            ));
        }
        Err(e) => {
            issues.push(err(
                "config",
                format!("Config directory {:?} is NOT writable: {}", dir, e),
            ));
        }
    }
}

fn check_config_file(issues: &mut Vec<HealthIssue>) {
    let path = match Config::config_path() {
        Ok(p) => p,
        Err(e) => {
            issues.push(err(
                "config",
                format!("Cannot determine config path: {}", e),
            ));
            return;
        }
    };

    if !path.exists() {
        issues.push(ok("config", "No config file (using defaults)".to_string()));
        return;
    }

    // Read the raw file
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            issues.push(err("config", format!("Cannot read {:?}: {}", path, e)));
            return;
        }
    };

    // Check for BOM
    if content.starts_with('\u{feff}') {
        issues.push(warn(
            "config",
            format!(
                "{:?} contains a UTF-8 BOM — this may cause parse errors",
                path
            ),
        ));
    }

    // Check for tab characters (TOML should use spaces)
    let tab_lines: Vec<usize> = content
        .lines()
        .enumerate()
        .filter(|(_, l)| l.contains('\t'))
        .map(|(i, _)| i + 1)
        .collect();
    if !tab_lines.is_empty() {
        issues.push(warn(
            "config",
            format!(
                "{:?} contains tab characters on lines {:?} (TOML convention: use spaces)",
                path,
                &tab_lines[..tab_lines.len().min(5)]
            ),
        ));
    }

    // Parse as raw TOML first (catches syntax errors)
    if let Err(e) | Ok(Err(e)) = content.parse::<toml::Value>().map(|_| Ok(())) {
        issues.push(err(
            "config",
            format!("TOML syntax error in {:?}: {}", path, e),
        ));
        return;
    }

    // Full typed parse
    match Config::load() {
        Ok(_) => {
            issues.push(ok("config", format!("Config parsed OK from {:?}", path)));
        }
        Err(ConfigError::Toml(e)) => {
            issues.push(err(
                "config",
                format!("Config type error in {:?}: {}", path, e),
            ));
        }
        Err(e) => {
            issues.push(err("config", format!("Config error: {}", e)));
        }
    }
}

fn check_config_values(issues: &mut Vec<HealthIssue>) {
    let config = match Config::load() {
        Ok(c) => c,
        Err(_) => return, // already reported
    };

    // ── Tab width ──
    if config.tab_width == 0 {
        issues.push(err("config", "tab_width must be > 0"));
    } else if config.tab_width > 16 {
        issues.push(warn(
            "config",
            format!("tab_width = {} is unusually large", config.tab_width),
        ));
    } else {
        issues.push(ok(
            "config",
            format!(
                "tab_width = {} ({})",
                config.tab_width,
                if config.use_tabs { "tabs" } else { "spaces" }
            ),
        ));
    }

    // ── Scroll offset ──
    if config.scroll_offset > 50 {
        issues.push(warn(
            "config",
            format!("scroll_offset = {} is very large", config.scroll_offset),
        ));
    } else {
        issues.push(ok(
            "config",
            format!("scroll_offset = {}", config.scroll_offset),
        ));
    }

    // ── Auto-save ──
    if config.auto_save > 0 {
        issues.push(ok("config", format!("auto_save = {}s", config.auto_save)));
    }

    // ── Undo levels ──
    if config.undo_levels == 0 {
        issues.push(err("config", "undo_levels = 0 (undo is disabled!)"));
    } else if config.undo_levels < 100 {
        issues.push(warn(
            "config",
            format!("undo_levels = {} is very low", config.undo_levels),
        ));
    }

    // ── Ruler ──
    if let Some(r) = config.ruler {
        if r == 0 {
            issues.push(warn("config", "ruler = 0 has no effect"));
        } else {
            issues.push(ok("config", format!("ruler at column {}", r)));
        }
    }

    // ── Completion ──
    if config.enable_completion {
        if config.completion_trigger_len == 0 {
            issues.push(warn(
                "config",
                "completion_trigger_len = 0 means completion triggers on every keystroke",
            ));
        } else {
            issues.push(ok(
                "config",
                format!(
                    "completion enabled (trigger len = {})",
                    config.completion_trigger_len
                ),
            ));
        }
    }

    // ── LSP ──
    if config.enable_lsp {
        issues.push(ok("config", "LSP enabled"));
    } else {
        issues.push(warn("config", "LSP disabled — no code intelligence"));
    }

    // ── Git ──
    if config.enable_git {
        issues.push(ok("config", "Git integration enabled"));
    } else {
        issues.push(warn("config", "Git integration disabled"));
    }

    // ── Word wrap ──
    if config.word_wrap {
        issues.push(ok("config", "word_wrap enabled"));
    }

    // ── Format on save ──
    if config.format_on_save {
        issues.push(ok("config", "format_on_save enabled"));
    }

    // ── LLM ──
    if config.llm.enabled {
        match &config.llm.backend {
            LlmBackend::OpenAi {
                api_key,
                endpoint,
                model,
            } => {
                if api_key.is_empty() {
                    issues.push(err("llm", "OpenAI backend enabled but api_key is empty"));
                } else if api_key.len() < 20 {
                    issues.push(warn("llm", "OpenAI api_key looks too short"));
                } else {
                    issues.push(ok(
                        "llm",
                        format!("OpenAI backend: {} @ {}", model, endpoint),
                    ));
                }
            }
            LlmBackend::Ollama { endpoint, model } => {
                issues.push(ok(
                    "llm",
                    format!("Ollama backend: {} @ {}", model, endpoint),
                ));
            }
            LlmBackend::LlamaCpp { endpoint, model } => {
                issues.push(ok(
                    "llm",
                    format!("llama.cpp backend: {} @ {}", model, endpoint),
                ));
            }
        }
        if config.llm.temperature > 2.0 {
            issues.push(err(
                "llm",
                format!(
                    "temperature = {} is out of range (0.0–2.0)",
                    config.llm.temperature
                ),
            ));
        } else {
            issues.push(ok(
                "llm",
                format!("temperature = {}", config.llm.temperature),
            ));
        }
        if config.llm.max_tokens == 0 {
            issues.push(warn("llm", "max_tokens = 0 (no output will be generated)"));
        } else {
            issues.push(ok("llm", format!("max_tokens = {}", config.llm.max_tokens)));
        }
        if config.llm.timeout_secs == 0 {
            issues.push(warn("llm", "timeout_secs = 0 (may hang indefinitely)"));
        }
    } else {
        issues.push(ok("llm", "LLM disabled (no issues)"));
    }

    // ── Codeium ──
    if config.codeium.enabled {
        if config.codeium.api_key.is_none() && std::env::var("CODEIUM_API_KEY").is_err() {
            issues.push(warn(
                "codeium",
                "enabled but no api_key set (run :codeium-auth or set CODEIUM_API_KEY)",
            ));
        } else {
            issues.push(ok("codeium", "API key configured"));
        }
        if config.codeium.debounce_ms == 0 {
            issues.push(warn(
                "codeium",
                "debounce_ms = 0 may cause excessive API calls",
            ));
        }
    } else {
        issues.push(ok("codeium", "disabled (no issues)"));
    }

    // ── Leader key ──
    issues.push(ok("config", format!("leader key = '{}'", config.leader)));
}

fn check_keybindings(issues: &mut Vec<HealthIssue>) {
    let config = match Config::load() {
        Ok(c) => c,
        Err(_) => return,
    };

    if config.keybindings.is_empty() {
        issues.push(ok("keybind", "No custom keybindings (using defaults)"));
        return;
    }

    let mut found_error = false;
    let mut total_bindings = 0;

    for (mode, value) in &config.keybindings {
        let mode_table = match value.as_table() {
            Some(t) => t,
            None => {
                issues.push(err(
                    "keybind",
                    format!(
                        "Mode '{}' must be a table, got {:?}",
                        mode,
                        value.type_str()
                    ),
                ));
                found_error = true;
                continue;
            }
        };

        // Validate mode name
        if !["normal", "insert", "visual", "command"].contains(&mode.as_str()) {
            issues.push(warn(
                "keybind",
                format!(
                    "Unknown mode '{}' (valid: normal, insert, visual, command)",
                    mode
                ),
            ));
        }

        for (key_seq, action_val) in mode_table {
            total_bindings += 1;

            // Validate key format
            if !is_valid_key_sequence(key_seq) {
                issues.push(err(
                    "keybind",
                    format!("Invalid key sequence '{}' in mode '{}'", key_seq, mode),
                ));
                found_error = true;
                continue;
            }

            // Validate action value structure (not the name itself)
            match action_val {
                toml::Value::String(action_name) => {
                    if !is_valid_action_name(action_name) {
                        issues.push(err(
                            "keybind",
                            format!(
                                "Invalid action name '{}' for key '{}' in mode '{}' \
                                 (use alphanumeric + underscores)",
                                action_name, key_seq, mode
                            ),
                        ));
                        found_error = true;
                    }
                }
                toml::Value::Array(arr) => {
                    for item in arr {
                        if let toml::Value::String(s) = item {
                            if !is_valid_action_name(s) {
                                issues.push(err(
                                    "keybind",
                                    format!(
                                        "Invalid action name '{}' in sequence for key '{}' in mode '{}'",
                                        s, key_seq, mode
                                    ),
                                ));
                                found_error = true;
                            }
                        } else {
                            issues.push(err(
                                "keybind",
                                format!(
                                    "Action array item for key '{}' in mode '{}' must be a string",
                                    key_seq, mode
                                ),
                            ));
                            found_error = true;
                        }
                    }
                }
                toml::Value::Boolean(false) => {
                    // unbind — always valid
                }
                _ => {
                    issues.push(err(
                        "keybind",
                        format!(
                            "Action for key '{}' in mode '{}' must be a string, array, or false (unbind)",
                            key_seq, mode
                        ),
                    ));
                    found_error = true;
                }
            }
        }
    }

    if !found_error {
        issues.push(ok(
            "keybind",
            format!("{} custom binding(s) validated OK", total_bindings),
        ));
    }
}

fn check_history_file(issues: &mut Vec<HealthIssue>) {
    let path = match Config::history_path() {
        Ok(p) => p,
        Err(_) => return,
    };

    if !path.exists() {
        issues.push(ok(
            "history",
            "No history file yet (will be created on exit)",
        ));
        return;
    }

    match std::fs::read_to_string(&path) {
        Ok(content) => match toml::from_str::<HistoryData>(&content) {
            Ok(data) => {
                let cmd_len = data.command.len();
                let search_len = data.search.len();
                issues.push(ok(
                    "history",
                    format!("History OK ({} commands, {} searches)", cmd_len, search_len),
                ));
                if cmd_len > 800 {
                    issues.push(warn(
                        "history",
                        format!(
                            "Command history has {} entries (max 1000, consider pruning)",
                            cmd_len
                        ),
                    ));
                }
            }
            Err(e) => {
                issues.push(err(
                    "history",
                    format!("Corrupt history file {:?}: {}", path, e),
                ));
            }
        },
        Err(e) => {
            issues.push(warn("history", format!("Cannot read {:?}: {}", path, e)));
        }
    }
}

fn check_mru_file(issues: &mut Vec<HealthIssue>) {
    let path = match Config::config_dir() {
        Ok(d) => d.join("mru.json"),
        Err(_) => return,
    };

    if !path.exists() {
        issues.push(ok("mru", "No MRU file yet (will be created on use)"));
        return;
    }

    match std::fs::read_to_string(&path) {
        Ok(content) => match serde_json::from_str::<serde_json::Value>(&content) {
            Ok(data) => {
                let count = data
                    .get("mru_files")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                issues.push(ok("mru", format!("MRU OK ({} entries)", count)));

                let missing = data
                    .get("mru_files")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|entry| entry.get("path").and_then(|p| p.as_str()))
                            .filter(|p| !std::path::Path::new(p).exists())
                            .count()
                    })
                    .unwrap_or(0);
                if missing > 0 {
                    issues.push(warn(
                        "mru",
                        format!(
                            "{} of {} entries point to files that no longer exist",
                            missing, count
                        ),
                    ));
                }
            }
            Err(e) => {
                issues.push(err("mru", format!("Corrupt MRU file {:?}: {}", path, e)));
            }
        },
        Err(e) => {
            issues.push(warn("mru", format!("Cannot read {:?}: {}", path, e)));
        }
    }
}

fn check_session_file(issues: &mut Vec<HealthIssue>) {
    let path = match Config::config_dir() {
        Ok(d) => d.join("session.json"),
        Err(_) => return,
    };

    if !path.exists() {
        issues.push(ok(
            "session",
            "No session file yet (will be created on exit)",
        ));
        return;
    }

    match std::fs::read_to_string(&path) {
        Ok(content) => match serde_json::from_str::<serde_json::Value>(&content) {
            Ok(data) => {
                let count = data
                    .get("positions")
                    .and_then(|v| v.as_object())
                    .map(|o| o.len())
                    .unwrap_or(0);
                issues.push(ok(
                    "session",
                    format!("Session OK ({} saved position(s))", count),
                ));
            }
            Err(e) => {
                issues.push(warn(
                    "session",
                    format!("Corrupt session file {:?}: {}", path, e),
                ));
            }
        },
        Err(e) => {
            issues.push(warn("session", format!("Cannot read {:?}: {}", path, e)));
        }
    }
}

fn check_tool_availability(issues: &mut Vec<HealthIssue>) {
    let tools = [
        ("git", "Git integration", true),
        ("rg", "Ripgrep search", false),
        ("rustfmt", "Rust formatting", false),
        ("prettier", "JS/TS formatting", false),
        ("black", "Python formatting", false),
        ("ollama", "Ollama LLM", false),
        ("node", "LSP servers", false),
    ];

    let mut any_required_missing = false;

    for (cmd, desc, required) in &tools {
        match which::which(cmd) {
            Ok(path) => {
                let version = get_tool_version(cmd);
                let ver_str = match version {
                    Some(v) => format!(" {}", v),
                    None => String::new(),
                };
                issues.push(ok(
                    "tool",
                    format!("{} ({}) found{}: {:?}", desc, cmd, ver_str, path),
                ));
            }
            Err(_) => {
                if *required {
                    issues.push(err(
                        "tool",
                        format!("{} ({}) NOT found (required)", desc, cmd),
                    ));
                    any_required_missing = true;
                } else {
                    issues.push(warn(
                        "tool",
                        format!("{} ({}) not found on PATH", desc, cmd),
                    ));
                }
            }
        }
    }

    if any_required_missing {
        issues.push(err(
            "tool",
            "Required tools are missing — some features will not work",
        ));
    }
}

fn check_terminal_env(issues: &mut Vec<HealthIssue>) {
    // $TERM
    match std::env::var("TERM") {
        Ok(term) => {
            issues.push(ok("terminal", format!("$TERM = {}", term)));
            if term.contains("256color") || term.contains("256") {
                issues.push(ok("terminal", "256-color support detected"));
            } else if term.contains("xterm") && !term.contains("256") {
                issues.push(warn(
                    "terminal",
                    format!("$TERM = {} — may not support 256 colors", term),
                ));
            }
        }
        Err(_) => {
            issues.push(warn("terminal", "$TERM not set"));
        }
    }

    // $TERM_PROGRAM
    if let Ok(prog) = std::env::var("TERM_PROGRAM") {
        issues.push(ok("terminal", format!("Terminal program: {}", prog)));
    }

    // COLORTERM
    match std::env::var("COLORTERM") {
        Ok(ct) => {
            issues.push(ok("terminal", format!("$COLORTERM = {} (true color)", ct)));
        }
        Err(_) => {
            issues.push(warn(
                "terminal",
                "$COLORTERM not set — true color (24-bit) may not be available",
            ));
        }
    }

    // Check if stdout is a TTY
    if !atty::is(atty::Stream::Stdout) {
        issues.push(warn(
            "terminal",
            "stdout is not a TTY — editor may not render correctly",
        ));
    } else {
        issues.push(ok("terminal", "stdout is a TTY"));
    }
}

fn check_clipboard(issues: &mut Vec<HealthIssue>) {
    match arboard::Clipboard::new() {
        Ok(_) => {
            issues.push(ok("clipboard", "System clipboard accessible"));
        }
        Err(e) => {
            issues.push(warn(
                "clipboard",
                format!("System clipboard not available: {}", e),
            ));
        }
    }
}

fn check_locale(issues: &mut Vec<HealthIssue>) {
    let lang = std::env::var("LANG")
        .or_else(|_| std::env::var("LC_ALL"))
        .unwrap_or_else(|_| "(not set)".to_string());

    if lang.contains("UTF-8") || lang.contains("utf8") || lang.contains("utf-8") {
        issues.push(ok("locale", format!("$LANG = {} (UTF-8)", lang)));
    } else if lang == "(not set)" {
        issues.push(warn(
            "locale",
            "$LANG and $LC_ALL not set — Unicode may not display correctly",
        ));
    } else {
        issues.push(warn(
            "locale",
            format!("$LANG = {} — may not support UTF-8", lang),
        ));
    }
}

// ── Helpers ─────────────────────────────────────────────────────────

#[rustfmt::skip]
fn ok(category: &'static str, message: impl Into<String>) -> HealthIssue {
    HealthIssue { severity: Severity::Ok, category, message: message.into() }
}

#[rustfmt::skip]
fn warn(category: &'static str, message: impl Into<String>) -> HealthIssue {
    HealthIssue { severity: Severity::Warn, category, message: message.into() }
}

#[rustfmt::skip]
fn err(category: &'static str, message: impl Into<String>) -> HealthIssue {
    HealthIssue { severity: Severity::Err, category, message: message.into() }
}

/// Try to get the version string for a tool.
fn get_tool_version(cmd: &str) -> Option<String> {
    let args = match cmd {
        "git" => vec!["--version"],
        "rg" => vec!["--version"],
        "rustfmt" => vec!["--version"],
        "prettier" => vec!["--version"],
        "black" => vec!["--version"],
        "ollama" => vec!["--version"],
        "node" => vec!["--version"],
        _ => return None,
    };

    let output = std::process::Command::new(cmd).args(&args).output().ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let first_line = stdout.lines().next().unwrap_or("");
    let version = first_line
        .trim()
        .trim_start_matches(&format!("{} ", cmd))
        .trim()
        .to_string();

    if version.is_empty() {
        None
    } else {
        Some(version)
    }
}

/// Check if a key sequence string looks valid.
fn is_valid_key_sequence(key: &str) -> bool {
    let key = key.trim();
    if key.is_empty() {
        return false;
    }

    let mut pos = 0;
    let bytes = key.as_bytes();
    let mut tokens = Vec::new();

    while pos < bytes.len() {
        if bytes[pos] == b'<' {
            let start = pos;
            let mut found_end = false;
            while pos < bytes.len() {
                if bytes[pos] == b'>' {
                    tokens.push(&key[start..=pos]);
                    pos += 1;
                    found_end = true;
                    break;
                }
                pos += 1;
            }
            if !found_end {
                return false;
            }
        } else {
            tokens.push(&key[pos..pos + 1]);
            pos += 1;
        }
    }

    if tokens.is_empty() {
        return false;
    }

    for token in &tokens {
        if !is_valid_key_token(token) {
            return false;
        }
    }

    true
}

fn is_valid_key_token(token: &str) -> bool {
    if token.starts_with('<') && token.ends_with('>') {
        let inner = &token[1..token.len() - 1];
        return is_valid_bracket_key(inner);
    }

    if token.chars().count() == 1 {
        return token.chars().next().unwrap().is_ascii();
    }

    false
}

fn is_valid_bracket_key(inner: &str) -> bool {
    let lower = inner.to_lowercase();

    if lower == "leader" {
        return true;
    }

    let named = [
        "enter",
        "return",
        "escape",
        "esc",
        "tab",
        "backspace",
        "bs",
        "delete",
        "del",
        "insert",
        "ins",
        "home",
        "end",
        "pageup",
        "pagedown",
        "pgup",
        "pgdn",
        "up",
        "down",
        "left",
        "right",
        "space",
        "spacebar",
        "plus",
        "minus",
        "equals",
    ];
    if named.contains(&lower.as_str()) {
        return true;
    }

    for i in 1..=12 {
        if lower == format!("f{}", i) {
            return true;
        }
    }

    for sep in &["-", "+"] {
        for prefix in &["ctrl", "alt", "c", "a"] {
            let pattern = format!("{}{}", prefix, sep);
            if let Some(rest) = lower.strip_prefix(&pattern) {
                if rest.is_empty() {
                    return false;
                }
                if rest.chars().count() == 1 {
                    return rest.chars().next().unwrap().is_ascii();
                }
                if named.contains(&rest) {
                    return true;
                }
                for i in 1..=12 {
                    if rest == format!("f{}", i) {
                        return true;
                    }
                }
                return false;
            }
        }
    }

    false
}

/// Check if an action name looks structurally valid.
fn is_valid_action_name(name: &str) -> bool {
    let name = name.trim();
    if name.is_empty() {
        return false;
    }

    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}
