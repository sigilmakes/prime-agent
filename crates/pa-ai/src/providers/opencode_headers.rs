//! `OpenCode` provider identity headers, ported from `opencode-headers.ts`.

/// Add the `OpenCode` routing identity to a request's merged headers.
///
/// TS contract: start with `User-Agent: prime-agent`, add
/// `x-opencode-session` only when a session id is present, then let model and
/// caller headers override either value case-insensitively. This helper runs
/// after those merges, so it only fills values the caller did not supply.
pub(crate) fn with_opencode_headers(
    provider: &str,
    session_id: Option<&str>,
    headers: &mut Vec<(String, String)>,
) {
    if provider != "opencode" && provider != "opencode-go" {
        return;
    }
    if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("User-Agent"))
    {
        headers.push(("User-Agent".to_string(), "prime-agent".to_string()));
    }
    if let Some(session_id) = session_id.filter(|session_id| !session_id.is_empty()) {
        if !headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("x-opencode-session"))
        {
            headers.push(("x-opencode-session".to_string(), session_id.to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::with_opencode_headers;

    fn pairs(values: &[(&str, &str)]) -> Vec<(String, String)> {
        values
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect()
    }

    fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn non_opencode_providers_are_untouched() {
        let mut headers = pairs(&[("authorization", "Bearer k")]);
        with_opencode_headers("openai", Some("session-1"), &mut headers);
        assert_eq!(headers, pairs(&[("authorization", "Bearer k")]));
    }

    #[test]
    fn adds_both_identity_headers_for_opencode() {
        let mut headers = Vec::new();
        with_opencode_headers("opencode", Some("session-1"), &mut headers);
        assert_eq!(header(&headers, "User-Agent"), Some("prime-agent"));
        assert_eq!(header(&headers, "x-opencode-session"), Some("session-1"));
    }

    #[test]
    fn adds_user_agent_without_a_session_id() {
        let mut headers = Vec::new();
        with_opencode_headers("opencode-go", None, &mut headers);
        assert_eq!(header(&headers, "User-Agent"), Some("prime-agent"));
        assert_eq!(header(&headers, "x-opencode-session"), None);
    }

    #[test]
    fn caller_headers_win_case_insensitively() {
        let mut headers = pairs(&[
            ("user-agent", "custom-agent"),
            ("X-OpenCode-Session", "caller-session"),
        ]);
        with_opencode_headers("opencode", Some("session-1"), &mut headers);
        assert_eq!(header(&headers, "user-agent"), Some("custom-agent"));
        assert_eq!(
            header(&headers, "x-opencode-session"),
            Some("caller-session")
        );
    }

    #[test]
    fn empty_session_id_is_not_an_identity() {
        let mut headers = Vec::new();
        with_opencode_headers("opencode", Some(""), &mut headers);
        assert_eq!(header(&headers, "User-Agent"), Some("prime-agent"));
        assert_eq!(header(&headers, "x-opencode-session"), None);
    }
}
