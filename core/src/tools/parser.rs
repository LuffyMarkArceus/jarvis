//! Multi-format tool call parser for agent loop.
//!
//! Supported shapes (all are accepted anywhere in the model response):
//! 1. Custom / flat:  `{"tool_calls": [{"name": "t", "args": {...}}]}`
//! 2. OpenAI:         `{"tool_calls": [{"type": "function", "function": {"name": "t", "arguments": "{...}"}}]}`
//! 3. Anthropic:      `{"content": [{"type": "tool_use", "name": "t", "input": {...}}]}`
//! 4. Alias:          `{"tools": [{"name": "t", "args": {...}}]}`  (small models emit this)
//! 5. Root level:     `{"type": "tool_use", "name": "t", "input": {...}}`
//! 6. Bare function:  `{"type": "function", "function": {"name": "t", "parameters": {...}}}`
//!    - what llama3.2 emits when it echoes a tool definition as a call.
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
                // The object never closes. A lone missing `}` at the very end
                // is recoverable; a value cut mid-string is not.
                if let Some(repaired) = repair_truncated_json(&response[i..]) {
                    let parsed = try_parse_formats(&repaired).unwrap_or_default();
                    if !parsed.is_empty() {
                        calls.extend(parsed);
                        break;
                    }
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
        Err(_) => match repair_truncated_json(json_str).and_then(|s| serde_json::from_str(&s).ok())
        {
            Some(v) => v,
            None => return Err(()),
        },
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

    // Format 6: a bare OpenAI-style function object at the root. Small
    // models sometimes mistake the tool definition for the call and reply
    // with it directly. Treating it as a call is the honest reading: it is
    // an attempt to act, and permissions decide whether it happens.
    if root_type == Some("function") {
        if let Some(call) = parse_single_call(&v) {
            calls.push(call);
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

/// Close a JSON object that was cut off at a structural boundary.
///
/// Small models regularly run out of completion tokens after writing all the
/// arguments but before the final `}`. Discarding a call whose every value is
/// complete throws away a correct action for no reason.
///
/// The repair is deliberately conservative:
///
/// * it never touches a document that ends inside a string, so a value that
///   was cut mid-way (`"/tmp/fo`) can never be "completed" into the wrong
///   path;
/// * it refuses mismatched brackets and absurd nesting depth.
///
/// Anything that fails these checks stays malformed, and the orchestrator
/// keeps telling the user that nothing ran.
fn repair_truncated_json(input: &str) -> Option<String> {
    const MAX_CLOSERS: usize = 8;

    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escape = false;

    for c in input.chars() {
        if escape {
            escape = false;
            continue;
        }
        if in_string {
            match c {
                '\\' => escape = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            // The guard pops as it compares; a mismatch means the document
            // was never going to parse, so repair is refused.
            '}' | ']' if stack.pop() != Some(c) => return None,
            _ => {}
        }
    }

    if in_string || stack.is_empty() || stack.len() > MAX_CLOSERS {
        return None;
    }

    let mut out = input.trim_end().to_string();
    while let Some(closer) = stack.pop() {
        out.push(closer);
    }
    Some(out)
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

/// True when text looks like a tool call that could not be parsed, such as
/// a JSON object truncated mid-way by the model.
///
/// Used to avoid presenting protocol fragments as if they were an answer.
/// Deliberately narrow: an object that merely contains braces is not enough.
pub fn looks_like_tool_call(response: &str) -> bool {
    let trimmed = response.trim();
    if !trimmed.starts_with('{') {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    let mentions_protocol = lower.contains("\"function\"")
        || lower.contains("\"tool_calls\"")
        || lower.contains("\"tool_use\"");
    mentions_protocol && lower.contains("\"name\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn single(response: &str) -> ToolCall {
        let mut calls = parse_tool_calls(response);
        assert_eq!(calls.len(), 1, "expected one call from {response}");
        calls.remove(0)
    }

    #[test]
    fn parses_the_custom_flat_shape() {
        let call = single(r#"{"tool_calls":[{"name":"filesystem.read","args":{"path":"/tmp"}}]}"#);
        assert_eq!(call.name, "filesystem.read");
        assert_eq!(call.args["path"], "/tmp");
    }

    #[test]
    fn parses_the_openai_array_shape() {
        let call = single(
            r#"{"tool_calls":[{"type":"function","function":{"name":"system.info","arguments":"{}"}}]}"#,
        );
        assert_eq!(call.name, "system.info");
        assert!(call.args.is_object());
    }

    #[test]
    fn parses_a_bare_function_object() {
        // This exact output used to leak to the user as raw JSON.
        let call = single(
            r#"{"type":"function","function":{"name":"filesystem.edit","parameters":{"path":"/tmp/x","old_string":"a","new_string":"b"}}}"#,
        );
        assert_eq!(call.name, "filesystem.edit");
        assert_eq!(call.args["new_string"], "b");
    }

    #[test]
    fn parses_an_anthropic_tool_use_block() {
        let call = single(
            r#"{"content":[{"type":"tool_use","name":"filesystem.stat","input":{"path":"/tmp"}}]}"#,
        );
        assert_eq!(call.name, "filesystem.stat");
        assert_eq!(call.args["path"], "/tmp");
    }

    #[test]
    fn parses_a_root_level_tool_call() {
        let call = single(r#"{"type":"tool_call","name":"process.list","args":{"limit":3}}"#);
        assert_eq!(call.name, "process.list");
        assert_eq!(call.args["limit"], 3);
    }

    #[test]
    fn finds_a_call_embedded_in_prose() {
        let call = single(
            "I will check that now.\n{\"tool_calls\": [{\"name\": \"filesystem.list_dir\", \"args\": {\"path\": \"/tmp\"}}]}\nDone.",
        );
        assert_eq!(call.name, "filesystem.list_dir");
    }

    #[test]
    fn a_call_missing_only_its_final_brace_is_repaired() {
        // The exact 153-byte reply a 1B model gave for "install docker": the
        // arguments are complete and the final `}` is missing. Throwing this
        // away discards a call the model fully specified.
        let truncated = r#"{"type":"function","function":{"name":"filesystem.edit","parameters": {"path": "/usr/local/bin/docker", "old_string": "docker", "new_string": "docker2"}}"#;
        let call = single(truncated);
        assert_eq!(call.name, "filesystem.edit");
        assert_eq!(call.args["new_string"], "docker2");
    }

    #[test]
    fn a_value_cut_mid_string_is_never_completed() {
        // Repairing this would invent the path "/tmp/fo", which the model
        // never asked for. It must stay malformed.
        let truncated = r#"{"type":"function","function":{"name":"filesystem.write","parameters":{"path":"/tmp/fo"#;
        assert!(parse_tool_calls(truncated).is_empty());
        assert!(looks_like_tool_call(truncated));
    }

    #[test]
    fn mismatched_brackets_are_not_repaired() {
        let broken = r#"{"type":"function","function":{"name":"filesystem.read","parameters":{"path":"/tmp"]"#;
        assert!(parse_tool_calls(broken).is_empty());
    }

    #[test]
    fn parses_the_exact_echoed_definition_from_llama() {
        let raw = r#"{"type":"function","function":{"name": "mcp.fetch.fetch","description": "(fetch MCP server) Fetch a URL and return its readable text content (HTML is stripped, scripts and styles removed)", "parameters": {"url": "https://en.wikipedia.org/wiki/Rust_(programming_language)"}}}"#;
        let call = single(raw);
        assert_eq!(call.name, "mcp.fetch.fetch");
        assert_eq!(
            call.args["url"],
            "https://en.wikipedia.org/wiki/Rust_(programming_language)"
        );
    }

    #[test]
    fn ordinary_json_is_not_mistaken_for_a_tool_call() {
        assert!(!looks_like_tool_call(r#"{"name": "Shrey", "age": 30}"#));
        assert!(!looks_like_tool_call("The capital of France is Paris."));
    }

    #[test]
    fn plain_text_yields_no_calls() {
        assert!(parse_tool_calls("The capital of France is Paris.").is_empty());
        assert!(parse_tool_calls("{\"not\": \"a call\"}").is_empty());
    }
}
