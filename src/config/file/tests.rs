//! dantesync#116 — loading `config.json` never destroys it (the 13.9.2026 strih incident: a BOM
//! write made the parse fail and the daemon overwrote the master's config with the defaults).

use super::*;
use std::path::Path;

/// strih's real shape: the NTP master, with a restricting allowlist.
const MASTER: &str = r#"{
  "ntp_server": "10.77.8.2",
  "ntp_server_mode": { "enabled": true, "port": 123, "stratum": 3, "max_step_us": 100000 },
  "http_status": { "enabled": true, "port": 8898 },
  "system": { "gm_allowlist": ["10.77.9.230"], "interface": "enp56s0" }
}"#;

struct Dir {
    _tmp: tempfile::TempDir,
    config: PathBuf,
    last_good: PathBuf,
}

fn dir() -> Dir {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = tmp.path().join("config.json");
    let last_good = last_good_path(&config);
    Dir {
        _tmp: tmp,
        config,
        last_good,
    }
}

fn bom(text: &str) -> Vec<u8> {
    let mut b = vec![0xEF, 0xBB, 0xBF];
    b.extend_from_slice(text.as_bytes());
    b
}

fn read(p: &Path) -> Vec<u8> {
    std::fs::read(p).expect("read")
}

fn assert_is_master(cfg: &Config) {
    assert!(cfg.ntp_server_mode.enabled, "still the NTP master");
    assert_eq!(cfg.system.gm_allowlist, vec!["10.77.9.230".to_string()]);
}

#[test]
fn the_last_good_copy_sits_beside_the_config_116() {
    assert_eq!(
        last_good_path(Path::new("/etc/dantesync/config.json")),
        PathBuf::from("/etc/dantesync/config.json.last-good")
    );
}

#[test]
fn a_utf8_bom_is_stripped_and_the_file_loads_116() {
    let d = dir();
    std::fs::write(&d.config, bom(MASTER)).expect("seed");
    let loaded = load(&d.config, &d.last_good);
    assert_eq!(loaded.state.loaded_from, LOADED_FROM_FILE);
    assert_eq!(loaded.state.error, None);
    assert_is_master(&loaded.config);
}

#[test]
fn a_utf16_file_with_its_bom_loads_116() {
    // PowerShell 5's Out-File default.
    let mut le = vec![0xFF, 0xFE];
    for u in MASTER.encode_utf16() {
        le.extend_from_slice(&u.to_le_bytes());
    }
    let mut be = vec![0xFE, 0xFF];
    for u in MASTER.encode_utf16() {
        be.extend_from_slice(&u.to_be_bytes());
    }
    for bytes in [le, be] {
        let parsed = parse(&bytes).expect("decoded and parsed");
        assert_is_master(&parsed.config);
    }
    assert!(
        decode_text(&[0xFF, 0xFE, 0x41]).is_err(),
        "odd UTF-16 length"
    );
}

#[test]
fn a_good_load_keeps_a_last_good_copy_and_leaves_an_up_to_date_file_alone_116() {
    let d = dir();
    // Already migrated: nothing to add, so the operator's file is not rewritten.
    let full = r#"{"_ntp_server_examples":"x","ntp_server":"10.77.8.2",
        "ntp_server_mode":{"enabled":true,"port":123,"stratum":3,"max_step_us":100000},
        "http_status":{"enabled":true,"port":8898},
        "system":{"gm_allowlist":["10.77.9.230"]}}"#;
    std::fs::write(&d.config, full).expect("seed");
    let loaded = load(&d.config, &d.last_good);
    assert_eq!(loaded.state.loaded_from, LOADED_FROM_FILE);
    assert_eq!(read(&d.config), full.as_bytes(), "untouched");
    let copy = parse(&read(&d.last_good)).expect("the last-good copy parses");
    assert_is_master(&copy.config);
    assert!(
        !d.config.with_extension("json.last-good.tmp").exists(),
        "written by temp + rename"
    );
}

#[test]
fn a_broken_file_is_never_overwritten_and_the_last_good_copy_runs_116() {
    let d = dir();
    std::fs::write(&d.config, MASTER).expect("seed");
    assert_eq!(
        load(&d.config, &d.last_good).state.loaded_from,
        LOADED_FROM_FILE
    );
    // The 13.9.2026 shape: an operator write that does not parse (a wrongly-shaped value).
    let broken = r#"{"ntp_server": "10.77.8.2", "ntp_server_mode": {"enabled": "yes"#;
    std::fs::write(&d.config, broken).expect("break it");

    let loaded = load(&d.config, &d.last_good);
    assert_eq!(
        read(&d.config),
        broken.as_bytes(),
        "config.json NOT overwritten"
    );
    assert_eq!(loaded.state.loaded_from, LOADED_FROM_LAST_GOOD);
    assert!(loaded
        .state
        .error
        .as_deref()
        .unwrap_or("")
        .contains("not JSON"));
    assert_is_master(&loaded.config);
    let reason = loaded.state.alarm_reason().expect("an alarm");
    assert!(reason.contains("running the last-good config"), "{reason}");
}

#[test]
fn a_typed_parse_failure_is_never_overwritten_either_116() {
    let d = dir();
    std::fs::write(&d.config, MASTER).expect("seed");
    load(&d.config, &d.last_good);
    // Valid JSON, wrong type: used to log "the existing file will be overwritten".
    let wrong = r#"{"ntp_server": 5}"#;
    std::fs::write(&d.config, wrong).expect("break it");
    let loaded = load(&d.config, &d.last_good);
    assert_eq!(read(&d.config), wrong.as_bytes());
    assert_eq!(loaded.state.loaded_from, LOADED_FROM_LAST_GOOD);
    assert!(loaded
        .state
        .error
        .as_deref()
        .unwrap_or("")
        .contains("not a valid config"));
}

#[test]
fn without_a_last_good_copy_the_defaults_run_loudly_and_the_file_stays_116() {
    let d = dir();
    let broken = "{ not json";
    std::fs::write(&d.config, broken).expect("seed");
    let loaded = load(&d.config, &d.last_good);
    assert_eq!(read(&d.config), broken.as_bytes(), "never overwritten");
    assert_eq!(loaded.state.loaded_from, LOADED_FROM_DEFAULTS);
    assert!(loaded.state.error.is_some());
    assert!(!loaded.config.ntp_server_mode.enabled);
    assert!(!d.last_good.exists(), "nothing is written on a failure");
    let reason = loaded.state.alarm_reason().expect("an alarm");
    assert!(reason.contains("running defaults"), "{reason}");
}

#[test]
fn a_broken_last_good_copy_falls_through_to_the_defaults_with_both_errors_116() {
    let d = dir();
    std::fs::write(&d.config, "[]").expect("seed");
    std::fs::write(&d.last_good, "{torn").expect("seed");
    let loaded = load(&d.config, &d.last_good);
    assert_eq!(loaded.state.loaded_from, LOADED_FROM_DEFAULTS);
    let e = loaded.state.error.expect("error");
    assert!(
        e.contains("not a JSON object") && e.contains("last-good"),
        "{e}"
    );
}

#[test]
fn a_removed_config_with_a_last_good_copy_runs_the_copy_and_writes_nothing_116() {
    let d = dir();
    std::fs::write(&d.config, MASTER).expect("seed");
    load(&d.config, &d.last_good);
    std::fs::remove_file(&d.config).expect("rm");
    let loaded = load(&d.config, &d.last_good);
    assert_eq!(loaded.state.loaded_from, LOADED_FROM_LAST_GOOD);
    assert_is_master(&loaded.config);
    assert!(
        !d.config.exists(),
        "the template is not written over the intent"
    );
}

#[test]
fn a_first_install_writes_the_template_and_runs_it_116() {
    let d = dir();
    let loaded = load(&d.config, &d.last_good);
    assert_eq!(loaded.state, ConfigState::file());
    assert_eq!(read(&d.config), TEMPLATE.as_bytes());
    assert!(d.last_good.exists());
    assert_eq!(loaded.config.ntp_server, "10.77.8.2");
    assert_eq!(
        loaded.config.system.interface, None,
        "the template pins nothing"
    );
}

#[test]
fn a_migration_is_written_back_only_once_the_file_is_known_good_116() {
    let d = dir();
    std::fs::write(&d.config, r#"{"ntp_server": "10.0.0.1"}"#).expect("seed");
    let loaded = load(&d.config, &d.last_good);
    assert_eq!(loaded.state.loaded_from, LOADED_FROM_FILE);
    let written = parse(&read(&d.config)).expect("parses");
    assert!(!written.migrated, "the migration was written back");
    assert_eq!(written.config.ntp_server, "10.0.0.1");
}

#[test]
fn a_scalar_gm_allowlist_is_accepted_116() {
    let one = parse(br#"{"system": {"gm_allowlist": "10.77.9.230"}}"#).expect("parses");
    assert_eq!(one.config.system.gm_allowlist, vec!["10.77.9.230"]);
    let many =
        parse(br#"{"system": {"gm_allowlist": "10.77.9.230, 10.77.9.0/24"}}"#).expect("parses");
    assert_eq!(
        many.config.system.gm_allowlist,
        vec!["10.77.9.230", "10.77.9.0/24"]
    );
    let null = parse(br#"{"system": {"gm_allowlist": null}}"#).expect("parses");
    assert!(null.config.system.gm_allowlist.is_empty());
    assert!(parse(br#"{"system": {"gm_allowlist": 5}}"#).is_err());
}

#[test]
fn a_root_that_is_not_an_object_fails_cleanly_instead_of_panicking_in_the_migration_116() {
    for root in ["[]", "5", "\"x\"", "true", "null"] {
        assert!(parse(root.as_bytes()).is_err(), "{root}");
    }
}

#[test]
fn the_built_in_template_parses_116() {
    let t = parse(TEMPLATE.as_bytes()).expect("parses");
    assert!(!t.migrated, "the template carries every migrated key");
}

#[test]
fn the_state_serializes_for_status_116() {
    let s = ConfigState {
        loaded_from: LOADED_FROM_LAST_GOOD.to_string(),
        error: Some("not JSON".to_string()),
    };
    let v = serde_json::to_value(&s).expect("json");
    assert_eq!(
        v,
        serde_json::json!({"loaded_from": "last-good", "error": "not JSON"})
    );
    assert_eq!(ConfigState::file().alarm_reason(), None);
}

#[test]
fn a_first_install_that_cannot_write_the_template_says_defaults_116() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = tmp.path().join("no/such/dir/config.json");
    let loaded = load(&config, &last_good_path(&config));
    assert_eq!(loaded.state.loaded_from, LOADED_FROM_DEFAULTS);
    assert!(loaded
        .state
        .error
        .as_deref()
        .unwrap_or("")
        .contains("cannot be written"));
}
