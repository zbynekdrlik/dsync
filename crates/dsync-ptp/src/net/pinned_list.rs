//! dsync#212 — the pinned PTP interface as a LIST (`--interface enp1s0,enp3s0`).
//!
//! Every SNV/PP cambox has two onboard ports and the crew plugs the cable into either one. With
//! one pinned name and the cable in the other port, cam3 ran the whole 19.11.2026 service on NTP.
//! A pin now names one NIC or several; dsync joins the LISTED NIC that carries an IPv4 and never
//! an unlisted one (the owner's "explicit, never by chance", #131).
//!
//! The pin travels as ONE canonical spec string (`"enp1s0,enp3s0"`: names split on `,`,
//! trimmed, blanks dropped), so every `Option<String>` pin site of #131 is unchanged and a single
//! name is exactly today's pin. [`choose_pinned`] is the one place the list is decided.

use super::{PinnedInterfaceUnavailable, PinnedLookup};
use std::net::Ipv4Addr;

/// The names of the pin `spec`, in its order: split on `,`, trimmed, blanks dropped.
pub fn interface_names(spec: &str) -> impl Iterator<Item = &str> {
    spec.split(',').map(str::trim).filter(|n| !n.is_empty())
}

/// The canonical form of the pin `spec` (`" enp1s0 , enp3s0 ,"` → `"enp1s0,enp3s0"`); `None`
/// when it names no interface (blank = unset).
pub fn canonical_interface_spec(spec: &str) -> Option<String> {
    let names: Vec<&str> = interface_names(spec).collect();
    (!names.is_empty()).then(|| names.join(","))
}

/// dsync#212 — the listed NIC a join uses now, and what it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedChoice {
    /// The listed name chosen (`Ready` / `LinkLocalOnly`), or the whole canonical spec when no
    /// listed NIC carries an IPv4 (`NoIpv4` / `Missing`): what the alarm names.
    pub name: String,
    pub lookup: PinnedLookup,
}

impl PinnedChoice {
    /// The listed name and the IPv4 to join it on (a link-local one too: a re-join takes the
    /// NIC's address as it is), or the typed #131 error naming the NIC / the list.
    pub fn usable(self) -> Result<(String, Ipv4Addr), PinnedInterfaceUnavailable> {
        match self.lookup {
            PinnedLookup::Ready(ip) | PinnedLookup::LinkLocalOnly(ip) => Ok((self.name, ip)),
            PinnedLookup::NoIpv4 => Err(PinnedInterfaceUnavailable {
                name: self.name,
                missing: false,
            }),
            PinnedLookup::Missing => Err(PinnedInterfaceUnavailable {
                name: self.name,
                missing: true,
            }),
        }
    }
}

/// dsync#212 — choose among the listed NICs of `spec` (pure; `look` classifies ONE name).
///
/// The first listed NIC with a routable IPv4 wins, else the first with a link-local one, else
/// `NoIpv4` when any listed NIC exists, else `Missing`. `after` is the listed name joined now: on
/// a re-join the order starts AFTER it (wrapping round), so when both ports keep an IPv4 (a
/// static address on a port without carrier) a stale re-join tries the other port instead of
/// the silent one again. A single name is today's #131 pin. An unlisted NIC is never looked at.
pub fn choose_pinned(
    spec: &str,
    after: Option<&str>,
    mut look: impl FnMut(&str) -> PinnedLookup,
) -> PinnedChoice {
    let names: Vec<&str> = interface_names(spec).collect();
    let start = after
        .and_then(|joined| names.iter().position(|n| *n == joined))
        .map_or(0, |i| i + 1);
    let mut link_local: Option<(&str, Ipv4Addr)> = None;
    let mut exists = false;
    for &name in names.iter().cycle().skip(start).take(names.len()) {
        match look(name) {
            PinnedLookup::Ready(ip) => {
                return PinnedChoice {
                    name: name.to_string(),
                    lookup: PinnedLookup::Ready(ip),
                }
            }
            PinnedLookup::LinkLocalOnly(ip) => {
                link_local.get_or_insert((name, ip));
            }
            PinnedLookup::NoIpv4 => exists = true,
            PinnedLookup::Missing => {}
        }
    }
    match link_local {
        Some((name, ip)) => PinnedChoice {
            name: name.to_string(),
            lookup: PinnedLookup::LinkLocalOnly(ip),
        },
        None => PinnedChoice {
            name: names.join(","),
            lookup: if exists {
                PinnedLookup::NoIpv4
            } else {
                PinnedLookup::Missing
            },
        },
    }
}

/// `true` when the pin `spec` names more than one interface (the alarm then names the list).
pub fn is_interface_list(spec: &str) -> bool {
    interface_names(spec).nth(1).is_some()
}

#[cfg(test)]
mod tests;
