//! D9: nothing carrying a raw credential enters regency. Inventories, host
//! definitions and tasks with `RawUser` (inline login/password) are rejected
//! at ingestion and again by the scheduler, so only secret references transit
//! RabbitMQ.

use serde::Serialize;

/// Serde names of SDK variants that embed a secret value.
const INLINE_SECRET_KEYS: [&str; 1] = ["RawUser"];

/// `Err(path)` if `value` contains an inline secret anywhere.
pub fn reject_inline_secrets<T: Serialize>(value: &T) -> Result<(), String> {
    let json = serde_json::to_value(value).map_err(|e| format!("cannot inspect value: {e}"))?;
    find(&json, "$")
}

fn find(value: &serde_json::Value, path: &str) -> Result<(), String> {
    match value {
        serde_json::Value::Object(map) => {
            for (key, inner) in map {
                let inner_path = format!("{path}.{key}");
                if INLINE_SECRET_KEYS.contains(&key.as_str()) {
                    return Err(format!(
                        "inline credentials at {inner_path} are refused: use a secret reference (D9)"
                    ));
                }
                find(inner, &inner_path)?;
            }
            Ok(())
        }
        serde_json::Value::Array(items) => items
            .iter()
            .enumerate()
            .try_for_each(|(i, inner)| find(inner, &format!("{path}[{i}]"))),
        serde_json::Value::String(s) if INLINE_SECRET_KEYS.contains(&s.as_str()) => Err(format!(
            "inline credentials at {path} are refused: use a secret reference (D9)"
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regent_sdk::hosts::handlers::{ConnectionMethod, TargetUser};
    use regent_sdk::hosts::managed_host::ManagedHostBuilder;

    #[test]
    fn raw_user_is_refused_references_pass() {
        let raw = ManagedHostBuilder::new(
            "h",
            "localhost",
            Some(ConnectionMethod::Localhost(TargetUser::user_raw("u", "p"))),
        );
        assert!(reject_inline_secrets(&raw).unwrap_err().contains("RawUser"));
        let reference = ManagedHostBuilder::new(
            "h",
            "localhost",
            Some(ConnectionMethod::Localhost(TargetUser::user("creds", None))),
        );
        reject_inline_secrets(&reference).unwrap();
    }
}
