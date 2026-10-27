//! Replacing secrets and personal data with placeholders before a request
//! leaves the machine.
//!
//! Only what a pattern finds reliably is handled: key formats, credentials
//! named as such, email addresses, phone numbers, card numbers and IP
//! addresses. Names, street addresses and free-text personal details are not
//! detected (see docs/design/model-tiers.md); claiming otherwise would be a
//! promise the code does not keep.

use regex::{Captures, Regex};
use std::sync::OnceLock;

/// What a placeholder stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Secret,
    Email,
    Phone,
    Card,
    Ip,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Secret => "SECRET",
            Kind::Email => "EMAIL",
            Kind::Phone => "PHONE",
            Kind::Card => "CARD",
            Kind::Ip => "IP",
        }
    }
}

/// A pattern and the kind of value it finds. When the pattern has a group
/// named `v`, only that group is replaced: in `API_KEY=abc` the name stays,
/// so the remote model still knows what kind of value was there.
struct Rule {
    kind: Kind,
    pattern: Regex,
    /// Extra test a match must pass, for formats a pattern alone over-matches.
    accept: fn(&str) -> bool,
}

/// Applied in order. Secrets first, so a key that happens to look like a
/// phone number is still called a secret; placeholders already inserted
/// contain no digits sequence the later rules could match.
fn rules() -> &'static [Rule] {
    static RULES: OnceLock<Vec<Rule>> = OnceLock::new();
    RULES.get_or_init(|| {
        let rule = |kind, pattern: &str, accept| Rule {
            kind,
            pattern: Regex::new(pattern).expect("valid redaction pattern"),
            accept,
        };
        let any: fn(&str) -> bool = |_| true;
        vec![
            rule(
                Kind::Secret,
                r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
                any,
            ),
            // Provider key formats: OpenAI/Anthropic, GitHub, AWS, Slack, Google.
            rule(
                Kind::Secret,
                r"\b(?:sk-[A-Za-z0-9_\-]{20,}|gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{22,}|AKIA[0-9A-Z]{16}|xox[abprs]-[A-Za-z0-9\-]{10,}|AIza[0-9A-Za-z_\-]{35})",
                any,
            ),
            rule(
                Kind::Secret,
                r"\beyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}",
                any,
            ),
            rule(Kind::Secret, r"(?i)\bbearer\s+(?P<v>[A-Za-z0-9._~+/\-]{16,}=*)", any),
            // A value whose name says what it is: API_KEY=..., db_password: ...
            // Name segments are whole, so "keyboard" and "monkey" do not count.
            rule(
                Kind::Secret,
                r#"(?i)\b(?:[a-z0-9]+_)*(?:api_?key|key|token|secret|password|passwd|pwd|passphrase)(?:_[a-z0-9]+)*\s*[:=]\s*["']?(?P<v>[^\s"',]{4,})"#,
                any,
            ),
            rule(
                Kind::Secret,
                r"(?i)\b(?:password|passcode|passphrase|pin)\s+is\s+(?P<v>[^\s,.;]{4,})",
                any,
            ),
            rule(
                Kind::Email,
                r"\b[A-Za-z0-9._%+\-]+@[A-Za-z0-9\-]+(?:\.[A-Za-z0-9\-]+)*\.[A-Za-z]{2,}\b",
                any,
            ),
            rule(Kind::Card, r"\b\d(?:[ \-]?\d){12,18}\b", luhn),
            rule(
                Kind::Ip,
                r"\b(?:(?:25[0-5]|2[0-4]\d|1?\d?\d)\.){3}(?:25[0-5]|2[0-4]\d|1?\d?\d)\b",
                |ip| !ip.starts_with("127.") && ip != "0.0.0.0",
            ),
            // International with a +, or the 3-3-4 North American shape.
            // Dates (2026-10-27) and versions (1.93.1) fit neither.
            rule(
                Kind::Phone,
                r"(?:\+\d{1,3}[ .\-]?(?:\(\d{1,4}\)[ .\-]?)?\d{2,4}(?:[ .\-]\d{2,4}){1,3}|\(?\b\d{3}\)?[ .\-]\d{3}[ .\-]\d{4}\b)",
                any,
            ),
        ]
    })
}

fn placeholder_pattern() -> &'static Regex {
    static PLACEHOLDER: OnceLock<Regex> = OnceLock::new();
    PLACEHOLDER.get_or_init(|| Regex::new(r"<(SECRET|EMAIL|PHONE|CARD|IP)_(\d+)>").expect("valid"))
}

fn is_placeholder(value: &str) -> bool {
    placeholder_pattern()
        .find(value)
        .is_some_and(|m| m.start() == 0 && m.end() == value.len())
}

/// Card numbers carry a checksum; order and tracking numbers usually fail it.
fn luhn(candidate: &str) -> bool {
    let digits: Vec<u32> = candidate.chars().filter_map(|c| c.to_digit(10)).collect();
    if !(13..=19).contains(&digits.len()) {
        return false;
    }
    let sum: u32 = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &d)| match (i % 2 == 1, d * 2) {
            (true, doubled) if doubled > 9 => doubled - 9,
            (true, doubled) => doubled,
            (false, _) => d,
        })
        .sum();
    sum.is_multiple_of(10)
}

/// One turn's redactions. The same value gets the same placeholder for the
/// whole turn, so the remote model can still tell two emails apart.
#[derive(Debug, Default)]
pub struct Redactor {
    found: Vec<(Kind, String)>,
}

impl Redactor {
    pub fn new() -> Self {
        Self::default()
    }

    /// `text` with every value the rules find replaced by `<KIND_n>`.
    pub fn redact(&mut self, text: &str) -> String {
        let mut out = text.to_string();
        for rule in rules() {
            out = rule
                .pattern
                .replace_all(&out, |caps: &Captures| {
                    let whole = caps.get(0).expect("group 0 always matches");
                    let value = caps.name("v").unwrap_or(whole);
                    // Already a placeholder: text quoted back from an earlier
                    // request is not a new secret.
                    if is_placeholder(value.as_str()) || !(rule.accept)(value.as_str()) {
                        return whole.as_str().to_string();
                    }
                    let placeholder = self.placeholder(rule.kind, value.as_str());
                    let start = value.start() - whole.start();
                    let end = value.end() - whole.start();
                    let w = whole.as_str();
                    format!("{}{}{}", &w[..start], placeholder, &w[end..])
                })
                .into_owned();
        }
        out
    }

    /// Put personal data back into an answer. Secrets stay as placeholders:
    /// a key shown in an answer is a leak on screen even if it never left.
    pub fn restore(&self, text: &str) -> String {
        placeholder_pattern()
            .replace_all(text, |caps: &Captures| {
                let n: usize = caps[2].parse().unwrap_or(0);
                let restored = self
                    .found
                    .iter()
                    .filter(|(kind, _)| kind.label() == &caps[1] && *kind != Kind::Secret)
                    .nth(n.wrapping_sub(1))
                    .map(|(_, value)| value.clone());
                restored.unwrap_or_else(|| caps[0].to_string())
            })
            .into_owned()
    }

    /// Distinct values replaced so far.
    pub fn count(&self) -> usize {
        self.found.len()
    }

    fn placeholder(&mut self, kind: Kind, value: &str) -> String {
        let same_kind: Vec<&String> = self
            .found
            .iter()
            .filter(|(k, _)| *k == kind)
            .map(|(_, v)| v)
            .collect();
        let n = match same_kind.iter().position(|v| *v == value) {
            Some(i) => i + 1,
            None => {
                let n = same_kind.len() + 1;
                self.found.push((kind, value.to_string()));
                n
            }
        };
        format!("<{}_{}>", kind.label(), n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redacted(text: &str) -> String {
        Redactor::new().redact(text)
    }

    /// The value is gone and a placeholder of the right kind is in its place.
    fn hides(text: &str, value: &str, placeholder: &str) {
        let out = redacted(text);
        assert!(!out.contains(value), "{:?} still in {:?}", value, out);
        assert!(out.contains(placeholder), "no {} in {:?}", placeholder, out);
    }

    #[test]
    fn key_formats_are_hidden() {
        hides(
            "key sk-proj-AbCdEf0123456789AbCdEf0123",
            "sk-proj-AbCdEf",
            "<SECRET_1>",
        );
        hides(
            "token ghp_0123456789abcdefghijABCDEFGHIJ012345",
            "ghp_0123",
            "<SECRET_1>",
        );
        hides(
            "AKIAIOSFODNN7EXAMPLE is the id",
            "AKIAIOSFODNN7EXAMPLE",
            "<SECRET_1>",
        );
        hides(
            "slack xoxb-1234567890-abcdefghij",
            "xoxb-1234",
            "<SECRET_1>",
        );
    }

    #[test]
    fn a_private_key_block_is_hidden_whole() {
        let pem = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXk\nAAAA\n-----END OPENSSH PRIVATE KEY-----";
        let out = redacted(&format!("here:\n{}\nthanks", pem));
        assert!(!out.contains("b3BlbnNzaC1rZXk"), "{}", out);
        assert!(out.starts_with("here:\n<SECRET_1>\nthanks"), "{}", out);
    }

    #[test]
    fn bearer_tokens_and_jwts_are_hidden() {
        hides(
            "Authorization: Bearer abcDEF123456ghiJKL789",
            "abcDEF123456",
            "<SECRET_1>",
        );
        hides(
            "jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0In0.dozjgNryP4J3jVmNHl0w5N",
            "eyJhbGciOiJIUzI1NiJ9",
            "<SECRET_1>",
        );
    }

    #[test]
    fn values_of_credentials_named_as_such_are_hidden() {
        hides(
            "OPENAI_API_KEY=abc123xyz789",
            "abc123xyz789",
            "OPENAI_API_KEY=<SECRET_1>",
        );
        hides("db_password: \"hunter22\"", "hunter22", "<SECRET_1>");
        hides(
            "export GITHUB_TOKEN='t0k3n-value'",
            "t0k3n-value",
            "<SECRET_1>",
        );
        hides("my password is hunter22, ok", "hunter22", "<SECRET_1>");
    }

    #[test]
    fn personal_data_is_hidden() {
        hides(
            "mail shrey@example.com today",
            "shrey@example.com",
            "<EMAIL_1>",
        );
        hides("call +1 (555) 123-4567", "123-4567", "<PHONE_1>");
        hides("call 555-123-4567", "555-123-4567", "<PHONE_1>");
        hides("card 4111 1111 1111 1111", "4111 1111", "<CARD_1>");
        hides("server at 192.168.1.20 is down", "192.168.1.20", "<IP_1>");
    }

    #[test]
    fn near_misses_are_left_alone() {
        for text in [
            "keyboard = qwerty",
            "monkey: banana",
            "the token count is 42",
            "postgres 18.6 shipped on 2026-10-27",
            "rustc 1.93.1",
            "order 4111 1111 1111 1112",
            "listening on 127.0.0.1:8080",
            "use ssh-keygen to make a key",
        ] {
            assert_eq!(redacted(text), text);
        }
    }

    #[test]
    fn redacting_twice_changes_nothing_more() {
        // A reply quoting `API_KEY=<SECRET_1>` goes out again next request.
        let mut r = Redactor::new();
        let once = r.redact("API_KEY=abc123xyz789 mail a@x.io password is hunter22");
        assert_eq!(r.redact(&once), once);
        assert_eq!(r.count(), 3);
    }

    #[test]
    fn words_containing_a_key_prefix_are_not_keys() {
        for text in [
            "task-management-module-for-the-app-2026",
            "a risk-assessment-framework-for-everyone",
        ] {
            assert_eq!(redacted(text), text);
        }
    }

    #[test]
    fn a_repeated_value_keeps_its_placeholder() {
        let mut r = Redactor::new();
        let out = r.redact("a@x.io, b@x.io, a@x.io");
        assert_eq!(out, "<EMAIL_1>, <EMAIL_2>, <EMAIL_1>");
        assert_eq!(r.redact("again a@x.io"), "again <EMAIL_1>");
        assert_eq!(r.count(), 2);
    }

    #[test]
    fn personal_data_is_restored_in_the_answer_but_secrets_are_not() {
        let mut r = Redactor::new();
        r.redact("mail a@x.io with key sk-ant-0123456789abcdefghij0123");
        assert_eq!(
            r.restore("Sent to <EMAIL_1> using <SECRET_1>."),
            "Sent to a@x.io using <SECRET_1>."
        );
    }

    #[test]
    fn a_placeholder_is_not_mistaken_for_a_longer_one() {
        let mut r = Redactor::new();
        let many: Vec<String> = (0..10).map(|i| format!("u{}@x.io", i)).collect();
        r.redact(&many.join(" "));
        assert_eq!(r.restore("<EMAIL_10> <EMAIL_1>"), "u9@x.io u0@x.io");
    }
}
