//! The outbound `User-Agent` for the fork's own backends.
//!
//! Cline and OpenCode Go are fork-local backends, so the identity their upstream sees is
//! underclass's to choose: the operator's configured override wins, then the client's own
//! `User-Agent` when the client sent one, then [`DEFAULT`]. The upstream backends (Codex,
//! Copilot, Claude) do not use this module - their user-agent behaviour is upstream's, and
//! Anthropic in particular validates the `claude-cli/` value the client sends.

use http::{HeaderMap, HeaderValue};

/// What the fork's backends present when the operator configured no override and the client sent
/// no user agent of its own.
pub const DEFAULT: &str = concat!("underclass/", env!("CARGO_PKG_VERSION"));

/// The client's own user agent, if it sent one.
pub fn client_value(headers: &HeaderMap) -> Option<&HeaderValue> {
    headers.get(http::header::USER_AGENT)
}

/// @cc [owner:ghuntley,label:proxy] user-agent-precedence
/// `resolve` MUST return the configured override when one is set and is a valid header value,
/// otherwise the client's own `User-Agent` when the client sent a non-empty one, otherwise
/// `DEFAULT`. An override that is blank or not a valid header value MUST be ignored rather than
/// panic, and MUST NOT cause an empty user agent to be sent.
pub fn resolve(client: Option<&HeaderValue>, override_ua: Option<&str>) -> HeaderValue {
    if let Some(value) = override_ua
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|value| HeaderValue::from_str(value).ok())
    {
        return value;
    }
    if let Some(client) = client.filter(|value| !value.is_empty()) {
        return client.clone();
    }
    HeaderValue::from_static(DEFAULT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(value: &str) -> HeaderValue {
        HeaderValue::from_str(value).expect("test value is a valid header")
    }

    #[test]
    fn the_override_wins_over_the_client_and_the_default() {
        let client = header("opencode/1.0");
        assert_eq!(
            resolve(Some(&client), Some("operator-agent/9")).as_bytes(),
            b"operator-agent/9"
        );
        assert_eq!(
            resolve(None, Some("operator-agent/9")).as_bytes(),
            b"operator-agent/9"
        );
    }

    #[test]
    fn the_client_wins_over_the_default() {
        let client = header("opencode/1.0");
        assert_eq!(resolve(Some(&client), None).as_bytes(), b"opencode/1.0");
    }

    #[test]
    fn the_default_is_used_when_nothing_else_is_available() {
        assert_eq!(resolve(None, None).as_bytes(), DEFAULT.as_bytes());
    }

    #[test]
    fn an_unusable_override_falls_through_instead_of_panicking() {
        for override_ua in ["", "   ", "bad\nvalue", "\u{7f}"] {
            let client = header("opencode/1.0");
            assert_eq!(
                resolve(Some(&client), Some(override_ua)).as_bytes(),
                b"opencode/1.0",
                "override {override_ua:?} must not be used"
            );
            assert_eq!(
                resolve(None, Some(override_ua)).as_bytes(),
                DEFAULT.as_bytes(),
                "override {override_ua:?} must not blank the user agent"
            );
        }
    }

    #[test]
    fn a_blank_client_value_is_ignored() {
        let blank = header("");
        assert_eq!(resolve(Some(&blank), None).as_bytes(), DEFAULT.as_bytes());
    }
}
