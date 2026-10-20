//! Local e2e fixture signer. The fixed PUBLIC test key must never hold assets.
use k256::ecdsa::SigningKey;
use sha2::{Digest, Sha256};
use sha3::Keccak256;
fn hex(b: &[u8]) -> String { b.iter().map(|b| format!("{b:02x}")).collect() }
fn main() {
    let a: Vec<String> = std::env::args().collect();
    assert_eq!(a.len(),4,"file route service-deployment-id");
    let body=std::fs::read(&a[1]).unwrap();
    let expiry=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()+300;
    let key=SigningKey::from_bytes(&[0x42u8;32].into()).unwrap();
    let address=format!("0x{}",hex(&Keccak256::digest(&key.verifying_key().to_encoded_point(false).as_bytes()[1..])[12..]));
    let hash=hex(&Sha256::digest(&body));
    let message=format!("enclave-storage-upload:v1\nchainId:8453\nservice:{}\nmethod:POST\npath:{}\naddress:{address}\nsha256:{hash}\nbytes:{}\nexpiry:{expiry}",a[3],a[2],body.len());
    let mut h=Keccak256::new();h.update(format!("\x19Ethereum Signed Message:\n{}",message.len()));h.update(message.as_bytes());
    let (sig,rec)=key.sign_prehash_recoverable(&h.finalize()).unwrap();
    println!("x-upload-address: {address}\nx-upload-expiry: {expiry}\nx-upload-sha256: {hash}\nx-upload-signature: 0x{}{:02x}",hex(&sig.to_bytes()),rec.to_byte()+27);
}
