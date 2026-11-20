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

/// dsync#212 — in which order a join looks at the listed NICs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinnedOrder<'a> {
    /// The startup: the list order, a routable IPv4 before a link-local one (a link-local
    /// address at boot may be a placeholder until DHCP answers, #131).
    Startup,
    /// A re-join while PTP still arrives on the listed NIC `joined` (#191: its link-local
    /// address became routable): that NIC again whenever it is usable, else the startup order.
    Stay(&'a str),
    /// A re-join after PTP went silent on the listed NIC `joined`: the NEXT usable listed NIC
    /// after it (wrapping round, `joined` last), a routable or a link-local address alike. The
    /// cable moved, or both ports keep an address (a static one without carrier, a segment
    /// without DHCP): each stale re-join tries the next port, so the live one is reached and
    /// the silent one is never re-joined for ever.
    MoveOn(&'a str),
}

/// dsync#212 — choose among the listed NICs of `spec` in `order` (pure; `look` classifies ONE
/// name). Nothing usable: `NoIpv4` when any listed NIC exists, else `Missing`, both naming the
/// whole list. A single name is today's #131 pin whatever the order. An unlisted NIC is never
/// looked at.
pub fn choose_pinned(
    spec: &str,
    order: PinnedOrder<'_>,
    mut look: impl FnMut(&str) -> PinnedLookup,
) -> PinnedChoice {
    let names: Vec<&str> = interface_names(spec).collect();
    let position = |joined: &str| names.iter().position(|n| *n == joined);
    let chosen = |name: &str, lookup| PinnedChoice {
        name: name.to_string(),
        lookup,
    };
    let usable = |lookup| {
        matches!(
            lookup,
            PinnedLookup::Ready(_) | PinnedLookup::LinkLocalOnly(_)
        )
    };
    let mut exists = false;
    match order {
        PinnedOrder::Stay(joined) if position(joined).is_some() => {
            let lookup = look(joined);
            if usable(lookup) {
                return chosen(joined, lookup);
            }
            exists |= lookup == PinnedLookup::NoIpv4;
        }
        PinnedOrder::MoveOn(joined) => {
            let start = position(joined).map_or(0, |i| i + 1);
            for &name in names.iter().cycle().skip(start).take(names.len()) {
                let lookup = look(name);
                if usable(lookup) {
                    return chosen(name, lookup);
                }
                exists |= lookup == PinnedLookup::NoIpv4;
            }
        }
        _ => {}
    }
    let mut link_local: Option<(&str, Ipv4Addr)> = None;
    if !matches!(order, PinnedOrder::MoveOn(_)) {
        for &name in &names {
            match look(name) {
                PinnedLookup::Ready(ip) => return chosen(name, PinnedLookup::Ready(ip)),
                PinnedLookup::LinkLocalOnly(ip) => {
                    link_local.get_or_insert((name, ip));
                }
                PinnedLookup::NoIpv4 => exists = true,
                PinnedLookup::Missing => {}
            }
        }
    }
    match link_local {
        Some((name, ip)) => chosen(name, PinnedLookup::LinkLocalOnly(ip)),
        None => chosen(
            &names.join(","),
            if exists {
                PinnedLookup::NoIpv4
            } else {
                PinnedLookup::Missing
            },
        ),
    }
}

/// dsync#212 — the order of a pinned re-join from the listed NIC `joined`: [`PinnedOrder::Stay`]
/// while the TRUSTED grandmaster's time still flows on it (the #191 link-local leave), else
/// [`PinnedOrder::MoveOn`] (it went silent there: the cable moved, or the port carries only a
/// foreign grandmaster). dsync#215: the caller judges it (`TrustedPtp`), since only the caller
/// knows the `gm_allowlist`; the backend's own count of time messages included foreign ones.
pub fn rejoin_order(joined: &str, trusted: crate::network::TrustedPtp) -> PinnedOrder<'_> {
    match trusted {
        crate::network::TrustedPtp::Flowing => PinnedOrder::Stay(joined),
        crate::network::TrustedPtp::Silent => PinnedOrder::MoveOn(joined),
    }
}

/// `true` when the pin `spec` names more than one interface (the alarm then names the list).
pub fn is_interface_list(spec: &str) -> bool {
    interface_names(spec).nth(1).is_some()
}

#[cfg(test)]
mod tests;
