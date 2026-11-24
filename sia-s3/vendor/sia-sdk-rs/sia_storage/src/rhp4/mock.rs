use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use bytes::Bytes;
use chrono::Utc;
use sia_core::encoding;
use sia_core::rhp4::protocol::Error as ProtocolError;
use sia_core::rhp4::{AccountToken, HostPrices};
use sia_core::signing::{PrivateKey, PublicKey, Signature};
use sia_core::types::{Currency, Hash256};

use super::{Error as RHP4Error, HostEndpoint, Transport};
use crate::time::{Duration, Instant, sleep};

/// Where the mock hosts keep the sectors written to them.
///
/// `Memory` (the default) holds every sector in RAM, as the mock always has.
/// `Disk` keeps the same bytes in files under a directory this mock created
/// itself, so tests that write thousands of 4 MiB sectors (every one really
/// encrypted, erasure-coded and root-hashed by the SDK) can run in bounded
/// memory. Both answer reads identically: the same bytes, the same ranges,
/// the same "host not found" / "sector not found" errors, the same `clear`.
/// Test/mock support only - never a production host backend.
enum Sectors {
    Memory(HashMap<PublicKey, HashMap<Hash256, Bytes>>),
    #[cfg(not(target_arch = "wasm32"))]
    Disk(disk::DiskSectors),
}

impl Sectors {
    fn put(&mut self, host: PublicKey, root: Hash256, sector: Bytes) -> Result<(), RHP4Error> {
        match self {
            Sectors::Memory(m) => {
                m.entry(host).or_default().insert(root, sector);
                Ok(())
            }
            #[cfg(not(target_arch = "wasm32"))]
            Sectors::Disk(d) => d.put(host, root, &sector),
        }
    }

    fn get(&self, host: &PublicKey, root: &Hash256, range: Range<usize>) -> Result<Bytes, RHP4Error> {
        match self {
            Sectors::Memory(m) => {
                let host_sectors = m
                    .get(host)
                    .ok_or_else(|| RHP4Error::Transport("host not found".to_string()))?;
                let sector = host_sectors
                    .get(root)
                    .ok_or_else(|| RHP4Error::Transport("sector not found".to_string()))?;
                Ok(Bytes::copy_from_slice(&sector[range]))
            }
            #[cfg(not(target_arch = "wasm32"))]
            Sectors::Disk(d) => d.get(host, root, range),
        }
    }

    fn clear(&mut self) {
        match self {
            Sectors::Memory(m) => m.clear(),
            #[cfg(not(target_arch = "wasm32"))]
            Sectors::Disk(d) => d.clear(),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod disk {
    //! Sectors in files, for the mock only.
    //!
    //! Layout: `<dir>/<hex host key>/<hex sector root>.sector`, where `dir` is
    //! a directory THIS store created (fresh, unique) under a base the caller
    //! names. Every path is derived from two validated 32-byte binary values -
    //! the host's public key and the sector's root, which the transport itself
    //! computed from the bytes - so no caller-supplied string ever reaches the
    //! filesystem, and only files this store wrote are ever read or removed.
    //! Cleanup removes exactly those files, then the (by then empty) host
    //! directories and `dir`; it never removes anything recursively, so a
    //! stray file someone else put there is left alone and reported.
    use std::collections::{HashMap, HashSet};
    use std::fs::{self, File};
    use std::io::{self, Read, Seek, SeekFrom, Write};
    use std::ops::Range;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use bytes::Bytes;
    use sia_core::signing::PublicKey;
    use sia_core::types::Hash256;

    use super::RHP4Error;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    pub(crate) struct DiskSectors {
        dir: PathBuf,
        held: HashMap<PublicKey, HashMap<Hash256, u64>>,
        host_dirs: HashSet<PublicKey>,
        bytes: u64,
        limit: u64,
    }

    fn io_err(what: &str, e: io::Error) -> RHP4Error {
        RHP4Error::Transport(format!("mock sector store: {what}: {e}"))
    }

    impl DiskSectors {
        /// A fresh, unique directory under `base` (which must already exist
        /// and be a directory), holding at most `limit` bytes of sectors.
        pub(crate) fn create(base: &Path, limit: u64) -> io::Result<DiskSectors> {
            if !fs::metadata(base)?.is_dir() {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "the sector base is not a directory"));
            }
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            for _ in 0..16 {
                let n = NEXT.fetch_add(1, Ordering::Relaxed);
                let dir = base.join(format!("sia-mock-sectors-{}-{nanos}-{n}", std::process::id()));
                match fs::create_dir(&dir) {
                    Ok(()) => return Ok(DiskSectors { dir, held: HashMap::new(), host_dirs: HashSet::new(), bytes: 0, limit }),
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(e),
                }
            }
            Err(io::Error::new(io::ErrorKind::AlreadyExists, "no unique sector directory could be made"))
        }

        pub(crate) fn dir(&self) -> &Path {
            &self.dir
        }

        pub(crate) fn bytes(&self) -> u64 {
            self.bytes
        }

        fn host_dir(&self, host: &PublicKey) -> PathBuf {
            self.dir.join(hex::encode(<PublicKey as AsRef<[u8]>>::as_ref(host)))
        }

        fn path(&self, host: &PublicKey, root: &Hash256) -> PathBuf {
            let r: [u8; 32] = (*root).into();
            self.host_dir(host).join(format!("{}.sector", hex::encode(r)))
        }

        /// Store one sector. A sector is named by its own root, so a second
        /// write of the same root on the same host is the same bytes and
        /// changes nothing. Over the byte budget, or on any I/O failure, the
        /// write fails - it is never silently dropped.
        pub(crate) fn put(&mut self, host: PublicKey, root: Hash256, data: &[u8]) -> Result<(), RHP4Error> {
            if self.held.get(&host).is_some_and(|h| h.contains_key(&root)) {
                return Ok(());
            }
            let len = data.len() as u64;
            if self.bytes.saturating_add(len) > self.limit {
                return Err(RHP4Error::Transport(format!(
                    "mock sector store: over its {} byte budget",
                    self.limit
                )));
            }
            if !self.host_dirs.contains(&host) {
                fs::create_dir(self.host_dir(&host)).map_err(|e| io_err("host directory", e))?;
                self.host_dirs.insert(host);
            }
            let path = self.path(&host, &root);
            let part = path.with_extension("part");
            let written = (|| -> io::Result<()> {
                let mut f = File::create_new(&part)?;
                f.write_all(data)?;
                f.flush()?;
                drop(f);
                fs::rename(&part, &path)
            })();
            if let Err(e) = written {
                let _ = fs::remove_file(&part);
                return Err(io_err("write", e));
            }
            self.held.entry(host).or_default().insert(root, len);
            self.bytes += len;
            Ok(())
        }

        /// Read `range` of one sector, straight from its file. Errors match
        /// the in-memory store's; a range outside the sector is an error here
        /// rather than the in-memory store's slice panic.
        pub(crate) fn get(&self, host: &PublicKey, root: &Hash256, range: Range<usize>) -> Result<Bytes, RHP4Error> {
            let host_sectors = self
                .held
                .get(host)
                .ok_or_else(|| RHP4Error::Transport("host not found".to_string()))?;
            let len = *host_sectors
                .get(root)
                .ok_or_else(|| RHP4Error::Transport("sector not found".to_string()))?;
            if range.start > range.end || range.end as u64 > len {
                return Err(RHP4Error::Transport("range outside the sector".to_string()));
            }
            let mut f = File::open(self.path(host, root)).map_err(|e| io_err("open", e))?;
            f.seek(SeekFrom::Start(range.start as u64)).map_err(|e| io_err("seek", e))?;
            let mut buf = vec![0u8; range.end - range.start];
            f.read_exact(&mut buf).map_err(|e| io_err("read", e))?;
            Ok(Bytes::from(buf))
        }

        /// Forget and delete every sector, as hosts losing their data.
        pub(crate) fn clear(&mut self) {
            for (host, sectors) in std::mem::take(&mut self.held) {
                for root in sectors.keys() {
                    let p = self.path(&host, root);
                    if let Err(e) = fs::remove_file(&p) {
                        eprintln!("mock sector store: could not remove {}: {e}", p.display());
                    }
                }
            }
            self.bytes = 0;
        }
    }

    impl Drop for DiskSectors {
        fn drop(&mut self) {
            self.clear();
            for host in std::mem::take(&mut self.host_dirs) {
                let d = self.host_dir(&host);
                if let Err(e) = fs::remove_dir(&d) {
                    eprintln!("mock sector store: could not remove {}: {e}", d.display());
                }
            }
            if let Err(e) = fs::remove_dir(&self.dir) {
                eprintln!("mock sector store: could not remove {}: {e}", self.dir.display());
            }
        }
    }
}

#[derive(Clone)]
pub struct Client {
    sectors: Arc<RwLock<Sectors>>,
    slow_hosts: Arc<RwLock<HashSet<PublicKey>>>,
    slow_delay: Arc<RwLock<Duration>>,
    /// Per-sector read delay. After each read, the delay is halved. Used to
    /// simulate out-of-order chunk completion.
    read_delays: Arc<RwLock<HashMap<Hash256, Duration>>>,
    initial_read_delay: Arc<RwLock<Option<Duration>>>,
    read_failures: Arc<RwLock<HashMap<PublicKey, usize>>>,
    price_delay: Arc<RwLock<Duration>>,
    price_requests: Arc<AtomicUsize>,
    price_failures: Arc<AtomicUsize>,
    read_requests: Arc<AtomicUsize>,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    pub fn new() -> Self {
        Self {
            sectors: Arc::new(RwLock::new(Sectors::Memory(HashMap::new()))),
            slow_hosts: Arc::new(RwLock::new(HashSet::new())),
            slow_delay: Arc::new(RwLock::new(Duration::ZERO)),
            read_delays: Arc::new(RwLock::new(HashMap::new())),
            initial_read_delay: Arc::new(RwLock::new(None)),
            read_failures: Arc::new(RwLock::new(HashMap::new())),
            price_delay: Arc::new(RwLock::new(Duration::ZERO)),
            price_requests: Arc::new(AtomicUsize::new(0)),
            price_failures: Arc::new(AtomicUsize::new(0)),
            read_requests: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// As [`Client::new`], but the hosts keep their sectors in files under a
    /// fresh directory created inside `base` (see [`Sectors`]), holding at most
    /// `limit` bytes. The directory and every file in it are removed when the
    /// last clone of this client is dropped.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn with_disk_sectors(base: &std::path::Path, limit: u64) -> std::io::Result<Self> {
        let c = Self::new();
        *c.sectors.write().unwrap() = Sectors::Disk(disk::DiskSectors::create(base, limit)?);
        Ok(c)
    }

    /// The directory a disk-backed client keeps its sectors in.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn sector_dir(&self) -> Option<std::path::PathBuf> {
        match &*self.sectors.read().unwrap() {
            Sectors::Disk(d) => Some(d.dir().to_path_buf()),
            Sectors::Memory(_) => None,
        }
    }

    /// Bytes of sectors a disk-backed client currently holds.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn sector_bytes(&self) -> Option<u64> {
        match &*self.sectors.read().unwrap() {
            Sectors::Disk(d) => Some(d.bytes()),
            Sectors::Memory(_) => None,
        }
    }

    pub fn clear(&self) {
        self.sectors.write().unwrap().clear();
    }

    /// Sets the given hosts as "slow" - they will sleep for the specified duration
    /// before completing any write_sector or read_sector operation.
    pub fn set_slow_hosts(&self, hosts: impl IntoIterator<Item = PublicKey>, delay: Duration) {
        let mut slow = self.slow_hosts.write().unwrap();
        slow.clear();
        slow.extend(hosts);
        *self.slow_delay.write().unwrap() = delay;
    }

    /// Clears all slow host settings.
    pub fn reset_slow_hosts(&self) {
        self.slow_hosts.write().unwrap().clear();
        *self.slow_delay.write().unwrap() = Duration::ZERO;
    }

    /// Sleeps out the host's configured slow delay. A delay longer than
    /// `idle_timeout` is a stall, which the real transports report once
    /// the idle limit passes. The error takes the shape a stalled response
    /// has after passing through the protocol decoder.
    async fn stall(&self, host: &PublicKey, idle_timeout: Duration) -> Result<(), RHP4Error> {
        let delay = {
            let slow_hosts = self.slow_hosts.read().unwrap();
            if slow_hosts.contains(host) {
                Some(*self.slow_delay.read().unwrap())
            } else {
                None
            }
        };
        let Some(delay) = delay else {
            return Ok(());
        };
        if delay > idle_timeout {
            sleep(idle_timeout).await;
            let io = std::io::Error::new(std::io::ErrorKind::TimedOut, "stream idle");
            return Err(ProtocolError::from(encoding::Error::from(io)).into());
        }
        sleep(delay).await;
        Ok(())
    }
}

#[cfg(test)]
impl Client {
    /// Fails the next `count` `read_sector` calls to each of the given hosts,
    /// after which reads are served normally.
    pub fn set_read_failures(&self, hosts: impl IntoIterator<Item = PublicKey>, count: usize) {
        let mut failures = self.read_failures.write().unwrap();
        failures.clear();
        failures.extend(hosts.into_iter().map(|host| (host, count)));
    }

    /// Sets an initial per-sector read delay. After each read, the per-sector
    /// delay is halved. Used to simulate out-of-order chunk completion.
    /// Sectors written after this is set will start with `delay`.
    pub fn set_initial_read_delay(&self, delay: Duration) {
        *self.initial_read_delay.write().unwrap() = Some(delay);
    }

    /// Delays every `host_prices` call by `delay`.
    pub fn set_price_delay(&self, delay: Duration) {
        *self.price_delay.write().unwrap() = delay;
    }

    /// Number of `host_prices` calls served so far.
    pub fn price_requests(&self) -> usize {
        self.price_requests.load(Ordering::Relaxed)
    }

    /// Fails the next `count` `host_prices` calls, after which prices are
    /// served normally.
    pub fn set_price_failures(&self, count: usize) {
        self.price_failures.store(count, Ordering::Relaxed);
    }

    /// Number of `read_sector` calls started so far.
    pub fn read_requests(&self) -> usize {
        self.read_requests.load(Ordering::Relaxed)
    }
}

impl Transport for Client {
    async fn host_prices(
        &self,
        _: &HostEndpoint,
        _: Duration,
    ) -> Result<(HostPrices, Duration), RHP4Error> {
        let start = Instant::now();
        self.price_requests.fetch_add(1, Ordering::Relaxed);
        let delay = *self.price_delay.read().unwrap();
        if !delay.is_zero() {
            sleep(delay).await;
        }
        if self
            .price_failures
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(RHP4Error::Transport("price fetch failed".to_string()));
        }
        let prices = HostPrices {
            contract_price: Currency::zero(),
            collateral: Currency::zero(),
            ingress_price: Currency::zero(),
            egress_price: Currency::zero(),
            storage_price: Currency::zero(),
            free_sector_price: Currency::zero(),
            tip_height: 1,
            signature: Signature::default(),
            valid_until: Utc::now() + chrono::Duration::days(1),
        };
        Ok((prices, start.elapsed()))
    }

    async fn write_sector(
        &self,
        host: &HostEndpoint,
        _: HostPrices,
        _: &PrivateKey,
        sector: Bytes,
        idle_timeout: Duration,
    ) -> Result<(Hash256, Duration), RHP4Error> {
        if host.addresses.is_empty() {
            return Err(RHP4Error::Transport("host has no addresses".to_string()));
        }
        let start = Instant::now();
        self.stall(&host.public_key, idle_timeout).await?;

        sleep(Duration::from_millis(3)).await; // simulate network latency ~ 10Gbps
        let sector_root = sia_core::rhp4::sector_root(&sector);
        self.sectors.write().unwrap().put(host.public_key, sector_root, sector)?;
        if let Some(delay) = *self.initial_read_delay.read().unwrap() {
            self.read_delays.write().unwrap().insert(sector_root, delay);
        }
        Ok((sector_root, start.elapsed()))
    }

    async fn read_sector(
        &self,
        host: &HostEndpoint,
        _: HostPrices,
        _: AccountToken,
        root: Hash256,
        range: Range<usize>,
        idle_timeout: Duration,
    ) -> Result<(Bytes, Duration), RHP4Error> {
        self.read_requests.fetch_add(1, Ordering::Relaxed);
        if host.addresses.is_empty() {
            return Err(RHP4Error::Transport("host has no addresses".to_string()));
        }
        let start = Instant::now();
        self.stall(&host.public_key, idle_timeout).await?;

        let fail = {
            let mut failures = self.read_failures.write().unwrap();
            match failures.get_mut(&host.public_key) {
                Some(remaining) if *remaining > 0 => {
                    *remaining -= 1;
                    true
                }
                _ => false,
            }
        };
        if fail {
            return Err(RHP4Error::Transport("injected read failure".to_string()));
        }

        // per-sector decreasing delay (used to simulate out-of-order chunk completion)
        let read_delay = {
            let mut delays = self.read_delays.write().unwrap();
            delays.get(&root).copied().inspect(|&delay| {
                delays.insert(root, delay / 2);
            })
        };
        if let Some(delay) = read_delay {
            sleep(delay).await;
        }

        let sector = self.sectors.read().unwrap().get(&host.public_key, &root, range)?;
        sleep(Duration::from_nanos(sector.len() as u64 * 8 / 10)).await; // simulate network latency ~ 10Gbps
        Ok((sector, start.elapsed()))
    }
}
