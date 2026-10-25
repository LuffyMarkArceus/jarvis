//! Deterministic checks on what a model claimed, against what tools returned.
//!
//! # Why
//!
//! A small local model composes fluent sentences but is unreliable about
//! specifics. Measured on this machine: asked for the latest Rust release, it
//! fetched a page containing "1.99.1 / October 15, 2026" and answered that
//! the release was "1.99.1, released in 2023". Both the version and the
//! sentence structure were plausible; the year was invented.
//!
//! Prompting cannot fix this reliably - the model is not choosing to lie, it
//! is predicting tokens - so the check is done in code. The answer is still
//! the model's, but figures that do not appear anywhere in the retrieved
//! material are called out instead of being presented as sourced.
//!
//! # Scope
//!
//! Only years and dotted versions are checked. They are high-signal: a wrong
//! year is a factual error, whereas a wrong small count is usually a summary
//! ("3 problems") that may legitimately be derived rather than quoted. Being
//! conservative keeps the note rare, which is what makes it worth reading.

/// A figure the answer states that the sources do not support.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unverified {
    pub figure: String,
}

/// Years and versions in `answer` that appear nowhere in `sources`.
///
/// `sources` should include everything the model was allowed to draw on: the
/// tool outputs *and* the user's request. A number the user supplied is not a
/// hallucination when the answer repeats it.
pub fn unverified_figures(answer: &str, sources: &[&str]) -> Vec<String> {
    let allowed = source_tokens(sources);
    let mut out: Vec<String> = Vec::new();

    for candidate in figures(answer) {
        if !is_supported(&candidate, &allowed) && !out.contains(&candidate) {
            out.push(candidate);
        }
    }
    out
}

/// Render the caveat appended to an answer, or `None` when everything checks
/// out.
pub fn caveat(unverified: &[String]) -> Option<String> {
    match unverified {
        [] => None,
        [one] => Some(format!(
            "Note: {} does not appear in the sources I read, so treat it with caution.",
            one
        )),
        many => Some(format!(
            "Note: {} do not appear in the sources I read, so treat them with caution.",
            join(many)
        )),
    }
}

/// Append a caveat to `answer` when it states unsupported figures.
pub fn with_caveat(answer: String, sources: &[&str]) -> String {
    match caveat(&unverified_figures(&answer, sources)) {
        Some(note) => format!("{}\n\n{}", answer.trim_end(), note),
        None => answer,
    }
}

fn join(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [first, second] => format!("{} and {}", first, second),
        _ => {
            let (last, rest) = items.split_last().expect("non-empty");
            format!("{}, and {}", rest.join(", "), last)
        }
    }
}

/// Every token in the sources, plus the numbers inside them.
fn source_tokens(sources: &[&str]) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    for source in sources {
        for token in split_tokens(source) {
            tokens.push(token);
        }
    }
    tokens
}

fn is_supported(candidate: &str, allowed: &[String]) -> bool {
    allowed.iter().any(|token| {
        if token == candidate {
            return true;
        }
        // "1.99" is a truncation of a sourced "1.99.1", not an invention.
        candidate.contains('.')
            && token.starts_with(candidate)
            && token[candidate.len()..].starts_with('.')
    })
}

/// Four-digit years and dotted versions, in order of appearance.
fn figures(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if !bytes[index].is_ascii_digit() {
            index += 1;
            continue;
        }
        let start = index;
        while index < bytes.len() && (bytes[index].is_ascii_digit() || bytes[index] == b'.') {
            index += 1;
        }
        let mut end = index;
        // Never end a figure on a sentence's full stop.
        while end > start && bytes[end - 1] == b'.' {
            end -= 1;
        }
        let token = &text[start..end];
        if token.is_empty() || !is_standalone(bytes, start, end) {
            continue;
        }
        if is_year(token) || is_version(token) {
            out.push(token.to_string());
        }
    }
    out
}

fn is_standalone(bytes: &[u8], start: usize, end: usize) -> bool {
    let before = start.checked_sub(1).map(|i| bytes[i]).unwrap_or(b' ');
    let after = bytes.get(end).copied().unwrap_or(b' ');
    !before.is_ascii_alphanumeric() && after != b'-' && after != b'x' && after != b'X'
}

fn is_year(token: &str) -> bool {
    token.len() == 4
        && token.bytes().all(|b| b.is_ascii_digit())
        && (1900..=2099).contains(&token.parse::<u32>().expect("digits"))
}

fn is_version(token: &str) -> bool {
    let parts: Vec<&str> = token.split('.').collect();
    parts.len() >= 2
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// Split into runs of alphanumerics and dots, dropping trailing dots.
fn split_tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '.')
        .map(|s| s.trim_matches('.'))
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = "Stable release 1.99.1 / October 15, 2026. First appeared in 2012.";

    #[test]
    fn a_year_that_is_not_in_the_page_is_reported() {
        let out = unverified_figures("Rust 1.99.1 was released in 2023.", &[PAGE]);
        assert_eq!(out, vec!["2023"]);
    }

    #[test]
    fn a_fully_sourced_answer_has_no_caveat() {
        let answer = "Rust 1.99.1 was released on October 15, 2026.";
        assert!(unverified_figures(answer, &[PAGE]).is_empty());
        assert!(caveat(&[]).is_none());
    }

    #[test]
    fn a_version_truncated_by_the_answer_is_supported() {
        let out = unverified_figures("The release is 1.99.", &[PAGE]);
        assert!(out.is_empty(), "1.99 is a truncation of 1.99.1");
    }

    #[test]
    fn a_version_that_is_not_in_the_page_is_reported() {
        let out = unverified_figures("The release is 2.0.3.", &[PAGE]);
        assert_eq!(out, vec!["2.0.3"]);
    }

    #[test]
    fn numbers_the_user_supplied_are_supported() {
        let out = unverified_figures(
            "In 1999 that was different.",
            &[PAGE, "what changed in 1999"],
        );
        assert!(out.is_empty());
    }

    #[test]
    fn figures_are_reported_once_and_in_order() {
        let out = unverified_figures("In 2023 and again in 2023, then 2024.", &[PAGE]);
        assert_eq!(out, vec!["2023", "2024"]);
    }

    #[test]
    fn a_sentence_final_period_is_not_part_of_a_version() {
        let out = unverified_figures("It is version 3.2.", &["nothing here"]);
        assert_eq!(out, vec!["3.2"]);
    }

    #[test]
    fn plain_small_numbers_are_not_checked() {
        let out = unverified_figures("There are 7 of them and 3 are solved.", &[PAGE]);
        assert!(out.is_empty());
    }

    #[test]
    fn digits_inside_a_longer_word_are_not_figures() {
        let out = unverified_figures("model llama3.2:1b and id 120261", &[PAGE]);
        assert!(out.is_empty(), "got {:?}", out);
    }

    #[test]
    fn years_outside_the_range_are_not_figures() {
        // A port number is not a year.
        let out = unverified_figures("listening on 3000", &[PAGE]);
        assert!(out.is_empty());
    }

    #[test]
    fn the_caveat_lists_every_unverified_figure() {
        let note = caveat(&["2023".to_string(), "2.0.3".to_string()]).expect("note");
        assert!(note.contains("2023"));
        assert!(note.contains("2.0.3"));
        assert!(note.contains("do not appear"));
    }

    #[test]
    fn the_caveat_uses_the_singular_form_for_one_figure() {
        let note = caveat(&["2023".to_string()]).expect("note");
        assert!(note.contains("does not appear"));
    }

    #[test]
    fn with_caveat_leaves_a_correct_answer_untouched() {
        let answer = "Rust 1.99.1 was released in 2026.".to_string();
        assert_eq!(with_caveat(answer.clone(), &[PAGE]), answer);
    }

    #[test]
    fn with_caveat_appends_after_the_answer() {
        let out = with_caveat("Rust 1.99.1 came out in 2023.".to_string(), &[PAGE]);
        assert!(out.starts_with("Rust 1.99.1 came out in 2023."));
        assert!(out.contains("2023 does not appear"));
    }
}
