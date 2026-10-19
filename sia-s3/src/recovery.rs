//! Explicit snapshot export and verified, read-only recovery. Recovery never
//! constructs an Sdk/indexer client, sweeps orphans, compacts, or updates metadata.
use crate::store::{Index, Key, Meta, Store};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sia_storage::{
    AppKey, Builder, DownloadOptions, Host, HostQuery, RecoveryReader, SealedObject,
};
use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};
use tokio::io::AsyncReadExt;
type Res<T> = Result<T, String>;
const MAX_SNAPSHOT: u64 = 64 * 1024 * 1024;
const MAX_OBJECTS: usize = 100_000;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    version: u32,
    created_at: String,
    hosts: Vec<Host>,
    objects: BTreeMap<String, SealedObject>,
    // References select the exact resolved S3 keys without exposing their names.
    keys: Vec<KeyRef>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyRef {
    object: String,
    entry: usize,
}
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
fn hash_arg(s: &str) -> Res<()> {
    if s.len() == 64
        && s.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        Ok(())
    } else {
        Err("expected a lowercase SHA-256".into())
    }
}
fn bounded(path: &Path, max: u64) -> Res<Vec<u8>> {
    let mut out = Vec::new();
    File::open(path)
        .map_err(err)?
        .take(max + 1)
        .read_to_end(&mut out)
        .map_err(err)?;
    if out.len() as u64 > max {
        return Err("input exceeds size limit".into());
    }
    Ok(out)
}
fn key_file() -> Res<AppKey> {
    let path =
        std::env::var("SIA_RECOVERY_KEY_FILE").map_err(|_| "SIA_RECOVERY_KEY_FILE is required")?;
    let raw = bounded(Path::new(&path), 128)?;
    let text = std::str::from_utf8(&raw)
        .map_err(|_| "invalid recovery key")?
        .trim();
    let bytes: [u8; 32] = hex::decode(text)
        .map_err(|_| "invalid recovery key")?
        .try_into()
        .map_err(|_| "invalid recovery key")?;
    Ok(AppKey::import(bytes))
}
struct Pending {
    path: PathBuf,
    file: File,
}
impl Pending {
    fn new(destination: &Path) -> Res<Self> {
        if destination.exists() {
            return Err("destination already exists".into());
        }
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce).map_err(err)?;
        let path = destination
            .parent()
            .unwrap_or(Path::new("."))
            .join(format!(".sia-recovery-{}", hex::encode(nonce)));
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let file = opts.open(&path).map_err(err)?;
        Ok(Self { path, file })
    }
    fn commit(mut self, destination: &Path) -> Res<()> {
        self.file.flush().map_err(err)?;
        self.file.sync_all().map_err(err)?;
        // Atomic create without overwriting a file/symlink that appeared mid-read.
        fs::hard_link(&self.path, destination).map_err(err)?;
        Ok(())
    }
}
impl Drop for Pending {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
fn open_snapshot(raw: &[u8], expected: &str, key: &AppKey) -> Res<(Index, Vec<Host>)> {
    hash_arg(expected)?;
    if raw.len() as u64 > MAX_SNAPSHOT || digest(raw) != expected {
        return Err("snapshot digest or size mismatch".into());
    }
    let snap: Snapshot = serde_json::from_slice(raw).map_err(err)?;
    if snap.version != 1
        || snap.objects.len() > MAX_OBJECTS
        || snap.keys.len() > MAX_OBJECTS
        || snap.hosts.len() > 10000
    {
        return Err("invalid snapshot limits/version".into());
    }
    let mut seen = HashSet::new();
    for host in &snap.hosts {
        if !seen.insert(host.public_key) {
            return Err("duplicate host identity".into());
        }
    }
    let mut idx = Index::default();
    for (id, sealed) in snap.objects {
        let obj = sealed.open(key).map_err(err)?;
        if obj.id().to_string() != id {
            return Err("object identity mismatch".into());
        }
        let meta: Meta = serde_json::from_slice(&obj.metadata).map_err(err)?;
        if meta.v != 1 {
            return Err("unknown S3 metadata version".into());
        }
        idx.objs.insert(id, (obj, meta));
    }
    for r in snap.keys {
        let (_, m) = idx.objs.get(&r.object).ok_or("missing key object")?;
        let entry = m.e.get(r.entry).ok_or("missing key entry")?.clone();
        let kk = (entry.b.clone(), entry.k.clone());
        if idx
            .keys
            .insert(
                kk,
                Key {
                    host: r.object,
                    entry,
                },
            )
            .is_some()
        {
            return Err("duplicate S3 key".into());
        }
    }
    for k in idx.keys.values() {
        let segs = idx.segments(k, 0, k.entry.s)?;
        let size = segs
            .iter()
            .try_fold(0u64, |n, (_, _, len)| n.checked_add(*len))
            .ok_or("segment size overflow")?;
        if size != k.entry.s {
            return Err("incomplete multipart key".into());
        }
    }
    Ok((idx, snap.hosts))
}
async fn export(indexer: &str, destination: &Path, key: AppKey) -> Res<()> {
    let sdk = Builder::new(indexer, crate::META)
        .map_err(err)?
        .connected(&key)
        .await
        .map_err(err)?
        .ok_or("app key not authorized")?;
    let store = Store::load(sdk.clone(), 0).await?; // read only; do not call boot/sweep
    let mut hosts = Vec::new();
    loop {
        let batch = sdk
            .hosts(HostQuery {
                offset: Some(hosts.len() as u64),
                limit: Some(500),
                ..Default::default()
            })
            .await
            .map_err(err)?;
        let done = batch.len() < 500;
        hosts.extend(batch);
        if hosts.len() > 10000 {
            return Err("too many hosts".into());
        }
        if done {
            break;
        }
    }
    let idx = store.idx.lock().await;
    if idx.objs.len() > MAX_OBJECTS || idx.keys.len() > MAX_OBJECTS {
        return Err("too many objects/keys".into());
    }
    let mut objects = BTreeMap::new();
    let mut keys = Vec::new();
    for k in idx.keys.values() {
        let (_, meta) = idx.objs.get(&k.host).ok_or("missing host object")?;
        let entry = meta
            .e
            .iter()
            .position(|e| e == &k.entry)
            .ok_or("missing key entry")?;
        keys.push(KeyRef {
            object: k.host.clone(),
            entry,
        });
        for id in idx.parts(k) {
            let (obj, _) = idx.objs.get(&id).ok_or("missing multipart object")?;
            objects.entry(id).or_insert_with(|| obj.seal(&key));
        }
    }
    let raw = serde_json::to_vec(&Snapshot {
        version: 1,
        created_at: chrono::Utc::now().to_rfc3339(),
        hosts,
        objects,
        keys,
    })
    .map_err(err)?;
    let sha = digest(&raw);
    let (verified, _) = open_snapshot(&raw, &sha, &key)?;
    let mut pending = Pending::new(destination)?;
    pending.file.write_all(&raw).map_err(err)?;
    pending.commit(destination)?;
    println!(
        "{}",
        serde_json::json!({"snapshotSha256":sha,"snapshotBytes":raw.len(),"objects":verified.objs.len(),"keys":verified.keys.len(),"readOnlyExport":true})
    );
    Ok(())
}
async fn recover(
    snapshot: &Path,
    sha: &str,
    bucket: &str,
    name: &str,
    destination: &Path,
    expected_sha: &str,
    expected_size: u64,
    key: AppKey,
) -> Res<()> {
    hash_arg(expected_sha)?;
    let raw = bounded(snapshot, MAX_SNAPSHOT)?;
    let (idx, hosts) = open_snapshot(&raw, sha, &key)?;
    let k = idx
        .keys
        .get(&(bucket.into(), name.into()))
        .ok_or("key absent from snapshot")?;
    if k.entry.s != expected_size {
        return Err("expected size differs from snapshot".into());
    }
    let segments = idx.segments(k, 0, expected_size)?;
    let reader = RecoveryReader::new(key, hosts);
    let mut pending = Pending::new(destination)?;
    let mut hash = Sha256::new();
    let mut total = 0u64;
    let mut buf = vec![0u8; 256 * 1024];
    for (obj, offset, length) in segments {
        let mut stream = reader
            .download(
                &obj,
                DownloadOptions {
                    offset,
                    length: Some(length),
                    max_buffered_chunks: Some(2),
                    ..Default::default()
                },
            )
            .map_err(err)?;
        let mut read = 0u64;
        loop {
            let n = stream.read(&mut buf).await.map_err(err)?;
            if n == 0 {
                break;
            }
            read = read.checked_add(n as u64).ok_or("size overflow")?;
            if read > length {
                return Err("oversize segment".into());
            }
            hash.update(&buf[..n]);
            pending.file.write_all(&buf[..n]).map_err(err)?;
        }
        if read != length {
            return Err("truncated segment".into());
        }
        total += read;
    }
    if total != expected_size || hex::encode(hash.finalize()) != expected_sha {
        return Err("recovered file digest or size mismatch".into());
    }
    pending.commit(destination)?;
    println!(
        "{}",
        serde_json::json!({"verified":true,"bytes":total,"sha256":expected_sha,"indexerUsed":false,"writesToSia":false})
    );
    Ok(())
}
pub async fn command(args: &[String]) -> Res<()> {
    let deadline = std::time::Duration::from_secs(600);
    match args {
        [op, indexer, out] if op == "export" => {
            tokio::time::timeout(deadline, export(indexer, Path::new(out), key_file()?))
                .await.map_err(|_| "snapshot export deadline exceeded")?
        }
        [op, snapshot, sha, bucket, name, out, expected_sha, size] if op == "read" => {
            let size = size.parse::<u64>().map_err(|_| "invalid expected size")?;
            tokio::time::timeout(deadline, recover(Path::new(snapshot), sha, bucket, name,
                Path::new(out), expected_sha, size, key_file()?))
                .await.map_err(|_| "recovery deadline exceeded")?
        }
        _ => Err("usage: SIA_RECOVERY_KEY_FILE=... sia-s3 recovery export INDEXER SNAPSHOT | recovery read SNAPSHOT SHA256 BUCKET KEY OUTPUT EXPECTED_SHA256 EXPECTED_BYTES".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Entry;
    use sia_storage::{Object, UploadOptions, mock::MockNetwork};
    fn fixture(key: &AppKey) -> Snapshot {
        let meta = Meta {
            v: 1,
            e: vec![Entry {
                b: "ipfs".into(),
                k: "empty".into(),
                s: 0,
                t: String::new(),
                m: 1,
                c: None,
                x: vec![],
            }],
            ..Default::default()
        };
        let obj = Object::new(Some(serde_json::to_vec(&meta).unwrap()));
        let id = obj.id().to_string();
        Snapshot {
            version: 1,
            created_at: "test".into(),
            hosts: vec![],
            objects: BTreeMap::from([(id.clone(), obj.seal(key))]),
            keys: vec![KeyRef {
                object: id,
                entry: 0,
            }],
        }
    }
    #[test]
    fn snapshot_requires_external_digest_and_correct_key_and_signatures() {
        let key = AppKey::import([7; 32]);
        let snap = fixture(&key);
        let raw = serde_json::to_vec(&snap).unwrap();
        assert!(open_snapshot(&raw, &digest(&raw), &key).is_ok());
        assert!(open_snapshot(&raw, &"00".repeat(32), &key).is_err());
        assert!(open_snapshot(&raw, &digest(&raw), &AppKey::import([8; 32])).is_err());
        let mut altered = snap;
        altered
            .objects
            .values_mut()
            .next()
            .unwrap()
            .encrypted_metadata[0] ^= 1;
        let raw = serde_json::to_vec(&altered).unwrap();
        assert!(open_snapshot(&raw, &digest(&raw), &key).is_err());
    }
    #[test]
    fn duplicate_keys_missing_parts_and_unknown_versions_fail_closed() {
        let key = AppKey::import([7; 32]);
        let mut snap = fixture(&key);
        snap.keys.push(KeyRef {
            object: snap.keys[0].object.clone(),
            entry: 0,
        });
        let raw = serde_json::to_vec(&snap).unwrap();
        assert!(open_snapshot(&raw, &digest(&raw), &key).is_err());
        let mut snap = fixture(&key);
        snap.version = 2;
        let raw = serde_json::to_vec(&snap).unwrap();
        assert!(open_snapshot(&raw, &digest(&raw), &key).is_err());
        let mut snap = fixture(&key);
        let id = snap.keys[0].object.clone();
        let mut obj = snap.objects.remove(&id).unwrap().open(&key).unwrap();
        let mut meta: Meta = serde_json::from_slice(&obj.metadata).unwrap();
        meta.e[0].s = 5;
        meta.e[0].x = vec!["missing".into()];
        obj.metadata = serde_json::to_vec(&meta).unwrap();
        snap.objects.insert(id, obj.seal(&key));
        let raw = serde_json::to_vec(&snap).unwrap();
        assert!(open_snapshot(&raw, &digest(&raw), &key).is_err());
    }
    #[test]
    fn failed_and_racing_output_never_replaces_existing_data() {
        let dir = std::env::temp_dir().join(format!(
            "sia-recovery-test-{}",
            hex::encode({
                let mut n = [0u8; 16];
                getrandom::fill(&mut n).unwrap();
                n
            })
        ));
        fs::create_dir(&dir).unwrap();
        let out = dir.join("result");
        {
            let mut pending = Pending::new(&out).unwrap();
            pending.file.write_all(b"partial").unwrap();
        }
        assert!(!out.exists());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        let mut pending = Pending::new(&out).unwrap();
        pending.file.write_all(b"verified").unwrap();
        fs::write(&out, b"other writer").unwrap();
        assert!(pending.commit(&out).is_err());
        assert_eq!(fs::read(&out).unwrap(), b"other writer");
        fs::remove_file(&out).unwrap();
        let mut pending = Pending::new(&out).unwrap();
        pending.file.write_all(b"verified").unwrap();
        pending.commit(&out).unwrap();
        assert_eq!(fs::read(&out).unwrap(), b"verified");
        fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn read_refuses_wrong_file_commitment_without_exposing_output() {
        let key=AppKey::import([7;32]);
        let mut nonce=[0u8;16];getrandom::fill(&mut nonce).unwrap();
        let dir=std::env::temp_dir().join(format!("sia-recovery-read-{}",hex::encode(nonce)));
        fs::create_dir(&dir).unwrap();let snapshot=dir.join("snapshot.json");let output=dir.join("result");
        let raw=serde_json::to_vec(&fixture(&key)).unwrap();fs::write(&snapshot,&raw).unwrap();
        assert!(recover(&snapshot,&digest(&raw),"ipfs","empty",&output,&"00".repeat(32),0,key.clone()).await.is_err());
        assert!(!output.exists());assert_eq!(fs::read_dir(&dir).unwrap().count(),1);
        assert!(recover(&snapshot,&digest(&raw),"ipfs","empty",&output,&digest(b""),1,key.clone()).await.is_err());
        recover(&snapshot,&digest(&raw),"ipfs","empty",&output,&digest(b""),0,key).await.unwrap();
        assert_eq!(fs::read(&output).unwrap(),b"");fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn real_erasure_coded_bytes_survive_discarding_the_indexer() {
        let net = MockNetwork::new();
        net.add_hosts(30);
        let key = AppKey::import([5; 32]);
        let sdk = net.sdk(key.clone()).await.unwrap();
        let bytes = vec![0xa7; 2 * 1024 * 1024 + 19];
        let object = sdk
            .upload(
                Object::default(),
                std::io::Cursor::new(bytes.clone()),
                UploadOptions {
                    data_shards: 10,
                    parity_shards: 20,
                    max_buffered_slabs: Some(1),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let hosts = sdk.hosts(HostQuery::default()).await.unwrap();
        let sealed = object.seal(&key);
        let reader = net.recovery_reader(key.clone(), hosts);
        drop(sdk);
        drop(net); // no indexer remains
        let restored = sealed.open(&key).unwrap();
        let mut stream = reader
            .download(
                &restored,
                DownloadOptions {
                    max_buffered_chunks: Some(2),
                    ..Default::default()
                },
            )
            .unwrap();
        let mut out = Vec::new();
        stream.read_to_end(&mut out).await.unwrap();
        assert_eq!(out, bytes);
        let mut ranged = reader
            .download(
                &restored,
                DownloadOptions {
                    offset: 123,
                    length: Some(713),
                    max_buffered_chunks: Some(1),
                    ..Default::default()
                },
            )
            .unwrap();
        out.clear();
        ranged.read_to_end(&mut out).await.unwrap();
        assert_eq!(out, &bytes[123..836]);
    }
}
