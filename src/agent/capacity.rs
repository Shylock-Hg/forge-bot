//! Detection of agent capacity / quota exhaustion.
//!
//! Coding-agent CLIs fail in very different ways when they cannot take more
//! work. Sometimes the account is out of quota (Codex prints a human-readable
//! "usage limit" message), sometimes the provider is overloaded or out of
//! capacity (Anthropic's `overloaded_error`, a `503 Service Unavailable`, a
//! "server is busy" note), and sometimes the CLI has hit a concurrency limit.
//! There is no shared machine-readable signal, so the dispatcher classifies a
//! failed run as *capacity-limited* by matching the output against a
//! conservative list of phrases.
//!
//! Matching is deliberately restricted to *failed* runs (see
//! [`super::command::CommandAgent`]): an agent that merely mentions "rate
//! limit" or "capacity" while editing code and still succeeds must not be
//! treated as exhausted.

/// Built-in phrases that indicate an agent has exhausted its quota, hit a
/// rate/concurrency limit, or that its provider is temporarily overloaded.
/// Compared case-insensitively as substrings.
pub const DEFAULT_CAPACITY_MARKERS: &[&str] = &[
    // Quota / usage limit.
    "usage limit",
    "hit your limit",
    "hit your usage limit",
    "reached your limit",
    "reached your quota",
    "rate limit",
    "rate-limit",
    "rate_limit",
    "rate_limit_error",
    "ratelimit",
    "rate limited",
    "too many requests",
    "quota exceeded",
    "exceeded your quota",
    "exceeded the quota",
    "exceeded your current quota",
    "quota has been reached",
    "out of quota",
    "insufficient quota",
    "insufficient_quota",
    "weekly limit",
    "daily limit",
    "monthly limit",
    "usage cap",
    "limit will reset",
    "credit balance",
    "out of credits",
    "purchase more credits",
    "billing hard limit",
    // Provider capacity / overload.
    "capacity limit",
    "at capacity",
    "reached capacity",
    "capacity exceeded",
    "no capacity",
    "out of capacity",
    "insufficient capacity",
    "overloaded",
    "overloaded_error",
    "server is busy",
    "server busy",
    "service unavailable",
    "temporarily unavailable",
    "high demand",
    "resource exhausted",
    "resource_exhausted",
    "too many concurrent",
    "concurrency limit",
    "concurrent request",
    "concurrent session",
    "maximum concurrent",
    "session limit",
    "max sessions",
    // Generic transient "try later" hint; harmless because it is only checked
    // on a failed run.
    "try again later",
];

/// Whether `text` looks like a capacity or quota message.
///
/// `extra` lets operators add phrases for a CLI whose wording is not covered
/// by [`DEFAULT_CAPACITY_MARKERS`]; it is matched case-insensitively too.
pub fn is_capacity_limited(text: &str, extra: &[String]) -> bool {
    let lower = text.to_ascii_lowercase();
    DEFAULT_CAPACITY_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
        || extra.iter().any(|marker| {
            let marker = marker.trim().to_ascii_lowercase();
            !marker.is_empty() && lower.contains(&marker)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_quota_and_rate_limit_messages() {
        assert!(is_capacity_limited(
            "You've hit your usage limit. Upgrade to Pro or try again later.",
            &[]
        ));
        assert!(is_capacity_limited("429 Too Many Requests", &[]));
        assert!(is_capacity_limited(
            "stream error: rate_limit_error: quota exceeded",
            &[]
        ));
        assert!(is_capacity_limited(
            "Claude AI usage limit reached. Your limit will reset at 3pm.",
            &[]
        ));
        assert!(is_capacity_limited("You exceeded your current quota", &[]));
    }

    #[test]
    fn detects_capacity_and_overload_messages() {
        assert!(is_capacity_limited(
            "The model is currently overloaded. Please try again later.",
            &[]
        ));
        assert!(is_capacity_limited("overloaded_error: Overloaded", &[]));
        assert!(is_capacity_limited("503 Service Unavailable", &[]));
        assert!(is_capacity_limited(
            "reached capacity limit for concurrent sessions",
            &[]
        ));
        assert!(is_capacity_limited(
            "you have exceeded the maximum number of concurrent requests",
            &[]
        ));
    }

    #[test]
    fn does_not_treat_pool_exhaustion_as_provider_capacity() {
        // The pooled adapter waiting for one of its own agents is a busy
        // gateway, not the provider refusing work, so it must not mark the
        // agent unavailable.
        assert!(!is_capacity_limited(
            "timed out waiting for an idle pi agent",
            &[]
        ));
        assert!(!is_capacity_limited("waiting for an idle agent", &[]));
    }

    #[test]
    fn ignores_unrelated_failures() {
        assert!(!is_capacity_limited(
            "compile error: cannot find crate",
            &[]
        ));
        assert!(!is_capacity_limited("test failed: expected 1, got 2", &[]));
        assert!(!is_capacity_limited("", &[]));
    }

    #[test]
    fn extra_markers_are_matched_case_insensitively() {
        let extra = vec!["No remaining tokens".to_owned()];
        assert!(is_capacity_limited(
            "error: no remaining tokens today",
            &extra
        ));
        assert!(!is_capacity_limited("error: something else", &extra));
        // Blank extra markers never match everything.
        assert!(!is_capacity_limited(
            "error: something else",
            &["".to_owned()]
        ));
    }
}
