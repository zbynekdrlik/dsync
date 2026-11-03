//! dsync#153 (owner, 3.11.2026): no PC keeps a mix of the old dantesync names and the new dsync.
//! `install.ps1` must remove every older name the fleet still had. mbc-snv still held
//! `C:\ProgramData\DanteTimeSync` with a December 2025 `dantetimesync.exe` after a 1.27.1
//! install, because the script only knew the DanteSync names.

const INSTALL: &str = include_str!("../install.ps1");

/// A line of `install.ps1` that removes `what`, matched by the command and its target.
fn removes(command: &str, target: &str) -> bool {
    INSTALL
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .any(|l| l.contains(command) && l.contains(target))
}

#[test]
fn install_removes_the_dantetimesync_data_dir_153() {
    assert!(removes("Remove-Item", r#""C:\ProgramData\DanteTimeSync""#));
}

#[test]
fn install_removes_the_dantetimesync_program_dir_153() {
    assert!(removes(
        "Remove-Item",
        r#""C:\Program Files\DanteTimeSync""#
    ));
}

#[test]
fn install_deletes_the_dantetimesync_service_and_process_153() {
    assert!(removes("sc.exe delete", "dantetimesync"), "the service");
    assert!(removes("Stop-Process", r#""dantetimesync""#), "the process");
}

#[test]
fn install_removes_the_dantetimesync_uninstall_entry_153() {
    assert!(removes(
        "Remove-Item",
        r"CurrentVersion\Uninstall\DanteTimeSync"
    ));
}

#[test]
fn install_removes_the_old_tray_run_keys_for_both_hives_153() {
    for hive in ["HKLM:", "HKCU:"] {
        for name in ["DanteTray", "DanteTimeSyncTray"] {
            let found = INSTALL.lines().map(str::trim).any(|l| {
                !l.starts_with('#')
                    && l.contains("Remove-ItemProperty")
                    && l.contains(hive)
                    && l.contains(r"CurrentVersion\Run")
                    && l.contains(&format!(r#"-Name "{name}""#))
            });
            assert!(found, "{hive} Run {name}");
        }
    }
}

#[test]
fn install_removes_the_old_tray_tasks_153() {
    for task in ["DanteTray", "DanteTimeSyncTray", "DanteSyncUpdate"] {
        assert!(
            removes("Unregister-ScheduledTask", &format!(r#""{task}""#)),
            "{task}"
        );
    }
}

#[test]
fn install_removes_the_dantetimesync_start_menu_shortcut_153() {
    assert!(removes("Remove-Item", "DanteTimeSync.lnk"));
}
