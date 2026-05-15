// src/jsonrpc.rs

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
// ============================================================================
// Error Types
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorCode {
    ParseError,
    InvalidRequest,
    MethodNotFound,
    InvalidParams,
    InternalError,
    ServerError(i64),
}

impl ErrorCode {
    pub fn code(&self) -> i64 {
        match self {
            ErrorCode::ParseError => -32700,
            ErrorCode::InvalidRequest => -32600,
            ErrorCode::MethodNotFound => -32601,
            ErrorCode::InvalidParams => -32602,
            ErrorCode::InternalError => -32603,
            ErrorCode::ServerError(c) => *c,
        }
    }
}

impl From<i64> for ErrorCode {
    fn from(code: i64) -> Self {
        match code {
            -32700 => ErrorCode::ParseError,
            -32600 => ErrorCode::InvalidRequest,
            -32601 => ErrorCode::MethodNotFound,
            -32602 => ErrorCode::InvalidParams,
            -32603 => ErrorCode::InternalError,
            c => ErrorCode::ServerError(c),
        }
    }
}

impl<'de> Deserialize<'de> for ErrorCode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let code = i64::deserialize(deserializer)?;
        Ok(ErrorCode::from(code))
    }
}

impl Serialize for ErrorCode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_i64(self.code())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl Error {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidParams, message)
    }

    pub fn internal_error(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InternalError, message)
    }

    pub fn method_not_found(method: &str) -> Self {
        Self::new(
            ErrorCode::MethodNotFound,
            format!("Method not found: {}", method),
        )
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.code.code(), self.message)
    }
}

impl std::error::Error for Error {}

// ============================================================================
// Request ID
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
#[derive(Default)]
pub enum Id {
    #[default]
    Null,
    Num(u64),
    Str(String),
}

impl Id {
    pub fn num(n: u64) -> Self {
        Id::Num(n)
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Id::Null => write!(f, "null"),
            Id::Num(n) => write!(f, "{}", n),
            Id::Str(s) => write!(f, "{}", s),
        }
    }
}

// ============================================================================
// Protocol Version
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Version {
    V2,
}

impl Serialize for Version {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str("2.0")
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "2.0" => Ok(Version::V2),
            _ => Err(serde::de::Error::custom("Invalid JSON-RPC version")),
        }
    }
}

// ============================================================================
// Parameters
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
#[derive(Default)]
pub enum Params {
    #[default]
    None,
    Array(Vec<Value>),
    Object(serde_json::Map<String, Value>),
}

impl Params {
    pub fn none() -> Self {
        Params::None
    }

    pub fn from_value(value: Value) -> Self {
        match value {
            Value::Null => Params::None,
            Value::Array(arr) => Params::Array(arr),
            Value::Object(map) => Params::Object(map),
            other => Params::Array(vec![other]),
        }
    }

    pub fn parse<T: DeserializeOwned>(self) -> Result<T, Error> {
        let value = Value::from(self);
        serde_json::from_value(value).map_err(|e| Error::invalid_params(e.to_string()))
    }

    pub fn is_none(&self) -> bool {
        matches!(self, Params::None)
    }
}

impl From<Params> for Value {
    fn from(params: Params) -> Value {
        match params {
            Params::None => Value::Null,
            Params::Array(a) => Value::Array(a),
            Params::Object(m) => Value::Object(m),
        }
    }
}

// ============================================================================
// Method Call (Request from client)
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MethodCall {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jsonrpc: Option<Version>,
    pub method: String,
    #[serde(default, skip_serializing_if = "Params::is_none")]
    pub params: Params,
    pub id: Id,
}

impl MethodCall {
    pub fn new<P: Serialize>(id: Id, method: impl Into<String>, params: P) -> Result<Self, Error> {
        let params =
            serde_json::to_value(params).map_err(|e| Error::internal_error(e.to_string()))?;
        Ok(Self {
            jsonrpc: Some(Version::V2),
            method: method.into(),
            params: Params::from_value(params),
            id,
        })
    }
}

// ============================================================================
// Notification (No response expected)
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jsonrpc: Option<Version>,
    pub method: String,
    #[serde(default, skip_serializing_if = "Params::is_none")]
    pub params: Params,
}

impl Notification {
    pub fn new<P: Serialize>(method: impl Into<String>, params: P) -> Result<Self, Error> {
        let params =
            serde_json::to_value(params).map_err(|e| Error::internal_error(e.to_string()))?;
        Ok(Self {
            jsonrpc: Some(Version::V2),
            method: method.into(),
            params: Params::from_value(params),
        })
    }
}

// ============================================================================
// Response (From server)
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Success {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jsonrpc: Option<Version>,
    pub result: Value,
    pub id: Id,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Failure {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jsonrpc: Option<Version>,
    pub error: Error,
    pub id: Id,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Output {
    Success(Success),
    Failure(Failure),
}

impl Output {
    pub fn id(&self) -> &Id {
        match self {
            Output::Success(s) => &s.id,
            Output::Failure(f) => &f.id,
        }
    }

    pub fn into_result(self) -> Result<Value, Error> {
        match self {
            Output::Success(s) => Ok(s.result),
            Output::Failure(f) => Err(f.error),
        }
    }
}

// ============================================================================
// Incoming Message (From server)
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum IncomingMessage {
    Output(Output),
    Call(MethodCall),           // Server request
    Notification(Notification), // Server notification
}

impl IncomingMessage {
    pub fn id(&self) -> Option<&Id> {
        match self {
            IncomingMessage::Output(o) => Some(o.id()),
            IncomingMessage::Call(c) => Some(&c.id),
            IncomingMessage::Notification(_) => None,
        }
    }

    pub fn method(&self) -> Option<&str> {
        match self {
            IncomingMessage::Call(c) => Some(&c.method),
            IncomingMessage::Notification(n) => Some(&n.method),
            IncomingMessage::Output(_) => None,
        }
    }
}

// ============================================================================
// Outgoing Message (To server)
// ============================================================================

#[derive(Debug, Clone)]
pub enum OutgoingMessage {
    Request {
        id: Id,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    Response(Output),
}

impl OutgoingMessage {
    pub fn serialize(&self) -> Result<String, Error> {
        let value = match self {
            OutgoingMessage::Request { id, method, params } => serde_json::to_value(MethodCall {
                jsonrpc: Some(Version::V2),
                method: method.clone(),
                params: Params::from_value(params.clone()),
                id: id.clone(),
            }),
            OutgoingMessage::Notification { method, params } => {
                serde_json::to_value(Notification {
                    jsonrpc: Some(Version::V2),
                    method: method.clone(),
                    params: Params::from_value(params.clone()),
                })
            }
            OutgoingMessage::Response(output) => match output {
                Output::Success(s) => serde_json::to_value(Success {
                    jsonrpc: Some(Version::V2),
                    result: s.result.clone(),
                    id: s.id.clone(),
                }),
                Output::Failure(f) => serde_json::to_value(Failure {
                    jsonrpc: Some(Version::V2),
                    error: f.error.clone(),
                    id: f.id.clone(),
                }),
            },
        }
        .map_err(|e| Error::internal_error(e.to_string()))?;

        serde_json::to_string(&value).map_err(|e| Error::internal_error(e.to_string()))
    }
}

// ============================================================================
// Transport Payload
// ============================================================================

#[derive(Debug)]
pub enum Payload {
    Request {
        id: Id,
        response_tx: tokio::sync::oneshot::Sender<Result<Value, Error>>,
        message: String,
    },
    Notification {
        message: String,
    },
    Response {
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_method_call_serialization() {
        let call = MethodCall::new(
            Id::num(1),
            "initialize",
            serde_json::json!({
                "processId": 12345,
                "capabilities": {}
            }),
        )
        .unwrap();

        let json = serde_json::to_string(&call).unwrap();
        assert!(json.contains("\"jsonrpc\":\"2.0\""));
        assert!(json.contains("\"method\":\"initialize\""));
        assert!(json.contains("\"id\":1"));
    }

    #[test]
    fn test_notification_serialization() {
        let notif = Notification::new("initialized", serde_json::json!({})).unwrap();

        let json = serde_json::to_string(&notif).unwrap();
        assert!(json.contains("\"method\":\"initialized\""));
        assert!(!json.contains("\"id\""));
    }

    #[test]
    fn test_output_deserialization() {
        let json = r#"{"jsonrpc":"2.0","result":{"capabilities":{}},"id":1}"#;
        let output: Output = serde_json::from_str(json).unwrap();

        assert!(matches!(output, Output::Success(_)));
        assert_eq!(output.id(), &Id::num(1));
    }

    #[test]
    fn test_error_deserialization() {
        let json =
            r#"{"jsonrpc":"2.0","error":{"code":-32601,"message":"Method not found"},"id":1}"#;
        let output: Output = serde_json::from_str(json).unwrap();

        assert!(matches!(output, Output::Failure(_)));
        if let Output::Failure(f) = output {
            assert_eq!(f.error.code, ErrorCode::MethodNotFound);
        }
    }
}
