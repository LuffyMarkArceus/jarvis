//! Multi-format tool call parser for agent loop

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub name: String,
    pub args: Value,
}

/// Parse tool calls from model response supporting multiple formats:
/// 1. Custom format: {"tool_calls": [{"name": "...", "args": {...}}]}
/// 2. OpenAI style: {"tool_calls": [{"type": "function", "function": {"name": "...", "arguments": "..."}}]}
/// 3. Anthropic style: {"type": "thought"/"tool_call", "tool": "...", "input": {...}} or tool_use blocks
pub fn parse_tool_calls(response: &str) -> Vec<ToolCall> {
    let mut calls = Vec::new();

    // Try to find JSON blocks (```json ... ``` or just {...})
    // Look for code blocks first
    if let Some(start) = response.find("```json") {
        if let Some(end) = response[start..].find("```") {
            let json_str = &response[start + 7..start + end].trim();
            if let Ok(parsed) = try_parse_formats(json_str) {
                calls.extend(parsed);
            }
        }
    } else if let Some(start) = response.find("```") {
        // Try any code block
        let after = &response[start + 3..];
        // Skip language identifier if present
        let json_start = if let Some(newline) = after.find('\n') {
            start + 3 + newline + 1
        } else {
            start + 3
        };
        if let Some(end) = response[json_start..].find("```") {
            let json_str = &response[json_start..json_start + end].trim();
            if let Ok(parsed) = try_parse_formats(json_str) {
                calls.extend(parsed);
            }
        }
    }

    // Try to find raw JSON objects with tool_calls
    if calls.is_empty() {
        // Look for { ... "tool_calls" ... }
        let mut i = 0;
        let bytes = response.as_bytes();
        while i < bytes.len() {
            if bytes[i] == b'{' {
                // Try to extract balanced JSON object
                if let Some((json_str, end)) = extract_json_object(response, i) {
                    if let Ok(parsed) = try_parse_formats(&json_str) {
                        if !parsed.is_empty() {
                            calls.extend(parsed);
                            break;
                        }
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
        if c == b'{' {
            depth += 1;
        } else if c == b'}' {
            depth -= 1;
            if depth == 0 {
                return Some((s[start..=i].to_string(), i + 1));
            }
        }
        i += 1;
    }
    None
}

fn try_parse_formats(json_str: &str) -> Result<Vec<ToolCall>, ()> {
    let mut calls = Vec::new();

    // Try to parse as generic Value first
    if let Ok(v) = serde_json::from_str::<Value>(json_str) {
        // Format 1: Custom format with tool_calls
        if let Some(tool_calls) = v.get("tool_calls").and_then(|t| t.as_array()) {
            for tc in tool_calls {
                // Try custom format: {name: "...", args: {...}}
                if let Some(name) = tc.get("name").and_then(|n| n.as_str()) {
                    let args = tc
                        .get("args")
                        .cloned()
                        .unwrap_or(Value::Object(Default::default()));
                    calls.push(ToolCall {
                        name: name.to_string(),
                        args,
                    });
                    continue;
                }
                // Try OpenAI format: {type: "function", function: {name: "...", arguments: "..."}}
                if tc.get("type").and_then(|t| t.as_str()) == Some("function") {
                    if let Some(func) = tc.get("function") {
                        if let Some(name) = func.get("name").and_then(|n| n.as_str()) {
                            let args = if let Some(args_str) =
                                func.get("arguments").and_then(|a| a.as_str())
                            {
                                serde_json::from_str(args_str)
                                    .unwrap_or(Value::Object(Default::default()))
                            } else if let Some(args_obj) = func.get("arguments") {
                                args_obj.clone()
                            } else {
                                Value::Object(Default::default())
                            };
                            calls.push(ToolCall {
                                name: name.to_string(),
                                args,
                            });
                            continue;
                        }
                    }
                }
                // Try OpenAI format without type: {function: {name, arguments}}
                if let Some(func) = tc.get("function") {
                    if let Some(name) = func.get("name").and_then(|n| n.as_str()) {
                        let args = if let Some(args_str) =
                            func.get("arguments").and_then(|a| a.as_str())
                        {
                            serde_json::from_str(args_str)
                                .unwrap_or(Value::Object(Default::default()))
                        } else if let Some(args_obj) = func.get("arguments") {
                            args_obj.clone()
                        } else {
                            Value::Object(Default::default())
                        };
                        calls.push(ToolCall {
                            name: name.to_string(),
                            args,
                        });
                        continue;
                    }
                }
            }
        }

        // Format 3: Anthropic style - tool_use
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
                            args,
                        });
                    }
                }
            }
        }

        // Direct tool_call/tool_use at root level
        if v.get("type").and_then(|t| t.as_str()) == Some("tool_call")
            || v.get("type").and_then(|t| t.as_str()) == Some("tool_use")
        {
            if let Some(name) = v
                .get("name")
                .or_else(|| v.get("tool"))
                .and_then(|n| n.as_str())
            {
                let args = v
                    .get("args")
                    .or_else(|| v.get("input"))
                    .or_else(|| v.get("arguments"))
                    .cloned()
                    .unwrap_or(Value::Object(Default::default()));
                let args = if args.is_string() {
                    serde_json::from_str(args.as_str().unwrap())
                        .unwrap_or(Value::Object(Default::default()))
                } else {
                    args
                };
                calls.push(ToolCall {
                    name: name.to_string(),
                    args,
                });
            }
        }
    }

    if calls.is_empty() {
        Err(())
    } else {
        Ok(calls)
    }
}
