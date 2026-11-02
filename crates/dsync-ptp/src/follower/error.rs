//! dsync#136 — why [`super::Follower::start`] failed, as a type an application can act on: log the
//! reason and fall back to the dsync service's DSYX.

/// A failed [`super::Follower::start`].
#[derive(Debug)]
#[non_exhaustive]
pub enum FollowerError {
    /// Windows: the Npcap runtime (`wpcap.dll`) is not installed or not loadable.
    NpcapMissing { detail: String },
    /// The PTP receive path was refused: on Linux, binding 319/320 needs `CAP_NET_BIND_SERVICE`
    /// or root.
    PermissionDenied { cause: anyhow::Error },
    /// The interface does not exist, or has no IPv4 now.
    InterfaceNotFound {
        interface: String,
        cause: anyhow::Error,
    },
    /// Anything else (the cause chain says what).
    Other(anyhow::Error),
}

impl FollowerError {
    /// Sort a failure of the receive path's open into its kind.
    pub(crate) fn from_open(cause: anyhow::Error) -> Self {
        #[cfg(windows)]
        if let Some(missing) = cause.downcast_ref::<crate::wpcap::NpcapMissing>() {
            return FollowerError::NpcapMissing {
                detail: missing.detail.clone(),
            };
        }
        if let Some(pin) = cause.downcast_ref::<crate::net::PinnedInterfaceUnavailable>() {
            return FollowerError::InterfaceNotFound {
                interface: pin.name.clone(),
                cause,
            };
        }
        let denied = cause.chain().any(|e| {
            e.downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::PermissionDenied)
        });
        if denied {
            FollowerError::PermissionDenied { cause }
        } else {
            FollowerError::Other(cause)
        }
    }
}

impl std::fmt::Display for FollowerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FollowerError::NpcapMissing { detail } => write!(
                f,
                "the Npcap runtime is missing ({detail}); install Npcap from https://npcap.com"
            ),
            FollowerError::PermissionDenied { cause } => write!(
                f,
                "the PTP receive path was refused ({cause:#}); on Linux binding 319/320 needs \
                 CAP_NET_BIND_SERVICE or root"
            ),
            FollowerError::InterfaceNotFound { interface, cause } => {
                write!(f, "PTP interface {interface:?} not usable: {cause:#}")
            }
            FollowerError::Other(cause) => write!(f, "{cause:#}"),
        }
    }
}

impl std::error::Error for FollowerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FollowerError::NpcapMissing { .. } => None,
            FollowerError::PermissionDenied { cause }
            | FollowerError::InterfaceNotFound { cause, .. }
            | FollowerError::Other(cause) => Some(cause.as_ref()),
        }
    }
}
