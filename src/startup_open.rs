//! dsync#206 — the UNPINNED startup open of the PTP receive path.
//!
//! mbc-snv, 19.11.2026 01:53: at boot the Npcap device list held only `NPF_Loopback`, the open
//! failed (`Interface '{CC54…}' not found`), and the error ended the service loop for good while
//! the process stayed up (stale `/status`, no 31900 time).
//!
//! The decision is a pure function of an injected open and an injected "unopened" backend, so it
//! is unit-tested on Linux.

//! An open error no longer ends the startup: the backend starts UNOPENED (the state a failed
//! re-join leaves, `recv_packet` reads nothing), the sync loop runs (NTP-only once PTP is stale,
//! the 31900 time server answers), the clock alarm names the reason, and the controller's #112
//! re-join opens the path on its schedule (10 s, then 30 / 60 / 120 / 300 s) through the same
//! selection and open as here.

use anyhow::Result;
use log::error;

/// What the startup got: the backend, and why its receive path is not open (`None` = open).
pub struct StartupOpen<N> {
    pub network: N,
    pub not_open: Option<String>,
}

/// Open the receive path for an unpinned startup; on an error, the `unopened` backend and the
/// reason. Never an error itself: a missing device must not end the service loop.
pub fn open_unpinned<N>(
    open: impl FnOnce() -> Result<N>,
    unopened: impl FnOnce() -> N,
) -> Result<StartupOpen<N>> {
    match open() {
        Ok(network) => Ok(StartupOpen {
            network,
            not_open: None,
        }),
        Err(e) => {
            let why = format!("{e:#}");
            error!(
                "[NET] the PTP receive path could not be opened at start: {} -- running without \
                 it (NTP-only once PTP is stale), re-joining on the PTP re-join schedule \
                 (10 s, then 30/60/120/300 s)",
                why
            );
            Ok(StartupOpen {
                network: unopened(),
                not_open: Some(why),
            })
        }
    }
}

#[cfg(test)]
mod tests;
