//! IPC protocol definitions for daemon <-> client communication

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: Uuid,
    pub method: String,
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: Uuid,
    pub ok: bool,
    pub result: Option<Value>,
    pub error: Option<String>,
}

impl Request {
    pub fn new(method: &str, params: Value) -> Self {
        Self {
            id: Uuid::new_v4(),
            method: method.to_string(),
            params,
        }
    }
}

impl Response {
    pub fn ok(id: Uuid, result: Value) -> Self {
        Self {
            id,
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Uuid, error: String) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(error),
        }
    }
}
