use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

const SENSITIVE_KEYS: &[&str] = &[
    "authorization",
    "access_token",
    "accesstoken",
    "refresh_token",
    "refreshtoken",
    "cookie",
    "set-cookie",
    "password",
    "secret",
    "session",
];

pub fn fingerprint(value: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"arc-live-redaction-v1\0");
    digest.update(value.as_bytes());
    hex::encode(digest.finalize())[..16].to_owned()
}

pub fn sanitize_json(value: &Value) -> Value {
    sanitize_with_key(None, value)
}

pub fn json_shape(value: &Value, depth: usize) -> Value {
    if depth >= 6 {
        return Value::String("depth_limit".into());
    }
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .take(200)
                .map(|(key, value)| (key.clone(), json_shape(value, depth + 1)))
                .collect::<Map<_, _>>(),
        ),
        Value::Array(values) => serde_json::json!({
            "type":"array",
            "len": values.len(),
            "sample": values.first().map(|value| json_shape(value, depth + 1)),
        }),
        Value::String(_) => Value::String("string".into()),
        Value::Number(_) => Value::String("number".into()),
        Value::Bool(_) => Value::String("boolean".into()),
        Value::Null => Value::String("null".into()),
    }
}

fn sanitize_with_key(key: Option<&str>, value: &Value) -> Value {
    if key.is_some_and(is_sensitive_key) {
        return Value::String("[REDACTED]".to_owned());
    }
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| (key.clone(), sanitize_with_key(Some(key), value)))
                .collect::<Map<_, _>>(),
        ),
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| sanitize_with_key(None, value))
                .collect(),
        ),
        Value::String(value) if looks_like_jwt(value) => {
            Value::String(format!("[JWT:{}]", fingerprint(value)))
        }
        Value::String(value) if value.len() > 4096 => {
            Value::String(format!("[LONG_STRING:{} bytes]", value.len()))
        }
        other => other.clone(),
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase().replace(['_', '-'], "");
    SENSITIVE_KEYS
        .iter()
        .map(|candidate| candidate.replace(['_', '-'], ""))
        .any(|candidate| normalized.contains(&candidate))
}

fn looks_like_jwt(value: &str) -> bool {
    let mut segments = value.split('.');
    matches!(
        (segments.next(), segments.next(), segments.next(), segments.next()),
        (Some(a), Some(b), Some(c), None) if a.len() >= 8 && b.len() >= 8 && c.len() >= 8
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sensitive_fields_and_jwts_are_removed() {
        let input = json!({
            "Authorization": "Bearer abc",
            "nested": {"accessToken": "secret"},
            "safe": "one.two.three",
            "jwt": "abcdefgh.ijklmnop.qrstuvwx"
        });
        let output = sanitize_json(&input);
        assert_eq!(output["Authorization"], "[REDACTED]");
        assert_eq!(output["nested"]["accessToken"], "[REDACTED]");
        assert_eq!(output["safe"], "one.two.three");
        assert!(output["jwt"].as_str().unwrap().starts_with("[JWT:"));
    }

    #[test]
    fn fingerprints_are_stable_and_short() {
        assert_eq!(fingerprint("same"), fingerprint("same"));
        assert_ne!(fingerprint("same"), fingerprint("other"));
        assert_eq!(fingerprint("same").len(), 16);
    }

    #[test]
    fn shapes_drop_json_values() {
        let shape = json_shape(&json!({"round_id":"secret", "kills":4}), 0);
        assert_eq!(shape["round_id"], "string");
        assert_eq!(shape["kills"], "number");
    }
}
