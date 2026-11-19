//! dsync#206 — the UNPINNED startup open of the PTP receive path.
//!
//! mbc-snv, 19.11.2026 01:53: at boot the Npcap device list held only `NPF_Loopback`, the open
//! failed (`Interface '{CC54…}' not found`), and the error ended the service loop for good while
//! the process stayed up (stale `/status`, no 31900 time).
//!
//! The decision is a pure function of an injected open and an injected "unopened" backend, so it
//! is unit-tested on Linux.

use anyhow::Result;

/// What the startup got: the backend, and why its receive path is not open (`None` = open).
pub struct StartupOpen<N> {
    pub network: N,
    pub not_open: Option<String>,
}

/// Open the receive path for an unpinned startup.
pub fn open_unpinned<N>(
    open: impl FnOnce() -> Result<N>,
    _unopened: impl FnOnce() -> N,
) -> Result<StartupOpen<N>> {
    let network = open()?;
    Ok(StartupOpen {
        network,
        not_open: None,
    })
}

#[cfg(test)]
mod tests;
