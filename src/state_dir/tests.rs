use super::*;

fn dirs(root: &Path) -> (PathBuf, PathBuf) {
    (root.join("dsync"), root.join("legacy"))
}

#[test]
fn the_new_dir_wins_when_it_exists() {
    let root = tempfile::tempdir().unwrap();
    let (new, legacy) = dirs(root.path());
    std::fs::create_dir(&new).unwrap();
    let got = resolve(&new, &legacy);
    assert_eq!(got.path, new);
    assert_eq!(got.choice, Choice::New);
    assert!(!legacy.exists(), "the legacy dir is never created");
}

#[test]
fn the_new_dir_wins_even_when_the_legacy_dir_exists_too() {
    let root = tempfile::tempdir().unwrap();
    let (new, legacy) = dirs(root.path());
    std::fs::create_dir(&new).unwrap();
    std::fs::create_dir(&legacy).unwrap();
    std::fs::write(legacy.join("drift.json"), "{}").unwrap();
    let got = resolve(&new, &legacy);
    assert_eq!(got.path, new);
    assert_eq!(got.choice, Choice::New);
    assert!(
        !new.join("drift.json").exists(),
        "nothing is copied from the legacy dir"
    );
}

#[test]
fn only_the_legacy_dir_is_used_in_place_and_nothing_is_created_or_copied() {
    let root = tempfile::tempdir().unwrap();
    let (new, legacy) = dirs(root.path());
    std::fs::create_dir(&legacy).unwrap();
    std::fs::write(legacy.join("config.json"), "{}").unwrap();
    let got = resolve(&new, &legacy);
    assert_eq!(got.path, legacy);
    assert_eq!(got.choice, Choice::Legacy);
    assert_eq!(got.config_path(), legacy.join("config.json"));
    assert_eq!(got.drift_path(), legacy.join("drift.json"));
    assert_eq!(got.date_state_path(), legacy.join("date-offset.json"));
    assert!(
        !new.exists(),
        "the new dir is not created beside a legacy one"
    );
}

#[test]
fn neither_dir_creates_the_new_one() {
    let root = tempfile::tempdir().unwrap();
    let (new, legacy) = dirs(root.path());
    let got = resolve(&new, &legacy);
    assert_eq!(got.path, new);
    assert_eq!(got.choice, Choice::Created);
    assert!(new.is_dir());
    assert!(!legacy.exists());
    // The next start finds it.
    assert_eq!(resolve(&new, &legacy).choice, Choice::New);
}

#[test]
fn a_failed_create_keeps_the_new_path_and_reports_the_error() {
    let root = tempfile::tempdir().unwrap();
    // A file where a parent directory must be: create_dir_all fails.
    let blocker = root.path().join("blocker");
    std::fs::write(&blocker, "x").unwrap();
    let new = blocker.join("dsync");
    let legacy = root.path().join("legacy");
    let got = resolve(&new, &legacy);
    assert_eq!(got.path, new);
    assert_eq!(got.choice, Choice::CreateFailed);
    assert!(got.error.is_some());
    assert!(got.describe().contains("could not be created"));
}

#[test]
fn existing_never_creates_and_prefers_the_new_dir() {
    let root = tempfile::tempdir().unwrap();
    let (new, legacy) = dirs(root.path());
    assert_eq!(existing(&new, &legacy), None);
    assert!(!new.exists());
    std::fs::create_dir(&legacy).unwrap();
    assert_eq!(existing(&new, &legacy), Some(legacy.clone()));
    std::fs::create_dir(&new).unwrap();
    assert_eq!(existing(&new, &legacy), Some(new.clone()));
}

#[test]
fn the_log_line_names_the_dir_and_the_rule() {
    let root = tempfile::tempdir().unwrap();
    let (new, legacy) = dirs(root.path());
    std::fs::create_dir(&legacy).unwrap();
    let got = resolve(&new, &legacy);
    let line = got.describe();
    assert!(line.contains(&legacy.display().to_string()), "{}", line);
    assert!(line.contains("legacy dir used in place"), "{}", line);
    assert_eq!(got.choice.as_str(), "legacy");
    assert_eq!(Choice::New.as_str(), "new");
    assert_eq!(Choice::Created.as_str(), "created");
    assert_eq!(Choice::CreateFailed.as_str(), "create-failed");
}

#[test]
fn the_platform_dirs_are_the_renamed_and_the_legacy_ones() {
    #[cfg(not(windows))]
    {
        assert_eq!(NEW_DIR, "/etc/dsync");
        assert_eq!(LEGACY_DIR, "/etc/dantesync");
    }
    #[cfg(windows)]
    {
        assert_eq!(NEW_DIR, r"C:\ProgramData\dsync");
        assert_eq!(LEGACY_DIR, r"C:\ProgramData\DanteSync");
    }
}
