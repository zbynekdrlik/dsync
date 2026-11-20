use anyhow::Result;
use clap::Parser;
use log::{error, info, warn};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(windows)]
use windows::Win32::Media::timeBeginPeriod;
#[cfg(windows)]
use windows::Win32::System::Threading::{
    GetCurrentProcess, SetPriorityClass, HIGH_PRIORITY_CLASS, REALTIME_PRIORITY_CLASS,
};

#[cfg(windows)]
use tokio::net::windows::named_pipe::NamedPipeServer;
#[cfg(windows)]
use windows::core::PCWSTR;
#[cfg(windows)]
use windows::Win32::Foundation::HLOCAL;
#[cfg(windows)]
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
#[cfg(windows)]
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
#[cfg(windows)]
use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
#[cfg(windows)]
use windows::Win32::System::Pipes::{CreateNamedPipeW, NAMED_PIPE_MODE};

// Constants for Pipe (Manual definition to avoid import issues)
#[cfg(windows)]
const PIPE_ACCESS_OUTBOUND: u32 = 0x00000002;
#[cfg(windows)]
const FILE_FLAG_OVERLAPPED: u32 = 0x40000000;
#[cfg(windows)]
const PIPE_UNLIMITED_INSTANCES: u32 = 255;

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn LocalFree(hMem: HLOCAL) -> HLOCAL;
}

// Service Imports
#[cfg(windows)]
use std::ffi::OsString;
#[cfg(windows)]
use windows_service::{
    define_windows_service,
    service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
};

// Use library crate modules
use dsync::{
    clock, config, controller, date_step_trigger, http_status, ntp, ntp_server, state_dir, status,
    sync_loop, time_server,
};

use config::file::{Config, ConfigState, Loaded};
use config::{HttpStatusConfig, NtpServerConfig, SystemConfig};
use controller::PtpController;
use status::SyncStatus;

mod service;
use service::ptp_join::{join_ptp_network, StartupJoin};
use service::singleton::acquire_singleton_lock;

/// dsync#147: the state directory, resolved ONCE per process (`dsync::state_dir`): the new
/// directory, else the legacy one in place, else a created new one. Every state file derives
/// from it — `config.json` (dsync#116: a file that does not parse is never overwritten; the
/// loader keeps `config.json.last-good` beside it), `date-offset.json` (dsync#126: the NTP
/// master's fleet date offset across a restart, only the master writes it), `drift.json`
/// (dsync#142: the learned frequency, written only while LOCKED) and the Windows service log.
/// A `OnceLock`, so the Windows service entry reuses the resolution `main` made.
static STATE_DIR: std::sync::OnceLock<state_dir::StateDir> = std::sync::OnceLock::new();

fn state_dir() -> &'static state_dir::StateDir {
    STATE_DIR.get_or_init(state_dir::resolve_platform)
}

/// Log which state directory is used and why (once the logger runs).
fn log_state_dir() {
    let dir = state_dir();
    match dir.choice {
        state_dir::Choice::CreateFailed => error!("{}", dir.describe()),
        _ => info!("{}", dir.describe()),
    }
}

fn load_config() -> Loaded {
    let path = state_dir().config_path();
    config::file::load(&path, &config::file::last_good_path(&path))
}

/// dsync#164 / #193: poll the bounded READY gate and, on its single reason, log it and tell
/// systemd `READY=1` with the reason in `STATUS=`. `stage` names where startup is.
fn poll_ready(
    ready_gate: &mut dsync::boot_ready::ReadyGate,
    placement: std::result::Result<&str, &str>,
    stage: &str,
) {
    let Some(reason) = ready_gate.poll(Instant::now(), placement) else {
        return;
    };
    match &reason {
        dsync::boot_ready::ReadyReason::Placed(how) => {
            info!("[READY] the wall is placed ({}): systemd READY=1", how)
        }
        dsync::boot_ready::ReadyReason::TimedOut(what) => warn!(
            "[READY] the wall is not placed after {} s ({}; {}): systemd READY=1 anyway, so the \
             units ordered after dsync are not held for ever",
            dsync::boot_ready::READY_PLACEMENT_BOUND.as_secs(),
            what,
            stage
        ),
    }
    #[cfg(unix)]
    {
        let _ = sd_notify::notify(
            false,
            &[
                sd_notify::NotifyState::Ready,
                sd_notify::NotifyState::Status(
                    format!(
                        "v{} | {} | {}",
                        env!("CARGO_PKG_VERSION"),
                        stage,
                        reason.status_text()
                    )
                    .as_str(),
                ),
            ],
        );
    }
}

/// Resolve the NTP server address: CLI arg takes priority, then config file value.
fn resolve_ntp_server(cli_ntp: &Option<String>, config: &Config) -> String {
    cli_ntp.clone().unwrap_or_else(|| config.ntp_server.clone())
}

#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// The PTP interface to pin (dsync#131): one NIC name, or several separated by commas
    /// (dsync#212, `enp1s0,enp3s0`: the listed NIC that has an IPv4). Overrides
    /// `system.interface`.
    #[arg(short, long)]
    interface: Option<String>,

    #[arg(long)]
    ntp_server: Option<String>,

    #[arg(long, default_value_t = false)]
    skip_ntp: bool,

    #[arg(long, default_value_t = false)]
    service: bool,
}

// Concrete Implementations for Traits
/// dsync#134 — one boot NTP reading on its own `boot-ntp` thread (its own client: name
/// resolution, the Windows Npcap transport and the burst all happen there), delivered on the
/// returned channel. A thread that cannot be started reads as a failed reading.
fn spawn_boot_ntp(
    server: &str,
    dscp: &dsync::dscp::DscpConfig,
) -> std::sync::mpsc::Receiver<controller::BootNtpResult> {
    let (tx, rx) = std::sync::mpsc::channel::<controller::BootNtpResult>();
    let server = server.to_string();
    let dscp = dscp.clone();
    let spawned = thread::Builder::new()
        .name("boot-ntp".to_string())
        .spawn(move || {
            let client = ntp::NtpClient::new(&server, &dscp);
            // The loop may have given up waiting (it logs that): a closed channel is fine.
            let _ = tx.send(
                client
                    .get_offset()
                    .map_err(|e| controller::BootNtpError::from_ntp(&e)),
            );
        });
    if let Err(e) = spawned {
        warn!("[NTP] could not start the boot-ntp thread: {}", e);
    }
    rx
}

// Windows uses Npcap for precise packet timestamps with HostHighPrec mode
// See net_pcap::NpcapPtpNetwork - uses KeQuerySystemTimePrecise() for synchronized timestamps

fn stop_conflicting_services() {
    #[cfg(windows)]
    {
        info!("Attempting to stop W32Time service...");
        match Command::new("net").args(["stop", "w32time"]).output() {
            Ok(out) => {
                if out.status.success() {
                    info!("W32Time stopped successfully.");
                } else {
                    // Ignore errors if already stopped
                }
            }
            Err(e) => warn!("Failed to execute 'net stop w32time': {}", e),
        }
    }

    #[cfg(unix)]
    {
        info!("Ensuring system NTP is disabled (timedatectl set-ntp false)...");
        match Command::new("timedatectl")
            .args(["set-ntp", "false"])
            .output()
        {
            Ok(_) => info!("NTP service disabled via timedatectl."),
            Err(e) => warn!("Failed to disable NTP via timedatectl (ignoring): {}", e),
        }
    }
}

fn enable_realtime_priority() {
    #[cfg(unix)]
    {
        unsafe {
            let policy = libc::SCHED_FIFO;
            let param = libc::sched_param { sched_priority: 50 };

            if libc::sched_setscheduler(0, policy, &param) == 0 {
                info!("Realtime priority (SCHED_FIFO, 50) enabled successfully.");
            } else {
                let err = std::io::Error::last_os_error();
                warn!(
                    "Failed to set realtime priority: {}. Latency might suffer.",
                    err
                );
            }
        }
    }
    #[cfg(windows)]
    {
        unsafe {
            if SetPriorityClass(GetCurrentProcess(), REALTIME_PRIORITY_CLASS).is_ok() {
                info!("Windows Realtime Priority enabled.");
            } else {
                warn!("Failed to set Windows Realtime Priority. Trying High...");
                if SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS).is_ok() {
                    info!("Windows High Priority enabled.");
                } else {
                    warn!("Failed to set Windows priority.");
                }
            }
            if timeBeginPeriod(1) == 0 {
                info!("Windows High-Res Timer (1ms) enabled.");
            } else {
                warn!("Failed to set Windows High-Res Timer.");
            }
        }
    }
}

// --- IPC Server (Windows) ---
#[cfg(windows)]
fn start_ipc_server(status: Arc<RwLock<SyncStatus>>) {
    thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Failed to build tokio runtime for IPC");

        rt.block_on(async move {
            use tokio::io::AsyncWriteExt;

            // Pre-allocate UTF-16 strings outside loop for performance
            let pipe_name_wide: Vec<u16> = r"\\.\pipe\dsync"
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            // SDDL: Grant full control to SYSTEM (service), read to Authenticated Users (tray)
            // GA = Generic All (for SYSTEM to write), GR = Generic Read (for users to read)
            // SY = SYSTEM, AU = Authenticated Users
            // MUST include SYSTEM explicitly since custom DACL overrides defaults
            let sddl_wide: Vec<u16> = "D:(A;;GA;;;SY)(A;;GR;;;AU)"
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();

            // Named pipe server loop
            loop {
                // Create pipe manually with Security Descriptor to allow Users to connect to Service

                let mut sd = PSECURITY_DESCRIPTOR::default();
                let mut sa = SECURITY_ATTRIBUTES {
                    nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                    lpSecurityDescriptor: std::ptr::null_mut(),
                    bInheritHandle: false.into(),
                };

                let handle = unsafe {
                    if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                        PCWSTR(sddl_wide.as_ptr()),
                        SDDL_REVISION_1,
                        &mut sd,
                        None,
                    )
                    .is_ok()
                    {
                        sa.lpSecurityDescriptor = sd.0;

                        let h = CreateNamedPipeW(
                            PCWSTR(pipe_name_wide.as_ptr()),
                            FILE_FLAGS_AND_ATTRIBUTES(PIPE_ACCESS_OUTBOUND | FILE_FLAG_OVERLAPPED),
                            NAMED_PIPE_MODE(0), // Byte mode (0) for Tokio compatibility
                            PIPE_UNLIMITED_INSTANCES,
                            1024,
                            1024,
                            0,
                            Some(&sa),
                        );

                        let _ = LocalFree(std::mem::transmute(sd));
                        h
                    } else {
                        // Fallback if SDDL fails (shouldn't happen)
                        windows::Win32::Foundation::INVALID_HANDLE_VALUE
                    }
                };

                if handle == windows::Win32::Foundation::INVALID_HANDLE_VALUE {
                    error!("Failed to create named pipe with SDDL. Retrying...");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }

                // Wrap in Tokio
                let mut server = unsafe {
                    match NamedPipeServer::from_raw_handle(handle.0 as *mut std::ffi::c_void) {
                        Ok(s) => s,
                        Err(e) => {
                            error!("Failed to wrap named pipe handle: {}", e);
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            continue;
                        }
                    }
                };

                if server.connect().await.is_ok() {
                    // Handle poisoned lock gracefully instead of panicking
                    let s = match status.read() {
                        Ok(guard) => guard.clone(),
                        Err(e) => {
                            error!("Status lock poisoned: {}. Skipping IPC write.", e);
                            continue;
                        }
                    };
                    // #47: shared serialization — the HTTP status endpoint (src/http_status.rs)
                    // calls the SAME SyncStatus::to_json_bytes(), so the pipe and the HTTP body
                    // can never silently drift apart into two different JSON shapes.
                    if let Ok(bytes) = s.to_json_bytes() {
                        let len = (bytes.len() as u32).to_le_bytes();
                        let _ = server.write_all(&len).await;
                        let _ = server.write_all(&bytes).await;
                    }
                }
            }
        });
    });
}

#[cfg(not(windows))]
fn start_ipc_server(_status: Arc<RwLock<SyncStatus>>) {
    // No-op on Linux for now (or implement Unix Domain Socket)
}

// --- Sync Loop ---
fn run_sync_loop(
    args: Args,
    running: Arc<AtomicBool>,
    system_config: SystemConfig,
    ntp_server_config: NtpServerConfig,
    http_status_config: HttpStatusConfig,
    config_state: ConfigState,
) -> Result<()> {
    // Notify systemd (Linux) that we are starting
    #[cfg(unix)]
    {
        let _ = sd_notify::notify(
            false,
            &[sd_notify::NotifyState::Status(
                format!("v{} | Starting...", env!("CARGO_PKG_VERSION")).as_str(),
            )],
        );
    }

    // dsync#164 + #193: the bounded READY counts from here, so a pinned NIC that is still missing
    // cannot hold the units ordered after dsync (camera-box#1483: lightdm) past the bound.
    let mut ready_gate = dsync::boot_ready::ReadyGate::new(Instant::now());

    // Initialize Shared Status
    let status_shared = Arc::new(RwLock::new(SyncStatus::default()));
    // dsync#116: the config source is visible at once, also while startup waits below.
    if let Ok(mut s) = status_shared.write() {
        s.config_state = Some(config_state.clone());
        s.state_dir = Some(state_dir().path.display().to_string());
    }

    // Start IPC Server immediately (so Tray App can connect even if network is down)
    start_ipc_server(status_shared.clone());

    // Start HTTP status endpoint (#47) — LAN automation reads the same status JSON
    // the named pipe serves, without a human or an SMB/pipe bridge in the loop.
    // dsync#126: its `POST /date/step` route (loopback only) reaches the sync loop over this
    // channel; the controller answers it below.
    let (date_step_tx, date_step_rx) = date_step_trigger::channel();
    if http_status_config.enabled {
        info!(
            "[HTTP-Status] Enabled — starting on port {}",
            http_status_config.port
        );
        http_status::start_http_status_server(
            status_shared.clone(),
            http_status_config.port,
            Some(date_step_tx),
        );
    } else {
        info!("[HTTP-Status] Disabled by config");
    }

    // UDP Time Query Server for network time verification: bound here, it answers on a thread of
    // its own once the sync loop starts (dsync#136, `sync_loop::run`).
    let time_server = match time_server::TimeServer::new() {
        Ok(ts) => Some(ts),
        Err(e) => {
            warn!(
                "Failed to start Time Query Server on port {}: {} (continuing without it)",
                time_server::TIME_SERVER_PORT,
                e
            );
            None
        }
    };

    stop_conflicting_services();
    enable_realtime_priority();

    let sys_clock = match clock::PlatformClock::new() {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to initialize system clock adjustment: {}", e);
            return Err(e);
        }
    };
    info!("System clock control initialized.");

    // dsync#131: the PTP interface — pinned (wait for it, never another NIC) or not.
    let Some(StartupJoin {
        network,
        pinned,
        pinned_name,
        iface: joined_iface,
        ip: joined_ip,
        not_open,
    }) = join_ptp_network(
        &args,
        &system_config,
        &running,
        &status_shared,
        &config_state,
        &mut ready_gate,
    )?
    else {
        return Ok(());
    };

    let ntp_server = args
        .ntp_server
        .as_deref()
        .expect("ntp_server must be resolved before run_sync_loop");
    // dsync#52: capture the DSCP config before `system_config` is moved into
    // the controller below, so both the client and the server socket can be marked.
    let dscp_config = system_config.dscp.clone();
    // dsync#136: ONE client (on Windows one Npcap transport) for the controller and its
    // `ntp-burst` thread, which measures the periodic burst off the loop.
    let ntp_client = Arc::new(ntp::NtpClient::new(ntp_server, &dscp_config));

    // dsync#129 slice 2: the fallback time source, opened below once PTP joined.
    let fallback_config = system_config.fallback.clone();
    let mut controller = PtpController::new(
        sys_clock,
        network,
        ntp_client.clone(),
        status_shared,
        system_config,
    );
    controller.set_date_step_requests(date_step_rx);
    // dsync#142: seed the servo with the learned frequency before the warmup (or log why not).
    controller.open_drift_file(&state_dir().drift_path());
    // dsync#131 / #116: /status names the pin, the joined interface and the config source.
    controller.set_ptp_interface(pinned, &joined_iface, joined_ip);
    controller.set_ptp_interface_name(pinned_name);
    // dsync#206: no receive path yet: the alarm names why, the #112 re-join opens it.
    if let Some(why) = not_open {
        controller.note_ptp_not_open(&why);
    }
    // dsync#129 slice 2: the fallback time source on the PTP interface (off by default).
    if fallback_config.enabled {
        match dsync::fallback::UdpFallbackTransport::open(joined_ip) {
            Ok(transport) => {
                controller.enable_fallback(Box::new(transport), joined_ip);
                // A PTP re-join opens it again on the address it joined.
                controller.set_fallback_reopener(Box::new(|ip| {
                    let transport: Box<dyn dsync::fallback::FallbackTransport> =
                        Box::new(dsync::fallback::UdpFallbackTransport::open(ip)?);
                    Ok(transport)
                }));
            }
            Err(e) => error!(
                "[FALLBACK] could not open the fallback socket on {}: {:#} -- the fallback time \
                 source stays OFF",
                joined_ip, e
            ),
        }
    } else {
        info!("[FALLBACK] off (system.fallback.enabled is false)");
    }
    controller.set_config_state(config_state);
    let burst_client = ntp_client.clone();
    match dsync::ntp_burst::NtpBurstThread::spawn(
        move || burst_client.get_offset(),
        running.clone(),
    ) {
        Ok(burst) => controller.set_ntp_burst_runner(Box::new(burst)),
        Err(e) => warn!(
            "[NTP] could not start the ntp-burst thread: {} — the periodic NTP burst runs in the \
             sync loop (as before 1.20)",
            e
        ),
    }

    // dsync#126: the NTP master's saved fleet date offset skips the boot step below (restored
    // at the first PTP window); any other node removes a leftover one.
    controller.open_date_state(&state_dir().date_state_path(), ntp_server_config.enabled);

    // dsync#134: the boot NTP reading is measured on its own thread (its own client: name
    // resolution, the Windows Npcap transport and the burst all happen there) and handed to the
    // loop, which starts at once — the PTP windows and the fleet date no longer wait for an
    // upstream that answers slowly at boot (12 s on strih-pp, 25.10.2026).
    if !args.skip_ntp {
        info!("Using NTP Server: {}", ntp_server);
        controller.start_boot_ntp(spawn_boot_ntp(ntp_server, &dscp_config));
        // A deferred boot step (a rejected or given-up saved state) measures again the same way,
        // never inside the loop (review of PR 135).
        let again_server = ntp_server.to_string();
        let again_dscp = dscp_config.clone();
        controller
            .set_boot_ntp_spawner(Box::new(move || spawn_boot_ntp(&again_server, &again_dscp)));
    }

    // Start NTP server if enabled (this machine becomes the time source)
    if ntp_server_config.enabled {
        info!(
            "[NTP-Server] Starting NTP server mode (port {}, stratum {})",
            ntp_server_config.port, ntp_server_config.stratum
        );

        // Create NTP server
        match ntp_server::NtpServer::new(
            ntp_server_config.port,
            ntp_server_config.stratum,
            dscp_config.clone(),
        ) {
            Ok(mut ntp_srv) => {
                // #68: KEEP querying upstream. This host is the fleet's time
                // source, not UTC's — with the periodic queries off it free-ran
                // at the grandmaster's rate (1.04 s of UTC drift over two
                // days, with the whole fleet coherently following it).
                controller.configure_ntp_server_mode(ntp_server_config.max_step_us);

                // #68: serve a reference timestamp that reflects the last REAL
                // upstream sync instead of process start.
                ntp_srv.set_status_source(controller.get_status_shared());

                // Start NTP server in a SUPERVISED background thread (#68) —
                // if the loop ever exits unexpectedly it is re-bound and
                // restarted loudly, instead of the daemon silently serving
                // nothing until somebody restarts the service.
                let server_running = running.clone();
                thread::spawn(move || ntp_server::run_supervised(ntp_srv, server_running));

                info!("[NTP-Server] Active - other machines can sync from this host");
            }
            Err(e) => {
                error!(
                    "[NTP-Server] Failed to start: {} (continuing with PTP-only mode)",
                    e
                );
                // Continue without NTP server - PTP sync still works. dsync#126: not the
                // master, so a saved date offset is not restored and the skipped boot step runs.
                controller.abandon_date_state();
            }
        }
    }

    // dsync#88: every node that is NOT the NTP master follows the master's fleet date offset,
    // polled once per second from the master's 31900 time server on a background thread (DNS and
    // socket waits never touch this loop). An older master simply answers without the extension,
    // and the node keeps its local NTP date path.
    if controller.phase_lock_enabled() && !controller.ntp_server_mode() && !args.skip_ntp {
        controller.set_date_authority_source(Box::new(time_server::UdpAuthorityPoller::spawn(
            ntp_server.to_string(),
            running.clone(),
        )));
    }

    info!("Starting PTP Loop...");

    // dsync#164: systemd's READY=1 waits for the placed wall (the boot date decided), bounded,
    // so units ordered After=dsync.service under Type=notify start on a placed wall.
    #[cfg(unix)]
    {
        let _ = sd_notify::notify(
            false,
            &[sd_notify::NotifyState::Status(
                format!(
                    "v{} | PTP Loop Running | waiting for the boot date",
                    env!("CARGO_PKG_VERSION")
                )
                .as_str(),
            )],
        );
    }

    let mut last_log = Instant::now();
    let loop_status = controller.get_status_shared();

    // dsync#136: the loop itself lives in the library (`sync_loop::run`), where it is tested.
    sync_loop::run(time_server, loop_status, running.clone(), || {
        if last_log.elapsed() >= Duration::from_secs(10) {
            controller.tick_status();

            // Update systemd status with latest metrics
            #[cfg(unix)]
            {
                let s = controller.get_status_shared();
                if let Ok(status) = s.read() {
                    let status_str = if status.settled {
                        format!(
                            "v{} | Locked | Offset: {:.3} µs",
                            env!("CARGO_PKG_VERSION"),
                            status.offset_ns as f64 / 1000.0
                        )
                    } else if status.mode == "NTP-only" {
                        // dsync#112: no allowed PTP packet for 10 s; the loop re-joins.
                        let last = match status.last_ptp_rx_age_s {
                            Some(age) => format!("last {}s ago", age),
                            None => "none since start".to_string(),
                        };
                        format!(
                            "v{} | No PTP packets ({}) | NTP-only, re-joining",
                            env!("CARGO_PKG_VERSION"),
                            last
                        )
                    } else {
                        format!("v{} | Settling...", env!("CARGO_PKG_VERSION"))
                    };
                    let _ =
                        sd_notify::notify(false, &[sd_notify::NotifyState::Status(&status_str)]);
                };
            }

            last_log = Instant::now();
        }

        if let Err(e) = controller.process_loop_iteration() {
            warn!("Error in loop: {}", e);
        }

        if !ready_gate.sent() {
            poll_ready(
                &mut ready_gate,
                controller.wall_placement(),
                "PTP Loop Running",
            );
        }
    });

    info!("Sync Loop Exiting.");
    // dsync#142: keep the learned frequency for the next start (only when LOCKED).
    controller.save_drift_file_at_shutdown();
    #[cfg(unix)]
    {
        let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Stopping]);
    }
    Ok(())
}

// --- Windows Service Entry ---
#[cfg(windows)]
const SERVICE_NAME: &str = "dsync";

#[cfg(windows)]
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

#[cfg(windows)]
fn my_service_main(_arguments: Vec<OsString>) {
    // We need to reload config or pass it?
    // Windows Service entry doesn't allow easy closure capture without unsafe global.
    // But we can just reload it, it's cheap.
    let Loaded {
        config,
        state: config_state,
    } = load_config();

    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel();

    let event_handler = move |control_event| -> ServiceControlHandlerResult {
        match control_event {
            // dsync#142: a host shutdown/reboot ends the loop like a stop, so the learned
            // frequency is saved (Windows sends SHUTDOWN, not STOP, to a service at shutdown).
            ServiceControl::Stop | ServiceControl::Shutdown => {
                let _ = shutdown_tx.send(());
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    let status_handle = match service_control_handler::register(SERVICE_NAME, event_handler) {
        Ok(h) => h,
        Err(_) => return,
    };

    // Set Running
    let _ = status_handle.set_service_status(ServiceStatus {
        service_type: SERVICE_TYPE,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    });

    // Parse CLI args and resolve NTP server from config file
    let mut args = Args::parse();
    args.ntp_server = Some(resolve_ntp_server(&args.ntp_server, &config));

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    // Spawn the sync loop in a thread
    let handle = thread::spawn(move || {
        if let Err(e) = run_sync_loop(
            args,
            r,
            config.system,
            config.ntp_server_mode,
            config.http_status,
            config_state,
        ) {
            error!("Service loop failed: {}", e);
        }
    });

    // Wait for stop signal
    let _ = shutdown_rx.recv();

    // Stop. dsync#142: say StopPending with a wait hint first, so the SCM (also at a host
    // shutdown) waits for the loop to end and the learned frequency to be saved.
    let _ = status_handle.set_service_status(ServiceStatus {
        service_type: SERVICE_TYPE,
        current_state: ServiceState::StopPending,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 1,
        wait_hint: Duration::from_secs(10),
        process_id: None,
    });
    running.store(false, Ordering::SeqCst);
    let _ = handle.join();

    let _ = status_handle.set_service_status(ServiceStatus {
        service_type: SERVICE_TYPE,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    });
}

// Generate FFI wrapper for Windows service entry point
#[cfg(windows)]
define_windows_service!(ffi_service_main, my_service_main);

#[cfg(windows)]
fn run_service_logic(_args: Args) -> Result<()> {
    info!("Service logic starting...");
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)?;
    Ok(())
}

fn main() -> Result<()> {
    let mut args = Args::parse();

    #[cfg(windows)]
    if args.service {
        // Initialize File Logging for Service
        // dsync#147: in the resolved state directory (the legacy one in place on an old install).
        let log_path = state_dir().log_path();

        // Log Rotation (Simple: 1MB limit check on startup)
        if let Ok(metadata) = std::fs::metadata(&log_path) {
            if metadata.len() > 1_000_000 {
                let mut old_path = log_path.clone().into_os_string();
                old_path.push(".old");
                let _ = std::fs::rename(&log_path, old_path);
            }
        }

        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .write(true)
            .open(&log_path)
        {
            let target = env_logger::Target::Pipe(Box::new(file));
            env_logger::builder()
                .target(target)
                .filter_level(log::LevelFilter::Info)
                .format_timestamp_millis()
                .format_target(false) // Remove module path from logs
                .format_level(false) // Remove INFO/WARN prefix
                .init();
        } else {
            // Fallback
            env_logger::builder()
                .filter_level(log::LevelFilter::Info)
                .init();
        }

        info!("Service Started: v{}", env!("CARGO_PKG_VERSION"));
        log_state_dir();
        // The service entry (`my_service_main`) loads the config itself, after this logger.
        return run_service_logic(args);
    }

    // Console Mode Logging (clean format)
    env_logger::builder()
        .format_timestamp(None)
        .format_target(false) // Remove module path
        .format_level(false) // Remove INFO/WARN prefix
        .filter_level(log::LevelFilter::Info)
        .init();

    // Log Version immediately
    info!("dsync v{}", env!("CARGO_PKG_VERSION"));
    log_state_dir();

    // dsync#116: loaded after the logger, so every load / fallback decision is logged.
    let Loaded {
        config,
        state: config_state,
    } = load_config();
    // Resolve NTP server: CLI arg > config file > default
    args.ntp_server = Some(resolve_ntp_server(&args.ntp_server, &config));

    // Console Mode
    let _lock_file = match acquire_singleton_lock() {
        Ok(f) => f,
        Err(e) => {
            error!("{}", e);
            std::process::exit(1);
        }
    };

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    ctrlc::set_handler(move || {
        // dsync#142: SIGTERM (systemd stop) and SIGHUP too (ctrlc's `termination`), so a
        // stopped service ends the loop and saves the learned frequency.
        info!("Stop signal received (Ctrl+C / SIGTERM / SIGHUP). Shutting down...");
        r.store(false, Ordering::SeqCst);
    })?;

    run_sync_loop(
        args,
        running,
        config.system,
        config.ntp_server_mode,
        config.http_status,
        config_state,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_ntp(server: &str) -> Config {
        Config {
            ntp_server: server.to_string(),
            ntp_server_mode: NtpServerConfig::default(),
            http_status: HttpStatusConfig::default(),
            system: SystemConfig::default(),
        }
    }

    #[test]
    fn resolve_ntp_server_cli_arg_takes_priority_over_config() {
        let cli = Some("192.168.1.100".to_string());
        let config = config_with_ntp("10.0.0.1");
        assert_eq!(resolve_ntp_server(&cli, &config), "192.168.1.100");
    }

    #[test]
    fn resolve_ntp_server_falls_back_to_config_when_cli_is_none() {
        let cli = None;
        let config = config_with_ntp("10.0.0.1");
        assert_eq!(resolve_ntp_server(&cli, &config), "10.0.0.1");
    }

    #[test]
    fn resolve_ntp_server_falls_back_to_default_config_when_both_use_default() {
        let cli = None;
        let config = Config::default();
        assert_eq!(resolve_ntp_server(&cli, &config), "strih.lan");
    }

    #[test]
    fn resolve_ntp_server_explicit_default_ip_on_cli_is_not_overridden() {
        // If user explicitly passes --ntp-server 10.77.8.2, it should NOT be overridden
        let cli = Some("10.77.8.2".to_string());
        let config = config_with_ntp("192.168.1.50");
        assert_eq!(resolve_ntp_server(&cli, &config), "10.77.8.2");
    }
}
