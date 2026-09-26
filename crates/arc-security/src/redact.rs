//! Secret redaction for logs, the audit trail and diagnostics.
//!
//! Three layers:
//! 1. **Keys**: JSON object members whose *name* looks secret
//!    (`api_key`, `password`, `authorization`, `token`…) are replaced.
//! 2. **Formats**: well-known credential shapes inside any string
//!    (`sk-…`, `sk-ant-…`, `ghp_…`, `github_pat_…`, `xox?-…`, AWS access keys,
//!    `Bearer …`, JWTs, PEM private keys, `password=…` pairs).
//! 3. **Live values**: the actual values of secret-looking environment
//!    variables in this process (e.g. `$OPENAI_API_KEY`) are replaced wherever
//!    they appear, whatever their format.
//!
//! Redaction always returns a copy; the input is never modified.

use regex::Regex;
use serde_json::Value;
use std::borrow::Cow;
use std::sync::OnceLock;

pub const REDACTED: &str = "[redacted]";

fn secret_key_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(^|[_\-.])(api[_\-]?key|apikey|secret|token|password|passwd|passphrase|authorization|auth[_\-]?header|cookie|credentials?|private[_\-]?key|access[_\-]?key|session[_\-]?id)($|[_\-.])")
            .unwrap()
    })
}

fn format_res() -> &'static [Regex] {
    static RES: OnceLock<Vec<Regex>> = OnceLock::new();
    RES.get_or_init(|| {
        [
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
            r"\bsk-(ant-|proj-|or-)?[A-Za-z0-9_\-]{16,}",
            r"\bgh[pousr]_[A-Za-z0-9]{30,}",
            r"\bgithub_pat_[A-Za-z0-9_]{30,}",
            r"\bxox[abprs]-[A-Za-z0-9\-]{10,}",
            r"\bAKIA[0-9A-Z]{16}\b",
            r"\bAIza[0-9A-Za-z_\-]{30,}",
            r"\beyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}",
            r"(?i)\b(bearer|basic)\s+[A-Za-z0-9._~+/=\-]{12,}",
        ]
        .iter()
        .map(|r| Regex::new(r).unwrap())
        .collect()
    })
}

fn kv_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?i)\b([A-Z0-9_]*(?:api[_-]?key|secret|token|password|passwd)[A-Z0-9_]*)\s*([=:])\s*("[^"]*"|'[^']*'|[^\s"',;]+)"#)
            .unwrap()
    })
}

/// Values of secret-looking environment variables present at first use.
fn env_secrets() -> &'static [String] {
    static V: OnceLock<Vec<String>> = OnceLock::new();
    V.get_or_init(|| {
        let mut v: Vec<String> = std::env::vars()
            .filter(|(k, val)| {
                val.len() >= 12 && secret_key_re().is_match(&k.to_ascii_lowercase())
                    || k.ends_with("_KEY") && val.len() >= 12
            })
            .map(|(_, val)| val)
            .collect();
        // Longest first so overlapping values redact fully.
        v.sort_by_key(|s| std::cmp::Reverse(s.len()));
        v.dedup();
        v
    })
}

/// True when an object key name indicates a secret value.
pub fn is_secret_key(key: &str) -> bool {
    secret_key_re().is_match(&key.to_ascii_lowercase())
}

/// Redact credential-shaped substrings in free text.
pub fn redact_str(s: &str) -> Cow<'_, str> {
    let mut out: Cow<'_, str> = Cow::Borrowed(s);
    for secret in env_secrets() {
        if out.contains(secret.as_str()) {
            out = Cow::Owned(out.replace(secret.as_str(), REDACTED));
        }
    }
    for re in format_res() {
        if re.is_match(&out) {
            out = Cow::Owned(re.replace_all(&out, REDACTED).into_owned());
        }
    }
    if kv_re().is_match(&out) {
        let replaced = kv_re().replace_all(&out, |c: &regex::Captures| {
            let val = c[3].trim_matches(|q| q == '"' || q == '\'');
            // Numbers are counts ("tokens: 512"), not secrets.
            if val.is_empty() || val.chars().all(|ch| ch.is_ascii_digit() || ch == '.') {
                c[0].to_string()
            } else {
                format!("{}{}{REDACTED}", &c[1], &c[2])
            }
        });
        if replaced != out.as_ref() {
            out = Cow::Owned(replaced.into_owned());
        }
    }
    out
}

/// Deep-copy `v` with secrets redacted.
pub fn redact_value(v: &Value) -> Value {
    match v {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, val)| {
                    let red = if is_secret_key(k) && !matches!(val, Value::Null | Value::Bool(_)) {
                        Value::String(REDACTED.into())
                    } else {
                        redact_value(val)
                    };
                    (k.clone(), red)
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(redact_value).collect()),
        Value::String(s) => Value::String(redact_str(s).into_owned()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keys_are_redacted_but_ordinary_fields_kept() {
        let v = json!({
            "api_key": "abc", "OPENAI_API_KEY": "x", "Authorization": "Bearer abc",
            "password": "hunter2", "nested": {"refresh_token": "t", "keyboard": "us", "monkey": "banana"},
            "tokens_used": 42, "has_secret": true, "workspace": 3, "keys": ["a"]
        });
        let r = redact_value(&v);
        assert_eq!(r["api_key"], REDACTED);
        assert_eq!(r["OPENAI_API_KEY"], REDACTED);
        assert_eq!(r["Authorization"], REDACTED);
        assert_eq!(r["password"], REDACTED);
        assert_eq!(r["nested"]["refresh_token"], REDACTED);
        // Not secrets: substring "key"/"token" in unrelated names or non-string values.
        assert_eq!(r["nested"]["keyboard"], "us");
        assert_eq!(r["nested"]["monkey"], "banana");
        assert_eq!(r["tokens_used"], 42);
        assert_eq!(r["has_secret"], true);
        assert_eq!(r["workspace"], 3);
        assert_eq!(r["keys"], json!(["a"]));
    }

    #[test]
    fn credential_formats_in_text() {
        let cases = [
            "key sk-ant-api03-AbCdEfGhIjKlMnOpQrStUv here",
            "sk-proj-abcdefghijklmnopqrstuvwxyz0123",
            "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            "curl -H 'Authorization: Bearer abcdef0123456789xyz'",
            "AKIAIOSFODNN7EXAMPLE",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
            "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----",
        ];
        for c in cases {
            let r = redact_str(c);
            assert!(r.contains(REDACTED), "{c} -> {r}");
        }
        let r = redact_str("export API_KEY=supersecretvalue; password: 'x y'");
        assert_eq!(r, format!("export API_KEY={REDACTED}; password:{REDACTED}"));
    }

    #[test]
    fn plain_text_untouched() {
        for s in [
            "Opening Firefox.",
            "switch to workspace 3",
            "the keyboard layout is us",
            "tokens: 512",
            "skip to the next track",
            "task-manager",
        ] {
            assert!(matches!(redact_str(s), Cow::Borrowed(_)), "{s}");
        }
    }

    #[test]
    fn original_not_mutated() {
        let v = json!({"password": "p"});
        let _ = redact_value(&v);
        assert_eq!(v["password"], "p");
    }
}
