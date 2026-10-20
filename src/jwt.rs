use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Value;

/// @cc [owner:ghuntley,label:auth] jwt-malformed-returns-none
/// `parse_jwt_claims` MUST return `None` — never panic — for tokens that are not three
/// dot-separated segments, have a non-base64url payload, or decode to invalid JSON.
pub fn parse_jwt_claims(token: &str) -> Option<Value> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(parts[1]).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn extract_account_id(claims: &Value) -> Option<String> {
    claims
        .get("chatgpt_account_id")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| {
            claims
                .get("https://api.openai.com/auth")
                .and_then(|auth| auth.get("chatgpt_account_id"))
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .or_else(|| {
            claims
                .get("organizations")
                .and_then(|o| o.get(0))
                .and_then(|o| o.get("id"))
                .and_then(|v| v.as_str())
                .map(String::from)
        })
}

pub fn extract_residency(claims: &Value) -> Option<String> {
    let residency = claims
        .get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("chatgpt_compute_residency"))
        .and_then(|v| v.as_str())
        .or_else(|| {
            claims
                .get("chatgpt_compute_residency")
                .and_then(|v| v.as_str())
        });
    match residency {
        Some(r) if r != "no_constraint" && !r.is_empty() => Some(r.to_string()),
        _ => None,
    }
}

pub fn extract_email(claims: &Value) -> Option<String> {
    claims
        .get("email")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| {
            claims
                .get("https://api.openai.com/profile")
                .and_then(|p| p.get("email"))
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .filter(|s| !s.is_empty())
}

pub fn extract_display_name(claims: &Value) -> Option<String> {
    claims
        .get("https://api.openai.com/profile")
        .and_then(|p| p.get("name"))
        .and_then(|v| v.as_str())
        .map(String::from)
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn encode_token(claims: &Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).unwrap());
        format!("{header}.{payload}.sig")
    }

    #[test]
    fn account_id_direct_claim() {
        let t = encode_token(&json!({"chatgpt_account_id": "acc-1"}));
        let claims = parse_jwt_claims(&t).unwrap();
        assert_eq!(extract_account_id(&claims).as_deref(), Some("acc-1"));
    }

    #[test]
    fn account_id_namespaced_claim() {
        let t =
            encode_token(&json!({"https://api.openai.com/auth": {"chatgpt_account_id": "acc-2"}}));
        let claims = parse_jwt_claims(&t).unwrap();
        assert_eq!(extract_account_id(&claims).as_deref(), Some("acc-2"));
    }

    #[test]
    fn account_id_falls_back_to_organization() {
        let t = encode_token(&json!({"organizations": [{"id": "org-9"}]}));
        let claims = parse_jwt_claims(&t).unwrap();
        assert_eq!(extract_account_id(&claims).as_deref(), Some("org-9"));
    }

    #[test]
    fn residency_skips_no_constraint() {
        let t = encode_token(&json!({"chatgpt_compute_residency": "no_constraint"}));
        let claims = parse_jwt_claims(&t).unwrap();
        assert_eq!(extract_residency(&claims), None);
        let t = encode_token(
            &json!({"https://api.openai.com/auth": {"chatgpt_compute_residency": "eu"}}),
        );
        let claims = parse_jwt_claims(&t).unwrap();
        assert_eq!(extract_residency(&claims).as_deref(), Some("eu"));
    }

    #[test]
    fn email_from_top_level_or_namespaced_profile() {
        let t = encode_token(&json!({"email": "a@b.c"}));
        let claims = parse_jwt_claims(&t).unwrap();
        assert_eq!(extract_email(&claims).as_deref(), Some("a@b.c"));

        let t = encode_token(
            &json!({"https://api.openai.com/profile": {"email": "x@y.z", "name": "Geo"}}),
        );
        let claims = parse_jwt_claims(&t).unwrap();
        assert_eq!(extract_email(&claims).as_deref(), Some("x@y.z"));
        assert_eq!(extract_display_name(&claims).as_deref(), Some("Geo"));
    }

    #[test]
    fn email_missing_or_empty_returns_none() {
        let t = encode_token(&json!({"https://api.openai.com/profile": {"email": ""}}));
        let claims = parse_jwt_claims(&t).unwrap();
        assert_eq!(extract_email(&claims), None);
        let t = encode_token(&json!({"aud": "x"}));
        let claims = parse_jwt_claims(&t).unwrap();
        assert_eq!(extract_email(&claims), None);
    }

    #[test]
    fn malformed_tokens_return_none() {
        assert!(parse_jwt_claims("not-a-jwt").is_none());
        assert!(parse_jwt_claims("a.b").is_none());
        assert!(parse_jwt_claims("a.!!!.c").is_none());
    }
}
