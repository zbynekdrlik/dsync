---
paths:
  - "src/main.rs"
  - "src/config.rs"
  - "src/config/file.rs"
  - "src/config/file/tests.rs"
  - "src/state_dir.rs"
  - "src/state_dir/tests.rs"
---

# Adding a config key — migration safety

Since 1.21 (#116) the whole loader is `config::file::load(path, last_good)` in
`src/config/file.rs`, with injected paths, and `main`'s `load_config()` only passes the system
path. The migration is `migrate_config_json(&mut serde_json::Value) -> bool` there. Test the
migration AND the loading through them, with temp files (`src/config/file/tests.rs`) — the old
untestable hardcoded-path wrapper is how #68's panicking migration and #116's overwrite reached
the fleet.

## A file that does not parse is NEVER written (#116)

- 13.9.2026: a BOM write on strih failed the parse and the old `load_config` wrote the default
  template OVER the file — the NTP master lost its master role, the stream box its allowlist.
- Now: a good load refreshes `config.json.last-good` (atomic, only when changed); a failure runs
  that copy (`loaded_from = "last-good"`), else `Config::default()` (`"defaults"`), writes
  NOTHING, and keeps the clock alarm active + `/status.config_state` (`ConfigState`). The
  migration is written back only after the typed parse succeeded.
- A missing `config.json` WITH a last-good copy runs the copy (the template would drop the
  operator's settings); only a genuine first install (neither file) writes `TEMPLATE`.
- Decoding: a UTF-8 BOM is stripped, a UTF-16 LE/BE file with its BOM decoded
  (`decode_text`). A non-object root is an error before the migration (whose root `IndexMut`
  would panic on an array).
- `main` loads the config AFTER the logger is up (before 1.21 every load message was lost); the
  Windows service loads it once, in `my_service_main`.
- Every atomically rewritten file goes through `crate::atomic_file` (temp + fsync + rename +
  dir sync), shared with the master's `date-offset.json`.

## Never index a nested value mutably without proving it is an object

`serde_json`'s `IndexMut<&str>` **panics** on any value that is neither an object nor null. The
top-level `json["key"] = …` shape used by the older migrations is safe (the root is an object by
construction), but the nested form is not:

```rust
// PANICS on `"ntp_server_mode": true` — a very plausible hand-edit to enable it
} else if json["ntp_server_mode"].get("max_step_us").is_none() {
    json["ntp_server_mode"]["max_step_us"] = serde_json::json!(100_000);

// Correct: prove it is an object, and leave anything else alone
} else if let Some(obj) = json["ntp_server_mode"].as_object_mut() {
    if !obj.contains_key("max_step_us") {
        obj.insert("max_step_us".to_string(), serde_json::json!(100_000));
    }
}
```

A malformed value must fail the typed parse, which `config::file::load` answers with the
last-good copy (never with a panic and never with a write).
On a daemon under systemd / Windows Service, a startup panic is a restart loop, not a degraded
start — and on the NTP master that takes the whole fleet's time source with it. Always add a test
that feeds the migration a bool, a string, a number and an array.

## Adding a field to a config struct

- Give it `#[serde(default = "…")]` and update the struct's `Default` impl. A config file written by
  an older version must still parse.
- Add a test that a config WITHOUT the key parses and gets the intended default — especially when
  the default is the safe/bounded value and `0` would mean "unbounded" or "always" (see
  `test_ntp_server_config_without_max_step_us_defaults_to_bounded_68`).
- Guard nonsense values where a literal reading would latch: `ntp_stale_secs: 0` would make every
  node permanently stale, so it is floored at one query cadence at read time
  (`effective_stale_window`), and `max_step_us <= 0` is treated as unbounded rather than as "never
  correct anything".
- Only migrate a key into an EXISTING object when the operator is meant to tune it. As of
  camera-box issue 1073, `SystemConfig`'s `servo`/`filters` DO carry `#[serde(default)]` (with
  `Default` impls holding the former inline values), and `gm_allowlist` is `#[serde(default)]`
  empty — so a partial `"system": {"gm_allowlist": [...]}}` block now parses cleanly (servo/filters
  fall back to their defaults). A `system.gm_allowlist` key MAY therefore be safely migrated into an
  existing file, or added by an operator, without breaking the parse. (Note the whole-FILE caveat:
  `Config.ntp_server` also gained a `#[serde(default)]` for the same rollout, so even a file that
  contains ONLY `{"system": {"gm_allowlist": [...]}}` parses. Before #116 a failed parse
  overwrote the file with defaults; now it runs the last-good copy, loudly.) Since #116
  `gm_allowlist` also accepts a single string (`string_or_list`: split on commas / whitespace;
  `null` = empty; a number still fails the parse).
- A lenient deserializer is right for TUNING (a bad value = the default). For a SAFETY setting
  (`system.interface`, #131) a non-string must FAIL the parse instead: the last-good config then
  runs with the alarm, where a lenient "unset" would silently bring back the incident the key
  exists for.

## The state dir is resolved, never hard-coded (#147, 1.25)

`src/state_dir.rs` picks the directory once per process (`main.rs`'s `OnceLock`): `/etc/dsync`
(`C:\ProgramData\dsync`) when it exists, else the pre-1.25 dir IN PLACE, else it creates the new
one. `config.json` (+ `last-good`), `date-offset.json`, `drift.json` and the Windows log all derive
from it; `/status.state_dir` and the `[State] state dir …` line say which one runs.

- Never copy state between the two dirs: a second `drift.json` would seed the stability gate's
  `--cold` run, which moves aside only the legacy one (ROZHODNUTÉ #147).
- A new state file goes through `StateDir` (a new path method), never a fresh `const` path.
- The tray only LOOKS UP the dir (`state_dir::existing_platform`); it never creates `/etc`.
- An installer must not create an empty new dir beside a legacy one: the daemon would then pick
  the empty new dir and lose the config (install.sh / install.ps1 follow the same rule).
