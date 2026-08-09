use serde_json::Value;
use std::collections::HashSet;

const SCAN_CHUNK_BYTES: usize = 1024 * 1024;
const SCAN_OVERLAP_BYTES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SensitiveFinding {
    pub code: &'static str,
    pub label: &'static str,
}

pub fn scan_json(value: &Value) -> Vec<SensitiveFinding> {
    match serde_json::to_vec(value) {
        Ok(bytes) => scan_bytes(&bytes),
        Err(_) => Vec::new(),
    }
}

pub fn scan_bytes(bytes: &[u8]) -> Vec<SensitiveFinding> {
    let mut found = HashSet::new();
    if bytes.is_empty() {
        return Vec::new();
    }

    let mut start = 0;
    while start < bytes.len() {
        let end = (start + SCAN_CHUNK_BYTES).min(bytes.len());
        let text = String::from_utf8_lossy(&bytes[start..end]);
        scan_text_into(&text, &mut found);
        if found.len() == FINDING_TYPES.len() || end == bytes.len() {
            break;
        }
        start = end.saturating_sub(SCAN_OVERLAP_BYTES);
    }

    FINDING_TYPES
        .iter()
        .filter(|finding| found.contains(finding.code))
        .cloned()
        .collect()
}

const FINDING_TYPES: &[SensitiveFinding] = &[
    SensitiveFinding {
        code: "PRIVATE_KEY_MATERIAL",
        label: "可能包含私钥正文",
    },
    SensitiveFinding {
        code: "AUTHORIZATION_CREDENTIAL",
        label: "可能包含 Authorization 凭据",
    },
    SensitiveFinding {
        code: "KNOWN_TOKEN_FORMAT",
        label: "可能包含常见服务令牌",
    },
    SensitiveFinding {
        code: "DATABASE_CONNECTION_STRING",
        label: "可能包含数据库或消息服务连接串",
    },
    SensitiveFinding {
        code: "SECRET_ASSIGNMENT",
        label: "可能包含密码、密钥或令牌赋值",
    },
];

fn scan_text_into(text: &str, found: &mut HashSet<&'static str>) {
    let upper = text.to_ascii_uppercase();
    if upper.contains("-----BEGIN PRIVATE KEY-----")
        || upper.contains("-----BEGIN RSA PRIVATE KEY-----")
        || upper.contains("-----BEGIN EC PRIVATE KEY-----")
        || upper.contains("-----BEGIN OPENSSH PRIVATE KEY-----")
        || upper.contains("-----BEGIN PGP PRIVATE KEY BLOCK-----")
    {
        found.insert("PRIVATE_KEY_MATERIAL");
    }

    if upper.contains("AUTHORIZATION: BEARER ")
        || upper.contains("AUTHORIZATION\":\"BEARER ")
        || upper.contains("AUTHORIZATION': 'BEARER ")
    {
        found.insert("AUTHORIZATION_CREDENTIAL");
    }

    if has_prefixed_token(text, "AKIA", 16, token_alphanumeric_upper)
        || has_prefixed_token(text, "ASIA", 16, token_alphanumeric_upper)
        || has_prefixed_token(text, "ghp_", 24, token_base64ish)
        || has_prefixed_token(text, "github_pat_", 24, token_base64ish)
        || has_prefixed_token(text, "sk-", 20, token_base64ish)
        || has_prefixed_token(text, "xoxb-", 16, token_base64ish)
        || has_prefixed_token(text, "xoxp-", 16, token_base64ish)
        || has_jwt_shape(text)
    {
        found.insert("KNOWN_TOKEN_FORMAT");
    }

    let lower = text.to_ascii_lowercase();
    if [
        "postgres://",
        "postgresql://",
        "mysql://",
        "mongodb://",
        "mongodb+srv://",
        "redis://",
        "rediss://",
        "amqp://",
        "amqps://",
    ]
    .iter()
    .any(|scheme| lower.contains(scheme))
    {
        found.insert("DATABASE_CONNECTION_STRING");
    }

    if text.lines().any(looks_like_secret_assignment) {
        found.insert("SECRET_ASSIGNMENT");
    }
}

fn has_prefixed_token(
    text: &str,
    prefix: &str,
    minimum_tail: usize,
    allowed: fn(u8) -> bool,
) -> bool {
    let bytes = text.as_bytes();
    let prefix_bytes = prefix.as_bytes();
    let mut offset = 0;
    while let Some(relative) = text[offset..].find(prefix) {
        let index = offset + relative;
        let boundary_ok = index == 0
            || !bytes[index - 1].is_ascii_alphanumeric()
                && !matches!(bytes[index - 1], b'_' | b'-');
        if boundary_ok {
            let mut length = 0;
            for byte in &bytes[index + prefix_bytes.len()..] {
                if !allowed(*byte) {
                    break;
                }
                length += 1;
            }
            if length >= minimum_tail {
                return true;
            }
        }
        offset = index + prefix_bytes.len();
        if offset >= text.len() {
            break;
        }
    }
    false
}

fn token_alphanumeric_upper(byte: u8) -> bool {
    byte.is_ascii_uppercase() || byte.is_ascii_digit()
}

fn token_base64ish(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
}

fn has_jwt_shape(text: &str) -> bool {
    text.split(|character: char| character.is_whitespace() || matches!(character, '"' | '\''))
        .any(|candidate| {
            candidate.starts_with("eyJ")
                && candidate.len() >= 48
                && candidate.split('.').count() == 3
                && candidate
                    .bytes()
                    .all(|byte| token_base64ish(byte) || byte == b'.')
        })
}

fn looks_like_secret_assignment(line: &str) -> bool {
    let trimmed = line.trim();
    let Some((key, value)) = trimmed.split_once('=') else {
        return false;
    };
    let key = key
        .trim()
        .trim_start_matches("export ")
        .trim_matches(|character: char| character == '"' || character == '\'')
        .to_ascii_uppercase();
    if ![
        "PASSWORD",
        "PASSWD",
        "SECRET",
        "TOKEN",
        "API_KEY",
        "ACCESS_KEY",
        "ACCESS_KEY_ID",
        "PRIVATE_KEY",
        "CLIENT_SECRET",
    ]
    .iter()
    .any(|marker| key == *marker || key.ends_with(&format!("_{marker}")))
    {
        return false;
    }

    let value = value
        .trim()
        .trim_matches(|character: char| matches!(character, '"' | '\'' | ',' | ';'));
    if value.len() < 8 {
        return false;
    }
    let normalized = value.to_ascii_lowercase();
    ![
        "example",
        "sample",
        "placeholder",
        "changeme",
        "your_",
        "your-",
        "<secret>",
        "${",
    ]
    .iter()
    .any(|placeholder| normalized.contains(placeholder))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes(value: &[SensitiveFinding]) -> Vec<&'static str> {
        value.iter().map(|finding| finding.code).collect()
    }

    #[test]
    fn detects_known_secret_shapes_without_returning_secret_values() {
        let input = r#"
            -----BEGIN OPENSSH PRIVATE KEY-----
            Authorization: Bearer this-is-a-real-looking-token-value
            AWS_ACCESS_KEY_ID=AKIAABCDEFGHIJKLMNOP
            DATABASE_URL=postgres://relay:password@example.test/relay
        "#;
        let findings = scan_bytes(input.as_bytes());
        assert_eq!(
            codes(&findings),
            vec![
                "PRIVATE_KEY_MATERIAL",
                "AUTHORIZATION_CREDENTIAL",
                "KNOWN_TOKEN_FORMAT",
                "DATABASE_CONNECTION_STRING",
                "SECRET_ASSIGNMENT",
            ]
        );
        let serialized = format!("{findings:?}");
        assert!(!serialized.contains("password"));
        assert!(!serialized.contains("AKIAABCDEFGHIJKLMNOP"));
    }

    #[test]
    fn ignores_documentation_placeholders_and_short_values() {
        let input = r#"
            API_KEY=your_api_key_here
            PASSWORD=changeme
            TOKEN=short
            DATABASE_URL=example
        "#;
        assert!(scan_bytes(input.as_bytes()).is_empty());
    }

    #[test]
    fn detects_access_key_id_assignments() {
        let input = "AWS_ACCESS_KEY_ID=AKIAABCDEFGHIJKLMNOP";
        assert!(codes(&scan_bytes(input.as_bytes())).contains(&"SECRET_ASSIGNMENT"));
    }

    #[test]
    fn detects_tokens_split_near_chunk_boundaries() {
        let mut input = vec![b'a'; SCAN_CHUNK_BYTES - 8];
        input.extend_from_slice(b" value=ghp_abcdefghijklmnopqrstuvwxyz0123456789 ");
        assert!(codes(&scan_bytes(&input)).contains(&"KNOWN_TOKEN_FORMAT"));
    }

    #[test]
    fn scans_json_values() {
        let value = serde_json::json!({"tool_output": "CLIENT_SECRET=abcdefghijklmnopqrstuvwxyz"});
        assert_eq!(codes(&scan_json(&value)), vec!["SECRET_ASSIGNMENT"]);
    }
}
