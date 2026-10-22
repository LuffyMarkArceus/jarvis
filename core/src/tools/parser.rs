//! Multi-format tool call parser for agent loop.
//!
//! Supported shapes (all are accepted anywhere in the model response):
//! 1. Custom / flat:  `{"tool_calls": [{"name": "t", "args": {...}}]}`
//! 2. OpenAI:         `{"tool_calls": [{"type": "function", "function": {"name": "t", "arguments": "{...}"}}]}`
//! 3. Anthropic:      `{"content": [{"type": "tool_use", "name": "t", "input": {...}}]}`
//! 4. Alias:          `{"tools": [{"name": "t", "args": {...}}]}`  (small models emit this)
//! 5. Root level:     `{"type": "tool_use", "name": "t", "input": {...}}`
//!
//! Design principle: be permissive when parsing, strict when executing.
//! The parser never executes anything - it only extracts intent. Actual
//! authorisation happens in the orchestrator via the PermissionManager.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub name: String,
    pub args: Value,
}

/// Parse tool calls from a model response supporting multiple formats.
pub fn parse_tool_calls(response: &str) -> Vec<ToolCall> {
    let mut calls = Vec::new();

    // 1) Fenced code blocks first: ```json { ... } ```
    if let Some(start) = response.find("```json") {
        if let Some(end) = response[start + 7..].find("```") {
            let json_str = response[start + 7..start + 7 + end].trim();
            calls.extend(try_parse_formats(json_str).unwrap_or_default());
        }
    }

    // 2) Any other fenced block
    if calls.is_empty() {
        if let Some(start) = response.find("```") {
            let after = &response[start + 3..];
            let body_start = match after.find('\n') {
                Some(nl) => start + 3 + nl + 1,
                None => start + 3,
            };
            if let Some(end) = response[body_start..].find("```") {
                let json_str = response[body_start..body_start + end].trim();
                calls.extend(try_parse_formats(json_str).unwrap_or_default());
            }
        }
    }

    // 3) Raw JSON objects embedded in prose
    if calls.is_empty() {
        let bytes = response.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'{' {
                if let Some((json_str, end)) = extract_json_object(response, i) {
                    let parsed = try_parse_formats(&json_str).unwrap_or_default();
                    if !parsed.is_empty() {
                        calls.extend(parsed);
                        break;
                    }
                    i = end;
                    continue;
                }
            }
            i += 1;
        }
    }

    calls
}

/// Extract a balanced `{...}` JSON object starting at `start`.
fn extract_json_object(s: &str, start: usize) -> Option<(String, usize)> {
    let bytes = s.as_bytes();
    let mut depth = 0;
    let mut in_string = false;
    let mut escape = false;
    let mut i = start;
    while i < bytes.len() {
        let c = bytes[i];
        if escape {
            escape = false;
            i += 1;
            continue;
        }
        if c == b'\\' && in_string {
            escape = true;
            i += 1;
            continue;
        }
        if c == b'"' {
            in_string = !in_string;
            i += 1;
            continue;
        }
        if in_string {
            i += 1;
            continue;
        }
        match c {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((s[start..=i].to_string(), i + 1));
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn try_parse_formats(json_str: &str) -> Result<Vec<ToolCall>, ()> {
    let mut calls = Vec::new();

    let v: Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return Err(()),
    };

    // Format 1, 2 and 4: arrays of calls under "tool_calls" or "tools".
    let arrays: Vec<&Vec<Value>> = [
        v.get("tool_calls").and_then(|t| t.as_array()),
        v.get("tools").and_then(|t| t.as_array()),
    ]
    .into_iter()
    .flatten()
    .collect();

    for arr in arrays {
        for tc in arr {
            if let Some(call) = parse_single_call(tc) {
                calls.push(call);
            }
        }
    }

    // Format 3: Anthropic tool_use blocks inside "content".
    if let Some(content) = v.get("content").and_then(|c| c.as_array()) {
        for item in content {
            if item.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                if let Some(name) = item.get("name").and_then(|n| n.as_str()) {
                    let args = item
                        .get("input")
                        .cloned()
                        .unwrap_or(Value::Object(Default::default()));
                    calls.push(ToolCall {
                        name: name.to_string(),
                        args: normalize_args(args),
                    });
                }
            }
        }
    }

    // Format 5: root-level tool call object.
    let root_type = v.get("type").and_then(|t| t.as_str());
    if matches!(
        root_type,
        Some("tool_call") | Some("tool_use") | Some("function_call")
    ) {
        let name = v
            .get("name")
            .or_else(|| v.get("tool"))
            .or_else(|| v.get("function"))
            .and_then(|n| n.as_str());
        if let Some(name) = name {
            let args = v
                .get("args")
                .or_else(|| v.get("input"))
                .or_else(|| v.get("arguments"))
                .or_else(|| v.get("parameters"))
                .cloned()
                .unwrap_or(Value::Object(Default::default()));
            calls.push(ToolCall {
                name: name.to_string(),
                args: normalize_args(args),
            });
        }
    }

    if calls.is_empty() {
        Err(())
    } else {
        Ok(calls)
    }
}

/// Parse one entry of a tool_calls/tools array.
fn parse_single_call(tc: &Value) -> Option<ToolCall> {
    // Flat custom style: {name, args|input|arguments|parameters}
    if let Some(name) = tc.get("name").and_then(|n| n.as_str()) {
        let args = tc
            .get("args")
            .or_else(|| tc.get("input"))
            .or_else(|| tc.get("arguments"))
            .or_else(|| tc.get("parameters"))
            .cloned()
            .unwrap_or(Value::Object(Default::default()));
        return Some(ToolCall {
            name: name.to_string(),
            args: normalize_args(args),
        });
    }

    // OpenAI style: {type: "function", function: {name, arguments}}
    if let Some(func) = tc.get("function") {
        if let Some(name) = func.get("name").and_then(|n| n.as_str()) {
            let args = func
                .get("arguments")
                .or_else(|| func.get("input"))
                .or_else(|| func.get("parameters"))
                .cloned()
                .unwrap_or(Value::Object(Default::default()));
            return Some(ToolCall {
                name: name.to_string(),
                args: normalize_args(args),
            });
        }
    }

    // Flat function_call style: {function: "name", arguments: {...}}
    if let Some(name) = tc.get("function").and_then(|n| n.as_str()) {
        let args = tc
            .get("arguments")
            .or_else(|| tc.get("args"))
            .or_else(|| tc.get("parameters"))
            .cloned()
            .unwrap_or(Value::Object(Default::default()));
        return Some(ToolCall {
            name: name.to_string(),
            args: normalize_args(args),
        });
    }

    None
}

/// Tool arguments are sometimes a JSON-encoded string rather than an object.
fn normalize_args(args: Value) -> Value {
    match &args {
        Value::String(s) => {
            serde_json::from_str(s).unwrap_or_else(|_| Value::Object(Default::default()))
        }
        _ => args,
    }
}

/// Remove tool-call JSON blocks from a final answer so users never see raw
/// protocol artefacts. Returns the cleaned text.
pub fn strip_tool_artifacts(response: &str) -> String {
    let mut cleaned = String::new();
    let mut rest = response;

    loop {
        let fence = rest.find("```");
        let Some(fence) = fence else {
            cleaned.push_str(rest);
            break;
        };
        let after = &rest[fence + 3..];
        let body_start = match after.find('\n') {
            Some(nl) => fence + 3 + nl + 1,
            None => fence + 3,
        };
        let Some(end) = rest[body_start..].find("```") else {
            cleaned.push_str(rest);
            break;
        };
        let block = &rest[body_start..body_start + end];
        // Keep the block if it does not look like a tool call.
        let looks_like_tool_call = block.contains("tool_calls")
            || block.contains("tool_use")
            || block.contains("\"function\"")
            || block.contains("\"tools\"");
        cleaned.push_str(&rest[..fence]);
        if !looks_like_tool_call {
            cleaned.push_str(&rest[fence..body_start + end + 3]);
        }
        rest = &rest[body_start + end + 3..];
    }

    cleaned.trim().to_string()
}
