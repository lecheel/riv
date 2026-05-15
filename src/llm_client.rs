//! LLM client for making HTTP requests to Ollama, llama.cpp, and OpenAI-compatible APIs.

use crate::config::{LlmBackend, LlmConfig};
use anyhow::Result;
use reqwest::Client;
use serde_json::{json, Value};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "operation cancelled")
    }
}
impl std::error::Error for Cancelled {}

/// Check if error is a cancellation
impl Cancelled {
    pub fn is_cancelled(err: &anyhow::Error) -> bool {
        err.downcast_ref::<Self>().is_some()
    }
}

#[derive(Clone)]
pub struct LlmClient {
    client: Client,
    config: LlmConfig,
}

impl LlmClient {
    pub fn new(config: &LlmConfig) -> Result<Self> {
        Ok(Self {
            client: Client::new(),
            config: config.clone(),
        })
    }

    pub fn dummy() -> Self {
        Self {
            client: Client::new(),
            config: LlmConfig::default(),
        }
    }

    fn backend_info(&self) -> (&'static str, &str, &str) {
        match &self.config.backend {
            LlmBackend::Ollama { endpoint, model } => ("Ollama", endpoint, model),
            LlmBackend::LlamaCpp { endpoint, model } => ("llama.cpp", endpoint, model),
            LlmBackend::OpenAi {
                endpoint,
                model,
                api_key: _,
            } => ("OpenAI", endpoint, model),
        }
    }

    /// Strip common LLM conversational filler
    fn clean_response(text: String) -> String {
        let mut cleaned = text.trim();

        let lower = cleaned.to_lowercase();
        if lower.starts_with("correct: ") {
            cleaned = &cleaned[9..];
        } else if lower.starts_with("corrected: ") {
            cleaned = &cleaned[11..];
        } else if lower.starts_with("translation: ") {
            cleaned = &cleaned[13..];
        }

        cleaned
            .trim_matches(|c| c == '"' || c == '\'')
            .trim()
            .to_string()
    }

    // ═══════════════════════════════════════════════════════════════════
    // Multi-turn chat with full message history
    // ═══════════════════════════════════════════════════════════════════

    /// Send a multi-turn chat request with the full message array.
    pub async fn chat_with_cancel(
        &self,
        messages: Vec<(String, String)>,
        cancel_flag: Arc<AtomicBool>,
    ) -> Result<String> {
        let (_backend_type, _endpoint, _model) = self.backend_info();

        let result = match &self.config.backend {
            LlmBackend::Ollama { .. } => self.chat_ollama(messages, cancel_flag).await,
            LlmBackend::LlamaCpp { .. } | LlmBackend::OpenAi { .. } => {
                self.chat_openai_compatible(messages, cancel_flag).await
            }
        };

        result
    }

    async fn chat_ollama(
        &self,
        messages: Vec<(String, String)>,
        cancel_flag: Arc<AtomicBool>,
    ) -> Result<String> {
        let (endpoint, model) = match &self.config.backend {
            LlmBackend::Ollama { endpoint, model } => (endpoint.clone(), model.clone()),
            _ => unreachable!(),
        };
        let url = format!("{}/api/chat", endpoint);

        let api_messages: Vec<Value> = messages
            .into_iter()
            .map(|(role, content)| json!({ "role": role, "content": content }))
            .collect();

        let body = json!({
            "model": model,
            "messages": api_messages,
            "stream": false,
            "options": {
                "temperature": self.config.temperature,
                "num_predict": self.config.max_tokens,
            }
        });

        let client = self.client.clone();
        let timeout = std::time::Duration::from_secs(self.config.timeout_secs);

        tokio::select! {
            res = client.post(&url).timeout(timeout).json(&body).send() => {
                let resp = res?;
                let status = resp.status();
                if !status.is_success() {
                    let err = resp.text().await?;
                    return Err(anyhow::anyhow!("Ollama {}: {}", status, err));
                }
                let raw = resp.text().await?;
                let data: Value = serde_json::from_str(&raw)?;
                Ok(data["message"]["content"].as_str().unwrap_or("").trim().to_string())
            }
            _ = async {
                while !cancel_flag.load(std::sync::atomic::Ordering::Relaxed) {
                    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                }
            } => Err(anyhow::Error::new(Cancelled))
        }
    }

    async fn chat_openai_compatible(
        &self,
        messages: Vec<(String, String)>,
        cancel_flag: Arc<AtomicBool>,
    ) -> Result<String> {
        let (endpoint, model, api_key) = match &self.config.backend {
            LlmBackend::LlamaCpp { endpoint, model } => (endpoint.clone(), model.clone(), None),
            LlmBackend::OpenAi {
                endpoint,
                model,
                api_key,
            } => (endpoint.clone(), model.clone(), Some(api_key.clone())),
            _ => unreachable!(),
        };
        let url = format!("{}/v1/chat/completions", endpoint);

        let api_messages: Vec<Value> = messages
            .into_iter()
            .map(|(role, content)| json!({ "role": role, "content": content }))
            .collect();

        let mut builder = self
            .client
            .post(&url)
            .timeout(std::time::Duration::from_secs(self.config.timeout_secs));
        if let Some(key) = api_key {
            builder = builder.bearer_auth(key);
        }

        let body = json!({
            "model": model,
            "messages": api_messages,
            "temperature": self.config.temperature,
            "max_tokens": self.config.max_tokens,
            "stream": false
        });

        tokio::select! {
            res = builder.json(&body).send() => {
                let resp = res?;
                let status = resp.status();
                if !status.is_success() {
                    let err = resp.text().await?;
                    return Err(anyhow::anyhow!("Server {}: {}", status, err));
                }
                let raw = resp.text().await?;
                let data: Value = serde_json::from_str(&raw)?;
                let content = data["choices"][0]["message"]["content"]
                    .as_str()
                    .or_else(|| data["choices"][0]["text"].as_str())
                    .ok_or_else(|| anyhow::anyhow!("Unexpected format: {}", raw))?;
                Ok(content.trim().to_string())
            }
            _ = async {
                while !cancel_flag.load(std::sync::atomic::Ordering::Relaxed) {
                    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                }
            } => Err(anyhow::Error::new(Cancelled))
        }
    }

    // ═══════════════════════════════════════════════════════════════════
    // Convenience methods for specific tasks
    // ═══════════════════════════════════════════════════════════════════

    pub async fn check_english_with_cancel(
        &self,
        text: &str,
        cancel_flag: Arc<AtomicBool>,
    ) -> Result<String> {
        let system =
            "You are a raw text corrector. You must reply ONLY with the corrected English text. \
             Do NOT wrap the text in quotes. Do NOT add prefixes like 'Correct:'. \
             Do NOT provide explanations. Just the raw string.";

        let result = self
            .raw_generate_with_cancel(text, system, cancel_flag)
            .await?;

        Ok(Self::clean_response(result))
    }

    pub async fn translate_to_chinese_with_cancel(
        &self,
        text: &str,
        cancel_flag: Arc<AtomicBool>,
    ) -> Result<String> {
        let system = "You are a raw translation engine. Translate the text to Simplified Chinese. \
                      Reply ONLY with the translated text. Do NOT wrap in quotes, do NOT add prefixes, \
                      and do NOT provide explanations.";

        let result = self
            .raw_generate_with_cancel(text, system, cancel_flag)
            .await?;

        Ok(Self::clean_response(result))
    }

    pub async fn translate_to_english_with_cancel(
        &self,
        text: &str,
        cancel_flag: Arc<AtomicBool>,
    ) -> Result<String> {
        let system = "You are a raw translation engine. Translate the text to English. \
                      Reply ONLY with the translated English text. Do NOT wrap in quotes, \
                      do NOT add prefixes, and do NOT provide explanations.";

        let result = self
            .raw_generate_with_cancel(text, system, cancel_flag)
            .await?;

        Ok(Self::clean_response(result))
    }

    /// Core single-turn generation with cancellation
    async fn raw_generate_with_cancel(
        &self,
        prompt: &str,
        system_prompt: &str,
        cancel_flag: Arc<AtomicBool>,
    ) -> Result<String> {
        let messages = vec![
            ("system".to_string(), system_prompt.to_string()),
            ("user".to_string(), prompt.to_string()),
        ];

        self.chat_with_cancel(messages, cancel_flag).await
    }
}
