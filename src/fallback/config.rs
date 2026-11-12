//! dsync#129 slice 2 — `system.fallback`: whether this node takes part in the fallback time
//! source, and its priority.
//!
//! ```json
//! "system": { "fallback": { "enabled": true, "priority": 200 } }
//! ```
//!
//! Off by default: no socket is opened and nothing changes on a box that does not set it.
//! `priority` 1-255 makes the node a candidate source (higher wins), 0 = follow only.
//! Lenient as a whole: a block that does not parse (a priority beyond 255, a string, a bool)
//! leaves the feature OFF with a warning, never a failed config parse. Off is the safe state.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FallbackConfig {
    /// Take part at all: follow a fallback source, and with a priority > 0 become one.
    #[serde(default)]
    pub enabled: bool,
    /// 1-255 = a candidate source, the higher the better; 0 = never a source.
    #[serde(default)]
    pub priority: u8,
}

/// `system.fallback`: `null` or a missing key = off; anything that does not parse = off, loudly.
pub fn lenient<'de, D>(deserializer: D) -> Result<FallbackConfig, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(from_value(value))
}

fn from_value(value: serde_json::Value) -> FallbackConfig {
    if value.is_null() {
        return FallbackConfig::default();
    }
    match serde_json::from_value::<FallbackConfig>(value.clone()) {
        Ok(config) => config,
        Err(e) => {
            log::warn!(
                "[FALLBACK] system.fallback {value} is not valid ({e}) -- the fallback time source \
                 stays OFF (expected {{\"enabled\": bool, \"priority\": 0-255}})"
            );
            FallbackConfig::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SystemConfig;

    fn system(json: &str) -> SystemConfig {
        serde_json::from_str(json).expect("parses")
    }

    #[test]
    fn the_fallback_is_off_without_the_key_and_by_default_129() {
        assert_eq!(system("{}").fallback, FallbackConfig::default());
        assert!(!SystemConfig::default().fallback.enabled);
        assert_eq!(SystemConfig::default().fallback.priority, 0);
    }

    #[test]
    fn a_valid_block_is_read_129() {
        let s = system(r#"{"fallback": {"enabled": true, "priority": 200}}"#);
        assert_eq!(
            s.fallback,
            FallbackConfig {
                enabled: true,
                priority: 200
            }
        );
        let s = system(r#"{"fallback": {"enabled": true}}"#);
        assert_eq!(s.fallback.priority, 0, "follow only by default");
    }

    #[test]
    fn a_malformed_block_leaves_the_feature_off_and_never_fails_the_parse_129() {
        for bad in [
            r#"{"fallback": true}"#,
            r#"{"fallback": "on"}"#,
            r#"{"fallback": 7}"#,
            r#"{"fallback": [1, 2]}"#,
            r#"{"fallback": null}"#,
            r#"{"fallback": {"enabled": true, "priority": 256}}"#,
            r#"{"fallback": {"enabled": true, "priority": -1}}"#,
            r#"{"fallback": {"enabled": "yes", "priority": 9}}"#,
        ] {
            assert_eq!(system(bad).fallback, FallbackConfig::default(), "{bad}");
        }
    }

    #[test]
    fn the_block_round_trips_through_serialization_129() {
        let mut s = SystemConfig::default();
        s.fallback = FallbackConfig {
            enabled: true,
            priority: 9,
        };
        let back: SystemConfig =
            serde_json::from_str(&serde_json::to_string(&s).expect("ser")).expect("de");
        assert_eq!(back.fallback, s.fallback);
    }
}
