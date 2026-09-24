//! `@agent` mention detection.
//!
//! A webhook is only actionable when the comment mentions the bot. The
//! extraction is deliberately forge agnostic: it operates on a plain comment
//! body and a configurable trigger string.

use serde::{Deserialize, Serialize};

/// The result of matching the configured trigger in a comment body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mention {
    /// Optional agent selected inline, e.g. `@agent:codex`.
    pub agent: Option<String>,
    /// The instruction left after removing the trigger.
    pub message: String,
}

impl Mention {
    /// The fallback instruction used when the mention carries no message.
    pub const DEFAULT_MESSAGE: &'static str =
        "Investigate the referenced issue or pull request and take the appropriate action.";

    /// The instruction, falling back to [`Mention::DEFAULT_MESSAGE`].
    pub fn message_or_default(&self) -> &str {
        let trimmed = self.message.trim();
        if trimmed.is_empty() {
            Self::DEFAULT_MESSAGE
        } else {
            trimmed
        }
    }
}

/// Find the first mention of `trigger` in `body`.
///
/// The trigger is matched case-insensitively at a word boundary (the character
/// before it must not be alphanumeric or `_`). An optional `:<agent>` suffix
/// selects a specific adapter, e.g. `@agent:codex fix the lint errors`.
///
/// Returns `None` when the trigger is absent.
pub fn extract_mention(body: &str, trigger: &str) -> Option<Mention> {
    let trigger = trigger.trim();
    if trigger.is_empty() {
        return None;
    }

    let lower_body = body.to_lowercase();
    let lower_trigger = trigger.to_lowercase();

    let mut search_from = 0;
    while let Some(rel) = lower_body[search_from..].find(&lower_trigger) {
        let idx = search_from + rel;
        let before_ok = idx == 0
            || !body[..idx]
                .chars()
                .next_back()
                .map(|c| c.is_alphanumeric() || c == '_' || c == '-')
                .unwrap_or(false);
        if !before_ok {
            search_from = idx + lower_trigger.len();
            continue;
        }

        let after = &body[idx + trigger.len()..];

        // Optional `:agent` selector directly after the trigger.
        let (agent, rest) = match after.strip_prefix(':') {
            Some(after_colon) => {
                let end = after_colon
                    .find(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_'))
                    .unwrap_or(after_colon.len());
                let name = &after_colon[..end];
                let rest = &after_colon[end..];
                let name = name.trim();
                (
                    if name.is_empty() {
                        None
                    } else {
                        Some(name.to_owned())
                    },
                    rest,
                )
            }
            None => (None, after),
        };

        let message = rest.trim_start_matches(|c: char| c == ':' || c == ',' || c.is_whitespace());
        return Some(Mention {
            agent,
            message: message.trim().to_owned(),
        });
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_simple_mention() {
        let m = extract_mention("@agent investigate this test failure", "@agent").unwrap();
        assert_eq!(m.agent, None);
        assert_eq!(m.message, "investigate this test failure");
    }

    #[test]
    fn extracts_inline_agent() {
        let m = extract_mention("@agent:codex fix it", "@agent").unwrap();
        assert_eq!(m.agent.as_deref(), Some("codex"));
        assert_eq!(m.message, "fix it");
    }

    #[test]
    fn is_case_insensitive() {
        let m = extract_mention("Hey @Agent please review", "@agent").unwrap();
        assert_eq!(m.message, "please review");
    }

    #[test]
    fn ignores_email_and_substring() {
        assert!(extract_mention("mail me at foo@agent.com", "@agent").is_none());
        assert!(extract_mention("super@agentfoo", "@agent").is_none());
    }

    #[test]
    fn empty_message_uses_default() {
        let m = extract_mention("@agent", "@agent").unwrap();
        assert!(m.message.is_empty());
        assert_eq!(m.message_or_default(), Mention::DEFAULT_MESSAGE);
    }

    #[test]
    fn strips_leading_separators() {
        let m = extract_mention("@agent: do X", "@agent").unwrap();
        assert_eq!(m.message, "do X");
        let m = extract_mention("@agent, do Y", "@agent").unwrap();
        assert_eq!(m.message, "do Y");
    }

    #[test]
    fn matches_custom_trigger() {
        let m = extract_mention("@forge-bot build it", "@forge-bot").unwrap();
        assert_eq!(m.message, "build it");
    }
}
