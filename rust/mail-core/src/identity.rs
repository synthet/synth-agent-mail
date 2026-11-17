//! Host identity: an Ed25519 signing key and an X25519 encryption key, stored as PEM text in
//! `identity.json` (`src/peer/identity.ts`).
//!
//! The fingerprint hashes the stored PEM strings, so they are kept exactly as read and never
//! re-encoded. Keys this crate creates use the same RFC 8410 DER and PEM layout Node exports.

use std::fs;
use std::path::Path;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use pkcs8::der::asn1::ObjectIdentifier;
use pkcs8::spki::SubjectPublicKeyInfoRef;
use pkcs8::PrivateKeyInfo;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey as XPublicKey, StaticSecret};

use crate::error::{invalid, Error, Result};
use crate::js::utf16_len;

const ED25519: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.101.112");
const X25519: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.101.110");
// RFC 8410 DER prefixes, followed by the 32 key bytes. Node exports exactly these encodings.
const ED25519_SPKI: &str = "302a300506032b6570032100";
const ED25519_PKCS8: &str = "302e020100300506032b657004220420";
const X25519_SPKI: &str = "302a300506032b656e032100";
const X25519_PKCS8: &str = "302e020100300506032b656e04220420";

/// The public half, as sent in adverts and packets. Field order matches `publicIdentity()`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicIdentity {
    pub name: String,
    pub signing_key: String,
    pub encryption_key: String,
}

/// The `identity.json` file, private keys included.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub name: String,
    pub signing_key: String,
    pub encryption_key: String,
    pub signing_private: String,
    pub encryption_private: String,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl Identity {
    /// Builds an identity from raw 32-byte seeds. Used by tests and by `generate`.
    pub fn from_seeds(name: &str, signing_seed: [u8; 32], encryption_seed: [u8; 32]) -> Self {
        let signing = SigningKey::from_bytes(&signing_seed);
        let encryption = StaticSecret::from(encryption_seed);
        Identity {
            name: name.to_owned(),
            signing_key: pem(
                "PUBLIC KEY",
                ED25519_SPKI,
                signing.verifying_key().as_bytes(),
            ),
            encryption_key: pem(
                "PUBLIC KEY",
                X25519_SPKI,
                XPublicKey::from(&encryption).as_bytes(),
            ),
            signing_private: pem("PRIVATE KEY", ED25519_PKCS8, &signing_seed),
            encryption_private: pem("PRIVATE KEY", X25519_PKCS8, &encryption_seed),
        }
    }

    /// A new identity with random keys (`initIdentity`).
    pub fn generate(name: &str) -> Result<Self> {
        if crate::js::trim(name).is_empty() || utf16_len(name) > 80 {
            return Err(invalid("name", "must contain 1-80 characters"));
        }
        let mut signing_seed = [0u8; 32];
        let mut encryption_seed = [0u8; 32];
        OsRng.fill_bytes(&mut signing_seed);
        OsRng.fill_bytes(&mut encryption_seed);
        Ok(Self::from_seeds(name, signing_seed, encryption_seed))
    }

    /// Reads `<dir>/identity.json` and checks that each private key matches its public key, the
    /// same check `loadIdentity()` makes.
    pub fn load(dir: &Path) -> Result<Self> {
        let identity: Identity =
            serde_json::from_str(&fs::read_to_string(dir.join("identity.json"))?)?;
        identity.check()?;
        Ok(identity)
    }

    /// Validates the public half and that each private key derives its stored public PEM.
    pub fn check(&self) -> Result<()> {
        validate_public(&self.public())?;
        let signing = self.signing_key()?;
        let encryption = self.encryption_secret()?;
        if pem(
            "PUBLIC KEY",
            ED25519_SPKI,
            signing.verifying_key().as_bytes(),
        ) != self.signing_key
            || pem(
                "PUBLIC KEY",
                X25519_SPKI,
                XPublicKey::from(&encryption).as_bytes(),
            ) != self.encryption_key
        {
            return Err(Error::KeyMismatch);
        }
        Ok(())
    }

    pub fn public(&self) -> PublicIdentity {
        PublicIdentity {
            name: self.name.clone(),
            signing_key: self.signing_key.clone(),
            encryption_key: self.encryption_key.clone(),
        }
    }

    pub fn fingerprint(&self) -> Result<String> {
        fingerprint(&self.public())
    }

    /// `signed()`: base64 Ed25519 signature over the payload bytes.
    pub fn sign(&self, payload: &[u8]) -> Result<String> {
        Ok(BASE64.encode(self.signing_key()?.sign(payload).to_bytes()))
    }

    pub(crate) fn signing_key(&self) -> Result<SigningKey> {
        Ok(SigningKey::from_bytes(&private_key(
            &self.signing_private,
            ED25519,
        )?))
    }

    pub(crate) fn encryption_secret(&self) -> Result<StaticSecret> {
        Ok(StaticSecret::from(private_key(
            &self.encryption_private,
            X25519,
        )?))
    }
}

impl PublicIdentity {
    /// `verified()`: false for any malformed key or signature, never an error.
    pub fn verify(&self, payload: &[u8], signature: &str) -> bool {
        let Ok(key) = public_key(&self.signing_key, ED25519) else {
            return false;
        };
        let Ok(key) = VerifyingKey::from_bytes(&key) else {
            return false;
        };
        let Ok(bytes) = BASE64.decode(signature) else {
            return false;
        };
        let Ok(signature) = ed25519_dalek::Signature::from_slice(&bytes) else {
            return false;
        };
        key.verify(payload, &signature).is_ok()
    }

    pub(crate) fn encryption_public(&self) -> Result<XPublicKey> {
        Ok(XPublicKey::from(public_key(&self.encryption_key, X25519)?))
    }
}

/// `validatePublic()`: the strict schema plus key types.
pub fn validate_public(identity: &PublicIdentity) -> Result<()> {
    let name_len = utf16_len(&identity.name);
    if name_len == 0 || name_len > 80 {
        return Err(invalid("identity.name", "must contain 1-80 characters"));
    }
    if utf16_len(&identity.signing_key) > 300 || utf16_len(&identity.encryption_key) > 300 {
        return Err(invalid("identity", "key longer than 300 characters"));
    }
    public_key(&identity.signing_key, ED25519)?;
    public_key(&identity.encryption_key, X25519)?;
    Ok(())
}

/// `fingerprint()`: lowercase hex sha256 of `signing_key + "\n" + encryption_key`, as stored.
pub fn fingerprint(identity: &PublicIdentity) -> Result<String> {
    validate_public(identity)?;
    let mut hash = Sha256::new();
    hash.update(identity.signing_key.as_bytes());
    hash.update(b"\n");
    hash.update(identity.encryption_key.as_bytes());
    Ok(hex::encode(hash.finalize()))
}

/// An X25519 public key as the SPKI PEM Node's `createPublicKey` reads (used for ephemeral keys).
pub(crate) fn x25519_public_pem(key: &XPublicKey) -> String {
    pem("PUBLIC KEY", X25519_SPKI, key.as_bytes())
}

pub(crate) fn x25519_public_from_pem(text: &str) -> Result<XPublicKey> {
    Ok(XPublicKey::from(public_key(text, X25519)?))
}

/// Node's PEM layout: 64-column base64 lines, LF endings, trailing newline.
fn pem(label: &str, prefix_hex: &str, key: &[u8]) -> String {
    let mut der = hex::decode(prefix_hex).expect("constant prefix");
    der.extend_from_slice(key);
    let body = BASE64.encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for line in body.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(line).expect("base64 is ascii"));
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

fn decode_pem(text: &str, label: &str) -> Result<Vec<u8>> {
    let (found, der) =
        pem_rfc7468::decode_vec(text.as_bytes()).map_err(|e| Error::Key(e.to_string()))?;
    if found != label {
        return Err(Error::Key(format!("expected {label}, found {found}")));
    }
    Ok(der)
}

fn public_key(text: &str, oid: ObjectIdentifier) -> Result<[u8; 32]> {
    let der = decode_pem(text, "PUBLIC KEY")?;
    let info =
        SubjectPublicKeyInfoRef::try_from(der.as_slice()).map_err(|e| Error::Key(e.to_string()))?;
    if info.algorithm.oid != oid || info.algorithm.parameters.is_some() {
        return Err(Error::Key(format!("expected a {oid} public key")));
    }
    let bytes = info
        .subject_public_key
        .as_bytes()
        .ok_or_else(|| Error::Key("unaligned key bits".into()))?;
    bytes
        .try_into()
        .map_err(|_| Error::Key("public key is not 32 bytes".into()))
}

fn private_key(text: &str, oid: ObjectIdentifier) -> Result<[u8; 32]> {
    let der = decode_pem(text, "PRIVATE KEY")?;
    let info = PrivateKeyInfo::try_from(der.as_slice()).map_err(|e| Error::Key(e.to_string()))?;
    if info.algorithm.oid != oid || info.algorithm.parameters.is_some() {
        return Err(Error::Key(format!("expected a {oid} private key")));
    }
    // RFC 8410 CurvePrivateKey: an OCTET STRING (04 20) wrapping the 32 key bytes.
    match info.private_key {
        [0x04, 0x20, key @ ..] if key.len() == 32 => Ok(key.try_into().expect("checked length")),
        _ => Err(Error::Key(
            "private key is not a 32-byte CurvePrivateKey".into(),
        )),
    }
}
