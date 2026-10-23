//! Token-budgeted context assembly.
//!
//! # Why this exists
//!
//! A prompt is not free. The provider has a fixed window, and when a prompt
//! is too long Ollama does not complain: it truncates. Measured on Ollama
//! 0.40 with `llama3.2:1b`:
//!
//! * the usable prompt is `num_ctx / 2` (2048 tokens for the default 4096);
//! * truncation drops the **front** of the prompt.
//!
//! The system prompt lives at the front. It contains the rules that stop the
//! assistant claiming actions it did not take. So an over-long prompt does
//! not degrade gracefully - it silently deletes the honesty rules. That is
//! the failure this module prevents: the prompt is measured *before* it is
//! sent, and history is shortened here, on purpose, instead of by the
//! provider by accident.
//!
//! # The budget
//!
//! One request is split into four costs, in priority order:
//!
//! 1. the system prompt - never dropped, never truncated;
//! 2. the current request - never dropped; absurdly long requests are
//!    shortened in the middle with an explicit marker;
//! 3. recent turns, verbatim, newest first;
//! 4. older turns, condensed to one line each.
//!
//! Anything that does not fit in (4) is counted and reported to the model as
//! "N earlier messages omitted" rather than disappearing silently.
//!
//! # Estimation
//!
//! Tokens are estimated as `chars / 4`. That is close for English prose and
//! slightly optimistic for JSON, which is why [`ContextConfig::prompt_budget`]
//! keeps a wide margin below the provider's real limit. The estimate is
//! checked against the provider's own `prompt_eval_count` in the logs, so it
//! can be corrected with evidence instead of faith.

use crate::config::ContextConfig;
use crate::memory::Message;
use crate::model::ChatMessage;

/// Characters per token for the estimator. English sits near 4; code is
/// denser, and the safety margin in `prompt_budget` covers the difference.
const CHARS_PER_TOKEN: usize = 4;

/// Chat templates add a role header and separators around every message.
const MESSAGE_OVERHEAD_TOKENS: usize = 4;

/// Characters reserved for the "[... N characters omitted ...]" marker.
const CLAMP_MARKER_CHARS: usize = 64;

const SUMMARY_INTRO: &str =
    "\n\nBackground from earlier in this conversation (condensed; may be incomplete):\n";
const SUMMARY_OUTRO: &str = "\nTreat that only as background. Answer the current request directly.";

/// The assembled prompt, plus the facts needed to log what was given up.
#[derive(Debug, Clone)]
pub struct ContextPlan {
    /// Base system prompt with the condensed background appended.
    pub system: String,
    /// Verbatim recent turns, oldest first, then the current request.
    pub messages: Vec<ChatMessage>,
    /// Older messages that did not fit even in condensed form.
    pub omitted_messages: usize,
    /// Older messages that made it in as condensed lines.
    pub condensed_messages: usize,
    /// Estimated cost of the whole request.
    pub estimated_tokens: usize,
    /// Whether the current request had to be shortened in the middle.
    pub request_truncated: bool,
}

/// Estimate the token cost of a piece of text.
pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(CHARS_PER_TOKEN)
}

/// Estimate the token cost of a full request.
pub fn total_tokens(system: &str, messages: &[ChatMessage], reserved_tokens: usize) -> usize {
    let mut total = estimate_tokens(system) + reserved_tokens;
    for message in messages {
        total += estimate_chat_message(message);
    }
    total
}

fn estimate_chat_message(message: &ChatMessage) -> usize {
    let mut extra = 0usize;
    if let Some(ref name) = message.tool_name {
        extra += estimate_tokens(name);
    }
    if let Some(ref calls) = message.tool_calls {
        if let Ok(encoded) = serde_json::to_string(calls) {
            extra += estimate_tokens(&encoded);
        }
    }
    MESSAGE_OVERHEAD_TOKENS
        + estimate_tokens(&message.role)
        + estimate_tokens(&message.content)
        + extra
}

fn estimate_message(role: &str, content: &str) -> usize {
    MESSAGE_OVERHEAD_TOKENS + estimate_tokens(role) + estimate_tokens(content)
}

/// Build the prompt for one turn.
///
/// `reserved_tokens` is space the caller already knows it will spend on
/// things this function cannot see - tool schemas, mostly.
pub fn assemble(
    base_system: &str,
    input: &str,
    history: &[Message],
    config: &ContextConfig,
    reserved_tokens: usize,
) -> ContextPlan {
    let budget = config.prompt_budget();
    let mut used = estimate_tokens(base_system) + reserved_tokens;

    // 1. The current request. Kept whole unless it cannot fit; a very long
    //    paste is shortened in the middle so both the context at the top and
    //    the actual question at the bottom survive.
    let request_room = budget.saturating_sub(used + MESSAGE_OVERHEAD_TOKENS);
    let (request, request_truncated) = clamp_request(input, request_room);
    used += MESSAGE_OVERHEAD_TOKENS + estimate_tokens(&request);

    // 2. Only the last `max_history_messages` may appear verbatim.
    let window_start = history.len().saturating_sub(config.max_history_messages);
    let older = &history[..window_start];
    let recent = &history[window_start..];

    // 3. Spend at most two thirds of what is left on verbatim turns when
    //    older turns also need room for their condensed form.
    let remaining = budget.saturating_sub(used);
    let verbatim_cap = if !older.is_empty() && remaining > 0 {
        remaining * 2 / 3
    } else {
        remaining
    };

    let mut kept: Vec<&Message> = Vec::new();
    let mut verbatim_used = 0usize;
    for message in recent.iter().rev() {
        let cost = estimate_message(&message.role, &message.content);
        // Always keep the newest message, even if it alone is over budget:
        // dropping it would leave the model without a conversation.
        if verbatim_used + cost > verbatim_cap && !kept.is_empty() {
            break;
        }
        verbatim_used += cost;
        kept.push(message);
    }
    kept.reverse();

    // 4. Everything older than the kept window is condensed, including
    //    recent turns that did not fit verbatim.
    let dropped_recent = recent.len() - kept.len();
    let to_condense: Vec<&Message> = older
        .iter()
        .chain(recent[..dropped_recent].iter())
        .collect();
    let summary_budget = budget.saturating_sub(used + verbatim_used);
    let (condensed, condensed_count) =
        condense(&to_condense, summary_budget, config.summary_message_chars);
    let omitted_messages = to_condense.len() - condensed_count;

    let mut system = base_system.to_string();
    if let Some(section) = condensed {
        system.push_str(&section);
    }

    let mut messages: Vec<ChatMessage> = Vec::with_capacity(kept.len() + 1);
    for message in &kept {
        messages.push(ChatMessage::text(&message.role, &message.content));
    }
    messages.push(ChatMessage::text("user", &request));

    let estimated_tokens = total_tokens(&system, &messages, reserved_tokens);

    ContextPlan {
        system,
        messages,
        omitted_messages,
        condensed_messages: condensed_count,
        estimated_tokens,
        request_truncated,
    }
}

/// Shorten text to `max_chars`, ending with an explicit marker.
///
/// Used for tool results that go back into the prompt. The beginning of a
/// result matters most (a directory listing, the head of a file), so this
/// keeps the head.
pub fn truncate_with_marker(text: &str, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_string();
    }
    let kept: String = text.chars().take(max_chars).collect();
    format!(
        "{}... [{} more characters not shown]",
        kept.trim_end(),
        total - max_chars
    )
}

fn clamp_request(input: &str, room_tokens: usize) -> (String, bool) {
    // Room for the omission marker, so the shortened request still fits.
    let limit_chars = room_tokens
        .saturating_mul(CHARS_PER_TOKEN)
        .saturating_sub(CLAMP_MARKER_CHARS);
    let total = input.chars().count();
    // Below a sensible floor, shortening the request would destroy it while
    // saving nothing. Send it whole and let the caller log the overspend.
    if total <= limit_chars || limit_chars < 64 {
        return (input.to_string(), false);
    }

    let head_chars = limit_chars * 2 / 3;
    let tail_chars = limit_chars - head_chars;
    let head: String = input.chars().take(head_chars).collect();
    let tail: String = input.chars().skip(total - tail_chars).collect();
    let omitted = total - head_chars - tail_chars;

    (
        format!(
            "{}\n\n[... {} characters omitted to fit the context window ...]\n\n{}",
            head.trim_end(),
            omitted,
            tail.trim_start()
        ),
        true,
    )
}

fn condense(
    messages: &[&Message],
    budget_tokens: usize,
    max_chars: usize,
) -> (Option<String>, usize) {
    if messages.is_empty() {
        return (None, 0);
    }

    // The wrapper text and the omission marker are part of the cost.
    let overhead = estimate_tokens(SUMMARY_INTRO) + estimate_tokens(SUMMARY_OUTRO) + 8;
    let line_budget = budget_tokens.saturating_sub(overhead);

    let mut lines: Vec<String> = Vec::new();
    let mut used = 0usize;
    // Newest first, so the lines closest to the verbatim window survive.
    for message in messages.iter().rev() {
        let line = condense_message(message, max_chars);
        let cost = estimate_tokens(&line);
        if used + cost > line_budget {
            break;
        }
        used += cost;
        lines.push(line);
    }
    if lines.is_empty() {
        return (None, 0);
    }
    lines.reverse();
    let kept = lines.len();
    let omitted = messages.len() - kept;

    let mut section = String::from(SUMMARY_INTRO);
    for line in &lines {
        section.push_str(line);
        section.push('\n');
    }
    if omitted > 0 {
        section.push_str(&format!(
            "({} earlier message{} omitted)\n",
            omitted,
            if omitted == 1 { "" } else { "s" }
        ));
    }
    section.push_str(SUMMARY_OUTRO);

    (Some(section), kept)
}

fn condense_message(message: &Message, max_chars: usize) -> String {
    let speaker = if message.role == "user" {
        "user"
    } else {
        "jarvis"
    };
    format!(
        "- {}: {}",
        speaker,
        one_line_excerpt(&message.content, max_chars)
    )
}

/// Collapse whitespace, prefer a whole first sentence, otherwise cut.
fn one_line_excerpt(text: &str, max_chars: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return "(no text)".to_string();
    }

    let sentence_end = collapsed
        .char_indices()
        .find(|(_, c)| matches!(c, '.' | '!' | '?'))
        .map(|(i, c)| i + c.len_utf8());
    if let Some(end) = sentence_end {
        if end <= max_chars {
            return collapsed[..end].to_string();
        }
    }

    let mut cut: String = collapsed.chars().take(max_chars).collect();
    cut = cut.trim_end().to_string();
    cut.push('\u{2026}');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn message(role: &str, content: &str) -> Message {
        Message {
            id: 0,
            session_id: "test".to_string(),
            role: role.to_string(),
            content: content.to_string(),
            created_at: Utc::now(),
        }
    }

    fn history(pairs: usize, body_chars: usize) -> Vec<Message> {
        let body = "x".repeat(body_chars);
        (0..pairs)
            .flat_map(|i| {
                [
                    message("user", &format!("question {} {}", i, body)),
                    message("assistant", &format!("answer {} {}.", i, body)),
                ]
            })
            .collect()
    }

    fn config() -> ContextConfig {
        ContextConfig::default()
    }

    #[test]
    fn estimate_scales_with_length() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
        // Unicode is counted in characters, not bytes.
        assert_eq!(estimate_tokens("éééé"), 1);
    }

    #[test]
    fn short_history_is_kept_verbatim() {
        let history = history(2, 20);
        let plan = assemble("rules", "current question", &history, &config(), 0);

        assert_eq!(plan.messages.len(), history.len() + 1);
        assert_eq!(plan.messages.last().unwrap().role, "user");
        assert_eq!(plan.messages.last().unwrap().content, "current question");
        assert_eq!(plan.system, "rules");
        assert_eq!(plan.omitted_messages, 0);
        assert!(!plan.request_truncated);
    }

    #[test]
    fn system_prompt_is_never_dropped() {
        let history = history(200, 2000);
        let plan = assemble("HONESTY RULES", "question", &history, &config(), 0);

        assert!(plan.system.starts_with("HONESTY RULES"));
        assert!(plan.estimated_tokens <= config().prompt_budget() + 50);
    }

    #[test]
    fn older_turns_are_condensed_and_counted() {
        let history = history(60, 200);
        let plan = assemble("rules", "question", &history, &config(), 0);

        assert!(plan.system.contains("Background from earlier"));
        assert!(plan.omitted_messages > 0);
        assert!(plan.system.contains("omitted"));
    }

    #[test]
    fn the_most_recent_history_survives() {
        // Each turn is large enough that only a few condensed lines fit.
        let body = "y".repeat(500);
        let mut history = Vec::new();
        for i in 0..40 {
            history.push(message(
                "user",
                &format!("old question number {} {}", i, body),
            ));
            history.push(message(
                "assistant",
                &format!("old answer number {} {}.", i, body),
            ));
        }
        let plan = assemble("rules", "question", &history, &config(), 0);

        // Whatever made it into the prompt must be the tail of the
        // conversation, not the beginning.
        assert!(!plan.system.contains("number 0 "));
        assert!(plan.system.contains("number 33 "));
    }

    #[test]
    fn budget_is_respected_with_a_large_history() {
        let history = history(100, 500);
        let plan = assemble("rules", "question", &history, &config(), 0);
        assert!(
            plan.estimated_tokens <= config().prompt_budget() + 50,
            "estimate {} over budget {}",
            plan.estimated_tokens,
            config().prompt_budget()
        );
    }

    #[test]
    fn max_history_messages_caps_the_verbatim_window() {
        let mut config = config();
        config.max_history_messages = 4;
        let history = history(10, 10);
        let plan = assemble("rules", "question", &history, &config, 0);

        // Four verbatim turns plus the current request.
        assert_eq!(plan.messages.len(), 5);
        assert!(plan.system.contains("Background from earlier"));
    }

    #[test]
    fn a_huge_request_is_shortened_in_the_middle() {
        let input = format!("{}MIDDLE{}", "A".repeat(20_000), "Z".repeat(20_000));
        let plan = assemble("rules", &input, &[], &config(), 0);

        let request = &plan.messages.last().unwrap().content;
        assert!(plan.request_truncated);
        assert!(request.starts_with("AAA"));
        assert!(request.ends_with("ZZZ"));
        assert!(request.contains("characters omitted"));
    }

    #[test]
    fn a_normal_request_is_untouched() {
        let plan = assemble("rules", "what time is it?", &[], &config(), 0);
        assert!(!plan.request_truncated);
        assert_eq!(plan.messages.last().unwrap().content, "what time is it?");
    }

    #[test]
    fn assembly_is_deterministic() {
        let history = history(50, 100);
        let first = assemble("rules", "question", &history, &config(), 0);
        let second = assemble("rules", "question", &history, &config(), 0);

        assert_eq!(first.system, second.system);
        assert_eq!(first.omitted_messages, second.omitted_messages);
        assert_eq!(first.estimated_tokens, second.estimated_tokens);
        assert_eq!(
            first
                .messages
                .iter()
                .map(|m| m.content.clone())
                .collect::<Vec<_>>(),
            second
                .messages
                .iter()
                .map(|m| m.content.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn condensed_lines_are_one_line_each() {
        let history = vec![
            message("user", "line one\nline two\nline three"),
            message("assistant", "Yes."),
        ];
        let mut config = config();
        config.max_history_messages = 0;
        config.max_history_fetch = 50;
        let plan = assemble("rules", "question", &history, &config, 0);

        let background = plan.system.split("Background").nth(1).unwrap();
        assert!(!background.contains("\nline two"));
        assert!(background.contains("- user: line one line two line three"));
    }

    #[test]
    fn truncate_with_marker_keeps_the_head() {
        let text = "abcdefghij";
        assert_eq!(truncate_with_marker(text, 20), text);
        let cut = truncate_with_marker(text, 4);
        assert!(cut.starts_with("abcd"));
        assert!(cut.contains("6 more characters not shown"));
    }

    #[test]
    fn reserved_tokens_reduce_the_history_budget() {
        let history = history(30, 200);
        let plain = assemble("rules", "question", &history, &config(), 0);
        let reserved = assemble("rules", "question", &history, &config(), 900);

        // Compare prompt content only; `estimated_tokens` includes the
        // reservation, which is space the caller spends elsewhere.
        let plain_content = total_tokens(&plain.system, &plain.messages, 0);
        let reserved_content = total_tokens(&reserved.system, &reserved.messages, 0);
        assert!(reserved_content < plain_content);
        assert!(reserved.omitted_messages >= plain.omitted_messages);
    }
}
