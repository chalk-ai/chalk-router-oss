//! Validation for immutable API-key labels.

use std::collections::HashMap;

pub const MAX_LABELS: usize = 8;
pub const MAX_LABEL_KEY_BYTES: usize = 64;
pub const MAX_LABEL_VALUE_BYTES: usize = 256;
pub const MAX_LABEL_BYTES: usize = 2 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelError(String);

impl LabelError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for LabelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LabelError {}

/// Validate the complete label map before creating a key. These fixed bounds
/// keep control-plane storage and later label-based usage grouping bounded.
pub fn validate(labels: &HashMap<String, String>) -> Result<(), LabelError> {
    if labels.len() > MAX_LABELS {
        return Err(LabelError::new(format!(
            "labels may contain at most {MAX_LABELS} entries"
        )));
    }

    let mut total_bytes = 0usize;
    for (key, value) in labels {
        let key_bytes = key.as_bytes();
        if key_bytes.is_empty() || key_bytes.len() > MAX_LABEL_KEY_BYTES {
            return Err(LabelError::new(format!(
                "label key length must be between 1 and {MAX_LABEL_KEY_BYTES} bytes"
            )));
        }
        if key.starts_with("chalk.") {
            return Err(LabelError::new(format!(
                "label key '{key}' uses the reserved chalk. prefix"
            )));
        }
        let valid_key = key_bytes.first().is_some_and(u8::is_ascii_lowercase)
            && key_bytes.iter().skip(1).all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'_' | b'.' | b'-')
            });
        if !valid_key {
            return Err(LabelError::new(format!(
                "label key '{key}' must match ^[a-z][a-z0-9_.-]{{0,63}}$"
            )));
        }
        if value.is_empty() || value.len() > MAX_LABEL_VALUE_BYTES {
            return Err(LabelError::new(format!(
                "label value for '{key}' must be between 1 and {MAX_LABEL_VALUE_BYTES} UTF-8 bytes"
            )));
        }
        total_bytes = total_bytes
            .checked_add(key.len())
            .and_then(|sum| sum.checked_add(value.len()))
            .ok_or_else(|| LabelError::new("label byte count overflow"))?;
    }
    if total_bytes > MAX_LABEL_BYTES {
        return Err(LabelError::new(format!(
            "labels may contain at most {MAX_LABEL_BYTES} encoded key/value bytes"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn validation_bounds_keys_values_and_cardinality() {
        assert!(validate(&labels(&[("team.name-1", "routing")])).is_ok());
        assert!(validate(&labels(&[("Team", "routing")])).is_err());
        assert!(validate(&labels(&[("chalk.internal", "routing")])).is_err());
        assert!(validate(&labels(&[("team", "")])).is_err());
        let too_many = (0..=MAX_LABELS)
            .map(|i| (format!("label{i}"), "value".to_string()))
            .collect();
        assert!(validate(&too_many).is_err());
    }
}
