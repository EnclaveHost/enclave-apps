//! Offline migration. No S3 access, no overwrite, no secret command arguments.
use rand_core::{OsRng, RngCore};
use snapshot_crypto::policy::Policy;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read},
    path::PathBuf,
};
use zeroize::{Zeroize, Zeroizing};

struct Staging(PathBuf);
impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 7 || !matches!(args[1].as_str(), "seal" | "open") {
        return Err(
            "usage: snapshot-crypto seal|open APP BUCKET OBJECT INPUT OUTPUT < config.json".into(),
        );
    }
    let mut raw = Zeroizing::new(String::new());
    io::stdin().take(65537).read_to_string(&mut raw)?;
    if raw.len() > 65536 {
        return Err("configuration too large".into());
    }
    let mut config: serde_json::Value = serde_json::from_str(&raw)?;
    if let Some(s) = config.pointer_mut("/snapshotEncryption/masterKey") {
        if let Some(reference) = s.as_str().and_then(|s| s.strip_prefix('$')) {
            let name = reference
                .strip_prefix('{')
                .and_then(|s| s.strip_suffix('}'))
                .unwrap_or(reference);
            *s = std::env::var(name)
                .map_err(|_| "snapshot secret environment variable is missing")?
                .into();
        }
    }
    // Local possession of the master secret authorizes this offline operation.
    // This does not authenticate a wallet or expose a network endpoint.
    let policy = Policy::from_config(&config, &args[2], true);
    if let Some(serde_json::Value::String(secret)) =
        config.pointer_mut("/snapshotEncryption/masterKey")
    {
        secret.zeroize();
    }
    let policy = policy?;
    let cipher = policy.private_cipher(&args[4])?;
    let input = File::open(&args[5])?;
    if !input.metadata()?.is_file() {
        return Err("input must be a regular file".into());
    }
    let destination = PathBuf::from(&args[6]);
    if destination.symlink_metadata().is_ok() {
        return Err("output already exists; choose a new path".into());
    }
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    let mut random = [0u8; 16];
    OsRng
        .try_fill_bytes(&mut random)
        .map_err(|_| "snapshot randomness unavailable")?;
    let suffix: String = random.iter().map(|b| format!("{b:02x}")).collect();
    let temporary = Staging(parent.join(format!(".snapshot-{suffix}.tmp")));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    // Non-Unix ACLs are not implemented: do not create plaintext staging there.
    #[cfg(not(unix))]
    return Err("offline migration requires Unix private file permissions".into());
    let mut output = options.open(&temporary.0)?;
    if args[1] == "seal" {
        cipher.seal(&args[3], &args[4], input, &mut output)?;
    } else {
        cipher.open(
            &args[3],
            &args[4],
            input,
            &mut output,
            128 * 1024 * 1024 * 1024,
        )?;
    }
    output.sync_all()?;
    drop(output);
    // Atomic, no-clobber publication, only after authentication of the entire
    // stream. On failure Staging removes the partial private file.
    fs::hard_link(&temporary.0, &destination)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("snapshot-crypto: {e}");
        std::process::exit(1);
    }
}
