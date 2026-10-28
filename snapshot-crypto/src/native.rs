//! Private native checkpoint transport, independent of the S3 provider.
//!
//! This is not a checkpoint device driver or a resume authorization service.
//! The privileged guest helper must supply a consistent Linux memory image and
//! writable-storage image, and obtain a fresh exclusive restore admission. It
//! must never use a value read from the checkpoint as the expected binding.
use crate::Cipher;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

const MAGIC: &[u8; 8] = b"KVMSNAP1";
const HEADER: usize = 8 + 4 * 32 + 8 * 5;
const MAX_COMPONENT: u64 = 128 * 1024 * 1024 * 1024;

/// Fixed app scope prevents accidental cross-app envelope acceptance.
pub struct NativeCipher(Cipher);
impl NativeCipher {
    pub fn new(master: [u8; 32], owner: &str, deployment: &str) -> io::Result<Self> {
        if master.iter().all(|b| *b == 0)
            || [owner, deployment]
                .iter()
                .any(|s| s.is_empty() || s.len() > 256 || s.chars().any(char::is_control))
        {
            return Err(invalid());
        }
        Ok(Self(Cipher::new(
            master,
            "kvm-box-native",
            owner,
            deployment,
        )))
    }
}

/// Trusted admission/configuration, not caller-supplied HTTP snapshot metadata.
/// `epoch` must be checked/consumed by an external exclusive admission service;
/// equality here does not itself prevent cloning or rollback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    pub kernel_sha256: [u8; 32],
    pub platform_sha256: [u8; 32],
    pub rootfs_sha256: [u8; 32],
    pub configuration_sha256: [u8; 32],
    pub memory_bytes: u64,
    pub vcpus: u64,
    pub epoch: u64,
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "native checkpoint binding or format failed",
    )
}

impl Binding {
    fn header(&self, memory_len: u64, disk_len: u64) -> io::Result<Vec<u8>> {
        if self.memory_bytes == 0
            || self.memory_bytes > MAX_COMPONENT
            || self.vcpus == 0
            || self.vcpus > 256
            || self.epoch == 0
            || memory_len == 0
            || memory_len > MAX_COMPONENT
            || disk_len > MAX_COMPONENT
        {
            return Err(invalid());
        }
        let mut bytes = Vec::with_capacity(HEADER);
        bytes.extend_from_slice(MAGIC);
        for digest in [
            &self.kernel_sha256,
            &self.platform_sha256,
            &self.rootfs_sha256,
            &self.configuration_sha256,
        ] {
            if digest.iter().all(|b| *b == 0) {
                return Err(invalid());
            }
            bytes.extend_from_slice(digest);
        }
        for n in [
            self.memory_bytes,
            self.vcpus,
            self.epoch,
            memory_len,
            disk_len,
        ] {
            bytes.extend_from_slice(&n.to_be_bytes());
        }
        Ok(bytes)
    }
}

/// Reader that rejects both a short input and trailing bytes. A failed seal
/// never emits the authenticated end marker, so a partial upload cannot open.
struct Exact<R> {
    input: R,
    remaining: u64,
    checked_end: bool,
}
impl<R: Read> Read for Exact<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            if !self.checked_end {
                let mut end = [0];
                if self.input.read(&mut end)? != 0 {
                    return Err(invalid());
                }
                self.checked_end = true;
            }
            return Ok(0);
        }
        let count = buf
            .len()
            .min(self.remaining.min(usize::MAX as u64) as usize);
        let n = self.input.read(&mut buf[..count])?;
        if n == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        self.remaining -= n as u64;
        Ok(n)
    }
}

/// Encrypt a consistent pair of streams inside the guest trust boundary.
/// A diskless guest uses an empty disk stream; applications with writable
/// storage must include that storage's consistent checkpoint, not an empty stub.
/// Construct the cipher from authenticated owner/deployment configuration.
/// Only publish `output` after success and durable object upload.
pub fn seal(
    cipher: &NativeCipher,
    bucket: &str,
    object: &str,
    binding: &Binding,
    memory: impl Read,
    memory_len: u64,
    disk: impl Read,
    disk_len: u64,
    output: impl Write,
) -> io::Result<()> {
    let header = binding.header(memory_len, disk_len)?;
    let memory = Exact {
        input: memory,
        remaining: memory_len,
        checked_end: false,
    };
    let disk = Exact {
        input: disk,
        remaining: disk_len,
        checked_end: false,
    };
    cipher.0.seal(
        bucket,
        object,
        header.as_slice().chain(memory).chain(disk),
        output,
    )
}

/// Holds fully authenticated state. There is deliberately no raw staging-file
/// accessor and no restore ioctl here. Callers must finish fresh admission and
/// device/transport recovery preparation before copying into restore devices.
pub struct Verified {
    staging: File,
    memory_len: u64,
    disk_len: u64,
}
impl Verified {
    pub fn memory_len(&self) -> u64 {
        self.memory_len
    }
    pub fn disk_len(&self) -> u64 {
        self.disk_len
    }
    pub fn copy_memory_to(&mut self, output: impl Write) -> io::Result<u64> {
        self.copy(HEADER as u64, self.memory_len, output)
    }
    pub fn copy_disk_to(&mut self, output: impl Write) -> io::Result<u64> {
        self.copy(HEADER as u64 + self.memory_len, self.disk_len, output)
    }
    fn copy(&mut self, offset: u64, len: u64, mut output: impl Write) -> io::Result<u64> {
        self.staging.seek(SeekFrom::Start(offset))?;
        let count = io::copy(&mut (&mut self.staging).take(len), &mut output)?;
        if count != len {
            return Err(invalid());
        }
        Ok(count)
    }
}
impl Drop for Verified {
    fn drop(&mut self) {
        let _ = self.staging.set_len(0);
    }
}

/// Authenticate the COMPLETE envelope into a private, unlinked staging file,
/// then validate all compatibility/admission fields before exposing any bytes.
/// The caller must create this file inside the guest, retain no other handles
/// or mappings, and ensure its backing store is private (tmpfs or encrypted).
/// Mode/unlink checks cannot establish trust in the backing store. A failure
/// truncates staging and never returns a `Verified` value. No VM device may be
/// passed as staging. `max_plaintext` is a trusted resource bound, not metadata.
#[cfg(unix)]
pub fn open_verified(
    cipher: &NativeCipher,
    bucket: &str,
    object: &str,
    expected: &Binding,
    input: impl Read,
    mut staging: File,
    max_plaintext: u64,
) -> io::Result<Verified> {
    let meta = staging.metadata()?;
    if !meta.is_file() || meta.nlink() != 0 || meta.mode() & 0o777 != 0o600 || meta.len() != 0 {
        return Err(invalid());
    }
    let result = (|| {
        if max_plaintext < HEADER as u64 || max_plaintext > HEADER as u64 + 2 * MAX_COMPONENT {
            return Err(invalid());
        }
        staging.seek(SeekFrom::Start(0))?;
        cipher
            .0
            .open(bucket, object, input, &mut staging, max_plaintext)?;
        let total = staging.stream_position()?;
        staging.seek(SeekFrom::Start(0))?;
        let mut header = [0; HEADER];
        staging.read_exact(&mut header)?;
        let memory_len = u64::from_be_bytes(header[HEADER - 16..HEADER - 8].try_into().unwrap());
        let disk_len = u64::from_be_bytes(header[HEADER - 8..].try_into().unwrap());
        if header.as_slice() != expected.header(memory_len, disk_len)?.as_slice()
            || total != HEADER as u64 + memory_len + disk_len
        {
            return Err(invalid());
        }
        Ok((memory_len, disk_len))
    })();
    match result {
        Ok((memory_len, disk_len)) => Ok(Verified {
            staging,
            memory_len,
            disk_len,
        }),
        Err(error) => {
            staging.set_len(0)?;
            Err(error)
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs::{self, OpenOptions};
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    fn staging() -> File {
        let path = std::env::temp_dir().join(format!(
            "native-snapshot-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        fs::remove_file(path).unwrap();
        f
    }
    fn binding() -> Binding {
        Binding {
            kernel_sha256: [1; 32],
            platform_sha256: [2; 32],
            rootfs_sha256: [3; 32],
            configuration_sha256: [4; 32],
            memory_bytes: 2048 * 1024 * 1024,
            vcpus: 1,
            epoch: 7,
        }
    }
    fn cipher() -> NativeCipher {
        NativeCipher::new([42; 32], "owner", "deployment").unwrap()
    }
    fn sealed() -> Vec<u8> {
        let mut out = vec![];
        seal(
            &cipher(),
            "b",
            "o",
            &binding(),
            &b"CPU RAM state"[..],
            13,
            &b"disk changes"[..],
            12,
            &mut out,
        )
        .unwrap();
        out
    }
    #[test]
    fn authenticated_components_roundtrip_and_drop_truncates() {
        let stage = staging();
        let observer = stage.try_clone().unwrap(); // Tests only: forbidden for production callers.
        let mut restored =
            open_verified(&cipher(), "b", "o", &binding(), &sealed()[..], stage, 4096).unwrap();
        let mut memory = vec![];
        let mut disk = vec![];
        assert_eq!(restored.copy_memory_to(&mut memory).unwrap(), 13);
        assert_eq!(restored.copy_disk_to(&mut disk).unwrap(), 12);
        assert!(memory == b"CPU RAM state" && disk == b"disk changes");
        drop(restored);
        assert_eq!(observer.metadata().unwrap().len(), 0);
    }
    #[test]
    fn late_corruption_never_exposes_restore_and_clears_staging() {
        let mut bytes = sealed();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        let stage = staging();
        let observer = stage.try_clone().unwrap();
        assert!(open_verified(&cipher(), "b", "o", &binding(), &bytes[..], stage, 4096).is_err());
        assert_eq!(observer.metadata().unwrap().len(), 0);
    }
    #[test]
    fn every_trusted_binding_is_checked() {
        for field in 0..7 {
            let mut b = binding();
            match field {
                0 => b.kernel_sha256[0] ^= 1,
                1 => b.platform_sha256[0] ^= 1,
                2 => b.rootfs_sha256[0] ^= 1,
                3 => b.configuration_sha256[0] ^= 1,
                4 => b.memory_bytes += 1,
                5 => b.vcpus += 1,
                _ => b.epoch += 1,
            }
            assert!(
                open_verified(&cipher(), "b", "o", &b, &sealed()[..], staging(), 4096).is_err()
            );
        }
    }
    #[test]
    fn rejects_short_or_extra_component_streams() {
        for (memory_len, disk_len) in [(12, 12), (14, 12), (13, 11), (13, 13)] {
            let mut out = vec![];
            assert!(seal(
                &cipher(),
                "b",
                "o",
                &binding(),
                &b"CPU RAM state"[..],
                memory_len,
                &b"disk changes"[..],
                disk_len,
                &mut out
            )
            .is_err());
            assert!(
                open_verified(&cipher(), "b", "o", &binding(), &out[..], staging(), 4096).is_err()
            );
        }
    }
    #[test]
    fn scope_bounds_and_plaintext_are_rejected() {
        for c in [
            NativeCipher::new([42; 32], "other", "deployment").unwrap(),
            NativeCipher::new([42; 32], "owner", "other").unwrap(),
        ] {
            assert!(
                open_verified(&c, "b", "o", &binding(), &sealed()[..], staging(), 4096).is_err()
            );
        }
        assert!(open_verified(
            &cipher(),
            "b",
            "o",
            &binding(),
            &sealed()[..],
            staging(),
            HEADER as u64
        )
        .is_err());
        assert!(open_verified(
            &cipher(),
            "b",
            "o",
            &binding(),
            &b"plaintext"[..],
            staging(),
            4096
        )
        .is_err());
        let other = Cipher::new([42; 32], "risc-box", "owner", "deployment")
            .seal_bytes("b", "o", &binding().header(1, 0).unwrap())
            .unwrap();
        assert!(
            open_verified(&cipher(), "b", "o", &binding(), &other[..], staging(), 4096).is_err()
        );
    }
    #[test]
    fn validates_lengths_inside_authenticated_envelope() {
        let mut payload = binding().header(4, 0).unwrap();
        payload.extend_from_slice(b"five!");
        let envelope = cipher().0.seal_bytes("b", "o", &payload).unwrap();
        assert!(open_verified(
            &cipher(),
            "b",
            "o",
            &binding(),
            &envelope[..],
            staging(),
            4096
        )
        .is_err());
    }
}
