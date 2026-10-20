use std::sync::Arc;

use sia_core::signing::{PrivateKey, PublicKey};
use sia_core::types::v2::{NetAddress, Protocol};

use crate::rhp4::{Client, mock};
use crate::time::Duration;
use crate::{AppKey, BuilderError, Host, Sdk, SharedSdk, SharingKey, app_client};

/// A fault injected into one indexer call (see [`MockNetwork::push_fault`]).
#[derive(Debug, Clone, Copy)]
pub enum Fault {
    /// The indexer applies the call, but the response is lost: the caller sees an error.
    ApplyThenFail,
    /// The call never answers and never takes effect.
    Hang,
    /// The call never answers, but the indexer applies it after this delay, as a
    /// request a client gave up on can still be carried out.
    ApplyLate(std::time::Duration),
    /// The call never answers; the indexer applies it whenever the test calls
    /// [`MockNetwork::apply_deferred`] (after any number of listings, restarts
    /// or deadlines: no timing assumption).
    Defer,
}

/// A mock Sia network: a mock indexer paired with a mock host transport, whose
/// sectors live in memory (the default) or, with [`MockNetwork::with_disk_sectors`], on disk.
///
/// [`MockNetwork::sdk`] builds a real [`Sdk`] against them, so tests drive the
/// same public API an application uses rather than a parallel mock surface.
pub struct MockNetwork {
    api: app_client::mock::Client,
    transport: mock::Client,
}

impl MockNetwork {
    pub fn new() -> Self {
        Self {
            api: app_client::mock::Client::new(),
            transport: mock::Client::new(),
        }
    }

    /// As [`MockNetwork::new`], but the mock hosts keep their sectors in files
    /// under a fresh, uniquely named directory created inside `base`, instead
    /// of in RAM, and hold at most `limit` bytes. Everything else - the real
    /// encryption, erasure coding and sector roots, faults, slow hosts,
    /// `clear_sectors` - is unchanged. For tests that write more sectors than
    /// memory should hold; `base` should be on disk (not tmpfs). The directory
    /// and its files are removed when the network and every SDK built from it
    /// are dropped. Native only: there is no filesystem mock on wasm.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn with_disk_sectors(base: &std::path::Path, limit: u64) -> std::io::Result<Self> {
        Ok(Self {
            api: app_client::mock::Client::new(),
            transport: mock::Client::with_disk_sectors(base, limit)?,
        })
    }

    /// The directory a disk-backed network keeps its sectors in, if it is one.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn sector_dir(&self) -> Option<std::path::PathBuf> {
        self.transport.sector_dir()
    }

    /// Bytes of sectors a disk-backed network holds right now, if it is one.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn sector_bytes(&self) -> Option<u64> {
        self.transport.sector_bytes()
    }

    /// Adds `n` hosts that are usable for uploads and returns their keys.
    ///
    /// Hosts must be added before [`MockNetwork::sdk`]; the SDK reads the
    /// indexer's host list when it is created and then on a long interval.
    pub fn add_hosts(&self, n: usize) -> Vec<PublicKey> {
        let hosts: Vec<Host> = (0..n)
            .map(|_| Host {
                public_key: PrivateKey::from_seed(&rand::random::<[u8; 32]>()).public_key(),
                addresses: vec![NetAddress {
                    protocol: Protocol::QUIC,
                    address: "localhost:9984".to_string(),
                }],
                country_code: "US".to_string(),
                latitude: 0.0,
                longitude: 0.0,
                good_for_upload: true,
            })
            .collect();
        let keys = hosts.iter().map(|h| h.public_key).collect();
        self.api.add_hosts(hosts);
        keys
    }

    /// Sets the given hosts as "slow" - they will sleep for the specified
    /// duration before completing any write_sector or read_sector operation.
    pub fn set_slow_hosts(&self, hosts: impl IntoIterator<Item = PublicKey>, delay: Duration) {
        self.transport.set_slow_hosts(hosts, delay);
    }

    /// Clears all slow host settings.
    pub fn reset_slow_hosts(&self) {
        self.transport.reset_slow_hosts();
    }

    /// Discards every sector stored by the hosts. The indexer's objects and
    /// slabs are untouched, so anything pinned before this becomes
    /// undownloadable.
    pub fn clear_sectors(&self) {
        self.transport.clear();
    }

    /// Takes the mock indexer offline (or back online). Hosts keep their
    /// sectors and the indexer keeps its objects, as across a real outage.
    pub fn set_indexer_offline(&self, offline: bool) {
        self.api.set_offline(offline);
    }

    /// Queues `fault` for the next call of `op` on the mock indexer:
    /// `"pin_object"` (also object metadata updates), `"delete_object"`, or
    /// `"pin_slabs"` (the upload path; only [`Fault::Hang`] applies there).
    pub fn push_fault(&self, op: &'static str, fault: Fault) {
        self.api.push_fault(op, fault);
    }

    /// Applies every call held by [`Fault::Defer`], in order; returns how many.
    pub fn apply_deferred(&self) -> usize {
        self.api.apply_deferred()
    }

    /// The number of slabs currently pinned to the mock indexer.
    pub fn pinned_slabs(&self) -> usize {
        self.api.pinned_slabs()
    }

    /// Makes the next `failures` slab pin requests fail, so callers can
    /// exercise the retries the upload path performs around pinning.
    pub fn set_pin_slabs_failures(&self, failures: usize) {
        self.api.set_pin_slabs_failures(failures);
    }

    /// The number of slab pin requests the mock indexer has received,
    /// including the ones made to fail.
    pub fn pin_slabs_calls(&self) -> usize {
        self.api.pin_slabs_calls()
    }

    /// Builds an [`Sdk`] backed by this network.
    pub async fn sdk(&self, app_key: AppKey) -> Result<Sdk, BuilderError> {
        Sdk::with_backends(
            app_client::Client::Mock(self.api.clone()),
            Client::Mock(self.transport.clone()),
            Arc::new(app_key),
        )
        .await
    }

    /// Builds an indexer-free reader against only the mock host transport.
    pub fn recovery_reader(&self, key: AppKey, hosts: Vec<Host>) -> crate::RecoveryReader {
        crate::RecoveryReader::with_transport(key, hosts, Client::Mock(self.transport.clone()))
    }

    /// Builds a [`SharedSdk`] backed by this network, as a recipient holding
    /// `seed`. The key must already exist on the mock indexer.
    pub async fn shared_sdk(&self, seed: [u8; 32]) -> Result<SharedSdk, BuilderError> {
        SharedSdk::with_backends(
            app_client::Client::Mock(self.api.clone()),
            Client::Mock(self.transport.clone()),
            SharingKey::import(seed),
        )
        .await
    }
}

impl Default for MockNetwork {
    fn default() -> Self {
        Self::new()
    }
}
