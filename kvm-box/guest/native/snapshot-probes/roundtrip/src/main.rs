//! PID1-only, synthetic two-boot qualification. NEVER ship in an app image.
//! Requires one new blank 1GiB virtual disk, no NIC, no real user data, and an
//! independent host watchdog. The fixed key is PUBLIC TEST MATERIAL.
//! Capture writes only encrypted state to that disk and powers off without
//! closing the snapshot device. A separate boot authenticates before restore.
//! This does not qualify secret custody, anti-clone admission, GPU or desktop.
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("this diagnostic requires x86_64 Linux");
use snapshot_crypto::native::{self, Binding, NativeCipher};
use std::{
    ffi::CStr,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::fd::{AsRawFd, IntoRawFd},
    os::unix::fs::{FileTypeExt, OpenOptionsExt},
    time::{Duration, Instant},
};

const FREEZE: libc::Ioctl = libc::_IO(b'3' as u32, 1);
const CREATE: libc::Ioctl = libc::_IOW::<libc::c_int>(b'3' as u32, 17);
const IMAGE_SIZE: libc::Ioctl = libc::_IOR::<libc::loff_t>(b'3' as u32, 14);
const PREF_SIZE: libc::Ioctl = libc::_IO(b'3' as u32, 18);
const RESTORE: libc::Ioctl = libc::_IO(b'3' as u32, 4);
const DISK_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_PLAIN: u64 = 512 * 1024 * 1024;
const MAX_ENCRYPTED: u64 = MAX_PLAIN + 1024 * 1024;
const OFFSET: u64 = 4096;
const DISK_MAGIC: &[u8; 8] = b"KVMTEST1";
const TEST_MARKER: &[u8] = b"synthetic-native-checkpoint-disk-state-v1";

fn fail(message: &str) -> io::Error {
    io::Error::other(message)
}
fn check(rc: libc::c_int) -> io::Result<()> {
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
fn mount(kind: &CStr, path: &CStr, options: Option<&CStr>) -> io::Result<()> {
    unsafe {
        check(libc::mount(
            kind.as_ptr(),
            path.as_ptr(),
            kind.as_ptr(),
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
            options.map_or(std::ptr::null(), |s| s.as_ptr().cast()),
        ))
    }
}
fn setup() -> io::Result<()> {
    for p in ["/dev", "/proc", "/sys", "/tmp"] {
        fs::create_dir_all(p)?;
    }
    // devtmpfs itself must allow the diagnostic character/block devices.
    unsafe {
        check(libc::mount(
            c"devtmpfs".as_ptr(),
            c"/dev".as_ptr(),
            c"devtmpfs".as_ptr(),
            libc::MS_NOSUID,
            std::ptr::null(),
        ))?;
    }
    let console = OpenOptions::new().write(true).open("/dev/console")?;
    unsafe {
        check(libc::dup2(console.as_raw_fd(), 1))?;
        check(libc::dup2(console.as_raw_fd(), 2))?;
    }
    mount(c"proc", c"/proc", None)?;
    mount(c"sysfs", c"/sys", None)?;
    mount(c"tmpfs", c"/tmp", Some(c"size=640m,mode=0700"))?;
    let limit = libc::rlimit {
        rlim_cur: libc::RLIM_INFINITY,
        rlim_max: libc::RLIM_INFINITY,
    };
    unsafe {
        check(libc::setrlimit(libc::RLIMIT_MEMLOCK, &limit))?;
        check(libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE))?;
    }
    Ok(())
}
fn cipher() -> NativeCipher {
    NativeCipher::new([0x52; 32], "synthetic-owner", "synthetic-probe-only").unwrap()
}
fn binding() -> Binding {
    // Diagnostic constants, not claims of measured production admission.
    Binding {
        kernel_sha256: [1; 32],
        platform_sha256: [2; 32],
        rootfs_sha256: [3; 32],
        configuration_sha256: [4; 32],
        memory_bytes: 2 * 1024 * 1024 * 1024,
        vcpus: 1,
        epoch: 1,
    }
}
fn disk() -> io::Result<File> {
    let deadline = Instant::now() + Duration::from_secs(8);
    while !std::path::Path::new("/dev/sda").exists() {
        if Instant::now() >= deadline {
            return Err(fail("diagnostic disk unavailable"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if std::path::Path::new("/dev/sdb").exists() {
        return Err(fail("unexpected second disk"));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open("/dev/sda")?;
    if !file.metadata()?.file_type().is_block_device() {
        return Err(fail("not a block device"));
    }
    let mut size = 0u64;
    unsafe {
        check(libc::ioctl(
            file.as_raw_fd(),
            libc::_IOR::<usize>(0x12, 114),
            &mut size,
        ))?;
    }
    if size != DISK_BYTES {
        return Err(fail("diagnostic disk size mismatch"));
    }
    Ok(file)
}
fn tpm_random() -> io::Result<()> {
    let mut tpm = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tpm0")?;
    tpm.write_all(&[0x80, 0x01, 0, 0, 0, 12, 0, 0, 1, 0x7b, 0, 16])?;
    let mut response = [0; 128];
    let n = tpm.read(&mut response)?;
    if n != 28
        || response[..2] != [0x80, 0x01]
        || response[2..6] != [0, 0, 0, 28]
        || response[6..10] != [0; 4]
        || response[10..12] != [0, 16]
    {
        return Err(fail("TPM GetRandom failed"));
    }
    Ok(())
}
fn poweroff() -> ! {
    let _ = io::stdout().flush();
    unsafe {
        libc::reboot(libc::RB_POWER_OFF);
    }
    loop {
        unsafe {
            libc::pause();
        }
    }
}
fn capture(mut disk: File) -> io::Result<()> {
    tpm_random()?;
    println!("ROUNDTRIP TPM_BEFORE OK");
    let mut sentinel = vec![0u8; 1024 * 1024];
    for (i, b) in sentinel.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    let snapshot = File::open("/dev/snapshot")?;
    unsafe {
        check(libc::ioctl(
            snapshot.as_raw_fd(),
            PREF_SIZE,
            128 * 1024 * 1024 as libc::c_ulong,
        ))?;
        check(libc::ioctl(snapshot.as_raw_fd(), FREEZE, 0))?;
    }
    let mut original: libc::c_int = -1;
    println!("ROUNDTRIP CREATE begin");
    unsafe {
        check(libc::ioctl(snapshot.as_raw_fd(), CREATE, &mut original))?;
    }
    println!("ROUNDTRIP CREATE OK original={original}");
    if original == 0 {
        // Successful ATOMIC_RESTORE continues HERE in the captured process.
        // Do not use restored storage/transport handles for further I/O.
        unsafe {
            check(libc::close(snapshot.into_raw_fd()))?;
        }
        println!("ROUNDTRIP RESUMED_CLOSE OK");
        for (i, b) in sentinel.iter().enumerate() {
            if unsafe { std::ptr::read_volatile(b) } != (i % 251) as u8 {
                return Err(fail("RAM sentinel mismatch"));
            }
        }
        println!("ROUNDTRIP RAM_CONTINUATION OK bytes={}", sentinel.len());
        tpm_random()?;
        println!("ROUNDTRIP TPM_AFTER OK");
        println!("ROUNDTRIP RESULT PASS synthetic-resume-only");
        poweroff();
    }
    if original != 1 {
        return Err(fail("unexpected snapshot branch"));
    }
    let mut size: libc::loff_t = 0;
    unsafe {
        check(libc::ioctl(snapshot.as_raw_fd(), IMAGE_SIZE, &mut size))?;
    }
    if size <= 0 || size as u64 > MAX_PLAIN - 4096 {
        return Err(fail("image exceeds bound"));
    }
    println!("ROUNDTRIP IMAGE_SIZE {size}");
    disk.seek(SeekFrom::Start(OFFSET))?;
    native::seal(
        &cipher(),
        "synthetic",
        "fresh-boot",
        &binding(),
        &snapshot,
        size as u64,
        TEST_MARKER,
        TEST_MARKER.len() as u64,
        &mut disk,
    )?;
    let size = disk.stream_position()? - OFFSET;
    if size > MAX_ENCRYPTED {
        return Err(fail("ciphertext exceeds bound"));
    }
    disk.sync_all()?;
    // Commit length only after complete authenticated ciphertext reaches disk.
    let mut header = [0u8; 4096];
    header[..8].copy_from_slice(DISK_MAGIC);
    header[8..16].copy_from_slice(&size.to_be_bytes());
    disk.seek(SeekFrom::Start(0))?;
    disk.write_all(&header)?;
    disk.sync_all()?;
    println!("ROUNDTRIP ENCRYPTED_SAVED bytes={size}");
    for b in &mut sentinel {
        unsafe {
            std::ptr::write_volatile(b, 0xff);
        }
    }
    println!("ROUNDTRIP POST_CAPTURE_RAM_CHANGED");
    // The kernel contract forbids closing/cancelling capture before resume.
    std::mem::forget(snapshot);
    println!("ROUNDTRIP CAPTURE_POWER_OFF");
    poweroff();
}
fn restore(mut disk: File, size: u64) -> io::Result<()> {
    if size == 0 || size > MAX_ENCRYPTED {
        return Err(fail("invalid ciphertext size"));
    }
    let stage = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open("/tmp/checkpoint")?;
    fs::remove_file("/tmp/checkpoint")?;
    disk.seek(SeekFrom::Start(OFFSET))?;
    let mut verified = native::open_verified(
        &cipher(),
        "synthetic",
        "fresh-boot",
        &binding(),
        disk.take(size),
        stage,
        MAX_PLAIN,
    )?;
    let mut marker = vec![];
    if verified.disk_len() != TEST_MARKER.len() as u64 {
        return Err(fail("synthetic disk marker size mismatch"));
    }
    verified.copy_disk_to(&mut marker)?;
    if marker != TEST_MARKER {
        return Err(fail("synthetic disk marker mismatch"));
    }
    println!(
        "ROUNDTRIP AUTHENTICATED memory={} disk={}",
        verified.memory_len(),
        verified.disk_len()
    );
    let mut snapshot = OpenOptions::new().write(true).open("/dev/snapshot")?;
    unsafe {
        check(libc::ioctl(snapshot.as_raw_fd(), FREEZE, 0))?;
    }
    verified.copy_memory_to(&mut snapshot)?;
    println!("ROUNDTRIP ATOMIC_RESTORE begin");
    unsafe {
        check(libc::ioctl(snapshot.as_raw_fd(), RESTORE, 0))?;
    }
    Err(fail("ATOMIC_RESTORE returned without continuation"))
}
fn run() -> io::Result<()> {
    setup()?;
    println!("ROUNDTRIP BEGIN encrypted-fresh-boot-v1 PUBLIC_TEST_KEY");
    let mut disk = disk()?;
    let mut header = [0u8; 4096];
    disk.read_exact(&mut header)?;
    if header.iter().all(|b| *b == 0) {
        capture(disk)
    } else if &header[..8] == DISK_MAGIC && header[16..].iter().all(|b| *b == 0) {
        restore(disk, u64::from_be_bytes(header[8..16].try_into().unwrap()))
    } else {
        Err(fail(
            "disk is neither blank nor a committed diagnostic checkpoint",
        ))
    }
}
fn main() {
    if unsafe { libc::getpid() } != 1 {
        eprintln!("PID1-only disposable probe");
        std::process::exit(64);
    }
    if let Err(e) = run() {
        println!("ROUNDTRIP RESULT FAIL {e}");
    }
    poweroff();
}
