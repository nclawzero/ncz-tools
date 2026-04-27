//! Persistent ed25519 device identity for OpenClaw gateway handshakes.
//!
//! Openclaw's `connect` request requires a `device` object whose
//! `publicKey` is an ed25519 public key and whose `signature` is the
//! client's signature over the server-provided nonce (plus other
//! canonical fields — see openclaw protocol v3 §auth).
//!
//! The keypair is persistent: re-generating it on every launch would
//! show up to the gateway as a fresh device, triggering silent-pairing
//! (at best) or approval-required pairing (remote). Instead we cache
//! the private key to disk on first use and reload it on subsequent
//! launches, so a paired zterm install keeps its paired identity.
//!
//! **Storage format:** PKCS#8 PEM, file mode `0o600`, at
//! `$ZTERM_CONFIG_DIR/openclaw-device.pem` (default
//! `~/.zterm/openclaw-device.pem`). PKCS#8 is the standard serialization
//! for ed25519 private keys; `ed25519-dalek` reads/writes it natively
//! via the `pkcs8` feature.
//!
//! **Fingerprint:** openclaw's protocol calls for a stable `device.id`
//! string derived from the keypair. We use `sha256(pub_key_bytes)` and
//! hex-encode the first 16 bytes (32 chars) — short enough for logs,
//! unique enough for practical purposes.

use anyhow::{Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::pkcs8::{DecodePrivateKey, EncodePrivateKey};
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use std::fs;
use std::path::Path;

/// A loaded-or-generated persistent ed25519 device identity.
pub struct DeviceIdentity {
    signing_key: SigningKey,
    verifying_key: VerifyingKey,
    device_id: String,
}

impl std::fmt::Debug for DeviceIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Redact the private key — Debug output should never leak it into
        // logs or panics. Show the stable public fields only.
        f.debug_struct("DeviceIdentity")
            .field("device_id", &self.device_id)
            .field("public_key_b64url", &self.public_key_b64url())
            .field("signing_key", &"<redacted>")
            .finish()
    }
}

impl DeviceIdentity {
    /// Load the keypair at `path`. If the file does not exist, generate
    /// a new keypair, persist it (mode 0o600), and return it.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if path.exists() {
            return Self::load(path);
        }
        Self::create(path)
    }

    /// Load an existing keypair from a PKCS#8 PEM file.
    pub fn load(path: &Path) -> Result<Self> {
        let pem = fs::read_to_string(path)
            .with_context(|| format!("reading openclaw device key from {}", path.display()))?;
        let signing_key = SigningKey::from_pkcs8_pem(&pem)
            .with_context(|| format!("parsing PKCS#8 ed25519 key at {}", path.display()))?;
        Ok(Self::from_signing_key(signing_key))
    }

    /// Generate a new keypair and persist it to `path` with mode 0o600.
    ///
    /// Errors if `path` already exists — use `load_or_create` for the
    /// idempotent path.
    pub fn create(path: &Path) -> Result<Self> {
        if path.exists() {
            anyhow::bail!(
                "openclaw device key already exists at {}; refusing to overwrite",
                path.display()
            );
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating config dir {}", parent.display()))?;
        }
        let mut rng = rand::rngs::OsRng;
        let signing_key = SigningKey::generate(&mut rng);
        let pem = signing_key
            .to_pkcs8_pem(ed25519_dalek::pkcs8::spki::der::pem::LineEnding::LF)
            .context("encoding fresh ed25519 key as PKCS#8 PEM")?;
        fs::write(path, pem.as_bytes())
            .with_context(|| format!("writing openclaw device key to {}", path.display()))?;
        Self::tighten_permissions(path)?;
        Ok(Self::from_signing_key(signing_key))
    }

    #[cfg(unix)]
    fn tighten_permissions(path: &Path) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(path, perms)?;
        Ok(())
    }

    #[cfg(not(unix))]
    fn tighten_permissions(_path: &Path) -> Result<()> {
        // Windows has no unix permission bits; rely on per-user profile ACLs.
        Ok(())
    }

    fn from_signing_key(signing_key: SigningKey) -> Self {
        let verifying_key = signing_key.verifying_key();
        let device_id = Self::fingerprint(&verifying_key);
        Self {
            signing_key,
            verifying_key,
            device_id,
        }
    }

    fn fingerprint(verifying_key: &VerifyingKey) -> String {
        // openclaw canonical derivation: hex(sha256(raw_pubkey)).
        // Full 64-char hex — the server compares against its own
        // computation in deriveDeviceIdFromPublicKey.
        use sha2_stub::sha256_hex_full;
        sha256_hex_full(verifying_key.as_bytes())
    }

    /// Stable device fingerprint. 32 hex chars. Appears in openclaw
    /// pairing records and audit logs.
    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// Base64url-no-pad encoding of the raw 32-byte public key — the
    /// `publicKey` field in openclaw's handshake `device` object.
    pub fn public_key_b64url(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.verifying_key.as_bytes())
    }

    /// Sign arbitrary bytes. Callers assemble the canonical handshake
    /// payload per openclaw protocol v3 § auth (includes device.id,
    /// client.id, role, scopes, token, server nonce, signedAt) and
    /// pass those bytes here. Signature is hex-encoded per protocol.
    pub fn sign_hex(&self, payload: &[u8]) -> String {
        let sig = self.signing_key.sign(payload);
        hex_encode(&sig.to_bytes())
    }

    /// Sign arbitrary bytes; return a base64url-no-pad encoding of
    /// the 64-byte signature. This is the encoding openclaw wire
    /// protocol expects in the handshake signature field (see openclaw
    /// src/infra/device-identity.ts verifyDeviceSignature which calls
    /// base64UrlDecode on the signature string).
    pub fn sign_b64url(&self, payload: &[u8]) -> String {
        let sig = self.signing_key.sign(payload);
        URL_SAFE_NO_PAD.encode(sig.to_bytes())
    }
}

/// Minimal hex encoder. We avoid pulling in the `hex` crate for one
/// trivial function — keeps the dep graph lean.
fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

// SHA-256 is supplied by the `sha2` crate which `ed25519-dalek` already
// pulls in transitively; expose a thin stub module so this file does
// not need to know the dep name.
mod sha2_stub {
    use sha2::{Digest as _, Sha256};
    pub(super) fn sha256_hex_full(data: &[u8]) -> String {
        let digest = Sha256::digest(data);
        super::hex_encode(&digest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Verifier;
    use tempfile::TempDir;

    #[test]
    fn create_then_load_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("openclaw-device.pem");

        let ident1 = DeviceIdentity::create(&path).unwrap();
        let ident2 = DeviceIdentity::load(&path).unwrap();

        assert_eq!(ident1.device_id(), ident2.device_id());
        assert_eq!(ident1.public_key_b64url(), ident2.public_key_b64url());
    }

    #[test]
    fn load_or_create_creates_when_missing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nested").join("openclaw-device.pem");
        assert!(!path.exists());

        let ident = DeviceIdentity::load_or_create(&path).unwrap();
        assert!(path.exists());
        assert_eq!(ident.device_id().len(), 64);
    }

    #[test]
    fn load_or_create_reloads_when_present() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("openclaw-device.pem");

        let id1 = DeviceIdentity::load_or_create(&path).unwrap();
        let id2 = DeviceIdentity::load_or_create(&path).unwrap();

        // Same keypair both times — persistence is the whole point.
        assert_eq!(id1.device_id(), id2.device_id());
        assert_eq!(id1.public_key_b64url(), id2.public_key_b64url());
    }

    #[test]
    fn create_fails_if_path_exists() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("openclaw-device.pem");
        DeviceIdentity::create(&path).unwrap();

        let err = DeviceIdentity::create(&path).unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn signatures_verify_with_matching_public_key() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("openclaw-device.pem");
        let ident = DeviceIdentity::create(&path).unwrap();

        let payload = b"handshake-canonical-bytes-v3";
        let sig_hex = ident.sign_hex(payload);
        assert_eq!(sig_hex.len(), 128); // 64 bytes * 2 hex chars

        // Reconstruct the signature and verify against the public key.
        let sig_bytes = hex_decode(&sig_hex);
        assert_eq!(sig_bytes.len(), 64);
        let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes.try_into().unwrap());
        ident.verifying_key.verify(payload, &sig).unwrap();
    }

    #[test]
    fn device_id_is_stable_hex_of_fixed_length() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("openclaw-device.pem");
        let ident = DeviceIdentity::create(&path).unwrap();

        let id = ident.device_id();
        assert_eq!(id.len(), 64); // sha256 full digest, not truncated
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn public_key_b64url_is_43_chars_no_padding() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("openclaw-device.pem");
        let ident = DeviceIdentity::create(&path).unwrap();

        // 32 raw bytes -> ceil(32 * 4 / 3) = 43 chars, no `=` padding.
        let b64 = ident.public_key_b64url();
        assert_eq!(b64.len(), 43);
        assert!(!b64.contains('='));
    }

    #[cfg(unix)]
    #[test]
    fn persisted_key_is_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("openclaw-device.pem");
        DeviceIdentity::create(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    fn hex_decode(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }
}
