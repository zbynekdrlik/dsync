//! dsync#214 — the `Config` default / partial-parse / migration tests, moved verbatim from the
//! `dsync` binary's `main.rs` (they test this module, not the binary).

use super::file::{migrate_config_json, Config};
use super::SystemConfig;

#[test]
fn config_default_has_expected_ntp_server() {
    // dsync#153 (owner 2026-11-03): every node takes its NTP from the strih PC, the fleet's
    // NTP master. The old default 10.77.8.2 does not answer NTP on the audio network.
    let config = Config::default();
    assert_eq!(config.ntp_server, "strih.lan");
}

/// #68 review finding: the `max_step_us` migration indexed
/// `json["ntp_server_mode"]["max_step_us"]` mutably. `serde_json`'s
/// `IndexMut<&str>` PANICS on any value that is neither an object nor null,
/// so a hand-edited `"ntp_server_mode": true` (a very plausible attempt to
/// enable it) aborted the process at startup — on the fleet's clock master,
/// under a service manager, that is a restart loop rather than a degraded
/// start. The pre-existing code only ever indexed the top-level object and
/// had a deliberate log-loudly-and-fall-back path for malformed input; this
/// must not regress it.
#[test]
fn config_migration_never_panics_on_a_malformed_ntp_server_mode_68() {
    for malformed in [
        serde_json::json!(true),
        serde_json::json!("yes"),
        serde_json::json!(123),
        serde_json::json!([1, 2, 3]),
    ] {
        let mut json = serde_json::json!({
            "ntp_server": "10.77.8.2",
            "ntp_server_mode": malformed,
        });
        // Must return, not abort the daemon.
        let _ = migrate_config_json(&mut json);
    }
}

#[test]
fn config_migration_adds_max_step_us_to_an_existing_server_mode_object_68() {
    let mut json = serde_json::json!({
        "ntp_server": "10.77.8.2",
        "ntp_server_mode": {"enabled": true, "port": 123, "stratum": 3},
    });
    assert!(
        migrate_config_json(&mut json),
        "migration must report a change"
    );
    assert_eq!(json["ntp_server_mode"]["max_step_us"], 100_000);
    // Idempotent: a second pass changes nothing.
    assert!(!migrate_config_json(&mut json));
}

#[test]
fn config_deserializes_with_custom_ntp_server() {
    let json = r#"{"ntp_server": "172.16.0.5"}"#;
    let config: Config = serde_json::from_str(json).unwrap();
    assert_eq!(config.ntp_server, "172.16.0.5");
}

#[test]
fn config_deserializes_with_missing_system_uses_defaults() {
    let json = r#"{"ntp_server": "172.16.0.5"}"#;
    let config: Config = serde_json::from_str(json).unwrap();
    let default_system = SystemConfig::default();
    // system config should use defaults when omitted
    assert_eq!(
        format!("{:?}", config.system),
        format!("{:?}", default_system)
    );
}

/// camera-box issue 1073: a config file that specifies ONLY the new
/// `system.gm_allowlist` (no `ntp_server`) must parse — otherwise
/// `load_config` would take its overwrite-with-defaults path and SILENTLY
/// DELETE the operator's freshly-added allowlist during the fix rollout.
/// `ntp_server` falls back to the fleet default.
#[test]
fn config_with_only_gm_allowlist_parses_and_defaults_ntp_server_1073() {
    let json = r#"{"system": {"gm_allowlist": ["10.77.9.0/24"]}}"#;
    let config: Config =
        serde_json::from_str(json).expect("a minimal gm_allowlist-only config must parse");
    assert_eq!(
        config.ntp_server, "strih.lan",
        "missing ntp_server must fall back to the fleet default, not fail the parse"
    );
    assert_eq!(config.system.gm_allowlist, vec!["10.77.9.0/24".to_string()]);
}

// ========================================================================
// NTP SERVER MODE TESTS
// ========================================================================

#[test]
fn config_default_has_ntp_server_mode_disabled() {
    let config = Config::default();
    assert!(!config.ntp_server_mode.enabled);
    assert_eq!(config.ntp_server_mode.port, 123);
    assert_eq!(config.ntp_server_mode.stratum, 3);
}

#[test]
fn config_deserializes_with_ntp_server_mode_enabled() {
    let json = r#"{
        "ntp_server": "10.77.8.2",
        "ntp_server_mode": {
            "enabled": true,
            "port": 123,
            "stratum": 3
        }
    }"#;
    let config: Config = serde_json::from_str(json).unwrap();
    assert!(config.ntp_server_mode.enabled);
    assert_eq!(config.ntp_server_mode.port, 123);
    assert_eq!(config.ntp_server_mode.stratum, 3);
}

#[test]
fn config_deserializes_with_custom_ntp_server_mode_port() {
    let json = r#"{
        "ntp_server": "10.77.8.2",
        "ntp_server_mode": {
            "enabled": true,
            "port": 1123,
            "stratum": 2
        }
    }"#;
    let config: Config = serde_json::from_str(json).unwrap();
    assert!(config.ntp_server_mode.enabled);
    assert_eq!(config.ntp_server_mode.port, 1123);
    assert_eq!(config.ntp_server_mode.stratum, 2);
}

#[test]
fn config_deserializes_without_ntp_server_mode_uses_defaults() {
    let json = r#"{"ntp_server": "10.77.8.2"}"#;
    let config: Config = serde_json::from_str(json).unwrap();
    // ntp_server_mode should use defaults when omitted
    assert!(!config.ntp_server_mode.enabled);
    assert_eq!(config.ntp_server_mode.port, 123);
    assert_eq!(config.ntp_server_mode.stratum, 3);
}

// ========================================================================
// HTTP STATUS ENDPOINT CONFIG TESTS (#47)
// ========================================================================

#[test]
fn config_default_has_http_status_enabled_on_port_8898() {
    let config = Config::default();
    assert!(
        config.http_status.enabled,
        "HTTP status endpoint should be enabled by default (unattended reads is the point)"
    );
    assert_eq!(config.http_status.port, 8898);
}

#[test]
fn config_deserializes_with_http_status_disabled() {
    let json = r#"{
        "ntp_server": "10.77.8.2",
        "http_status": {
            "enabled": false,
            "port": 8898
        }
    }"#;
    let config: Config = serde_json::from_str(json).unwrap();
    assert!(!config.http_status.enabled);
    assert_eq!(config.http_status.port, 8898);
}

#[test]
fn config_deserializes_with_custom_http_status_port() {
    let json = r#"{
        "ntp_server": "10.77.8.2",
        "http_status": {
            "enabled": true,
            "port": 9898
        }
    }"#;
    let config: Config = serde_json::from_str(json).unwrap();
    assert!(config.http_status.enabled);
    assert_eq!(config.http_status.port, 9898);
}

#[test]
fn config_deserializes_without_http_status_uses_defaults() {
    // An OLDER config.json predating #47 has no "http_status" key at all —
    // #[serde(default)] must fill in the enabled-by-default value, not fail
    // to parse or silently disable the endpoint.
    let json = r#"{"ntp_server": "10.77.8.2"}"#;
    let config: Config = serde_json::from_str(json).unwrap();
    assert!(config.http_status.enabled);
    assert_eq!(config.http_status.port, 8898);
}

#[test]
fn config_deserializes_with_partial_http_status_object() {
    // #47 review finding: a hand-edited/partial config.json with "http_status"
    // PRESENT but missing "port" must still parse the WHOLE Config — before the
    // fix, HttpStatusConfig had no per-field #[serde(default)], so this exact
    // shape made serde_json::from_value::<Config> fail, and load_config()'s
    // fallback then silently overwrote the file with fresh defaults (losing
    // ntp_server and any other customization). See config.rs's own
    // test_http_status_config_partial_object_* for the leaf-struct-level proof.
    let json = r#"{
        "ntp_server": "10.77.8.2",
        "http_status": {
            "enabled": false
        }
    }"#;
    let config: Config = serde_json::from_str(json)
        .expect("a partial http_status object must not fail the whole Config parse");
    assert_eq!(config.ntp_server, "10.77.8.2");
    assert!(!config.http_status.enabled);
    assert_eq!(config.http_status.port, 8898, "missing port must default");
}
