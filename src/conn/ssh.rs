//! SSH tunnels: reaching a profile's server, and its cluster nodes and replicas, through
//! a bastion with the system `ssh`.
//!
//! Owner: connections.
//!
//! Each endpoint gets its own forward, started the first time a connection needs it:
//! `ssh -N -L 127.0.0.1:<free port>:<host>:<port>` with `BatchMode=yes`,
//! `StrictHostKeyChecking=yes` and `ExitOnForwardFailure=yes`, plus `-p`, `-i`, `-l`
//! and `UserKnownHostsFile` from the profile's [`SshTunnel`] and keychain. ssh is run
//! directly, never through a shell. Its standard error is read only to pick a fixed
//! [`SshError`] and is never shown, logged or kept. Forwards are killed when the
//! [`Bastion`] is dropped.

use crate::model::{Endpoint, Profile, SshTunnel};
use crate::secrets::SecretStore;

use super::{ConnError, Resolved, Unusable};

/// Why an SSH tunnel could not be used. Fixed texts: never ssh's own output, the SSH
/// username, or anything from the keychain.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SshError {
    /// Placeholder until SSH tunnels are built.
    #[error("SSH tunnels are not supported yet")]
    NotImplemented,
}

/// A profile's bastion and the forwards opened through it so far.
pub struct Bastion {
    _private: (),
}

impl Bastion {
    /// Prepare the bastion for `profile`: read its SSH username from the keychain
    /// ([`Profile::ssh_secret_ref`]; no entry means none). Starts no process.
    pub fn new(
        _profile: &Profile,
        _tunnel: &SshTunnel,
        _secrets: &dyn SecretStore,
    ) -> Result<Self, ConnError> {
        Err(SshError::NotImplemented.into())
    }

    /// The local end of the forward to `ep`, started if there is none yet, as the
    /// address to pin connections to. `local` is judged from the bastion's side: `ep`'s
    /// host is loopback by name (see `is_loopback_name`), since its addresses are
    /// resolved on the bastion, not here.
    pub(super) async fn reach(&self, _ep: &Endpoint) -> Result<Resolved, Unusable> {
        Err(Unusable::Conn(SshError::NotImplemented.into()))
    }
}
