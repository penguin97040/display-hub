//! The hub's TLS identity: an ECDSA P-256 key and a self-signed certificate.
//!
//! Displays trust exactly this certificate, which was installed over USB. A new identity would
//! lock every display out until it is set up again. So the identity is written atomically, backed
//! up with the database, and never silently replaced: if the database exists but the identity
//! doesn't, the hub refuses to start.
//!
//! - **P-256:** ESP-IDF's mbedTLS supports it well; Ed25519 server certificates it does not.
//! - **Fixed name** ([`HUB_SERVER_NAME`]): firmware can check the name whatever address it uses.
//! - **Validity from 1970 to 9999-12-31** (RFC 5280's "no expiry"): trust comes from the pinned
//!   certificate, not from dates, and a display without the time must not reject it.

use std::io::Write;
use std::path::{Path, PathBuf};

use hub_core::api::HubIdentity;
use hub_protocol::pairing::hex;
use hub_protocol::provisioning::HUB_SERVER_NAME;
use rcgen::{
    date_time_ymd, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, KeyPair,
    KeyUsagePurpose, PublicKeyData, SerialNumber,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use zeroize::Zeroizing;

const CERT_FILE: &str = "hub-cert.pem";
const KEY_FILE: &str = "hub-key.pem";

pub struct Identity {
    pub cert_der: CertificateDer<'static>,
    pub cert_pem: String,
    key_pkcs8: Zeroizing<Vec<u8>>,
    pub spki_sha256: [u8; 32],
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("spki_sha256", &hex(&self.spki_sha256))
            .finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error(
        "the hub's TLS identity is missing from {dir}, but its database already exists. Displays trust \
         the old identity, so a new one would lock them all out. Restore {CERT_FILE} and {KEY_FILE} \
         from a backup (see docs/deploy-raspberry-pi.md). Only if the old identity is truly lost, \
         remove the database too and set every display and admin up again."
    )]
    Missing { dir: PathBuf },
    #[error("only one of {CERT_FILE} and {KEY_FILE} exists in {dir}; restore both from the same backup")]
    Partial { dir: PathBuf },
    #[error("the hub's TLS identity in {dir} is damaged or doesn't match: {detail}")]
    Damaged { dir: PathBuf, detail: String },
    #[error("couldn't create the hub's TLS identity: {0}")]
    Generate(String),
    #[error("couldn't write the hub's TLS identity to {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
}

/// SHA-256 of a certificate's SubjectPublicKeyInfo.
pub fn spki_sha256(cert: &CertificateDer<'_>) -> Option<[u8; 32]> {
    let ee = webpki::EndEntityCert::try_from(cert).ok()?;
    let spki = ee.subject_public_key_info();
    ring::digest::digest(&ring::digest::SHA256, spki.as_ref())
        .as_ref()
        .try_into()
        .ok()
}

impl Identity {
    /// A fresh identity, held in memory.
    pub fn generate() -> Result<Self, IdentityError> {
        let gen = |e: rcgen::Error| IdentityError::Generate(e.to_string());
        let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).map_err(gen)?;
        let mut params = CertificateParams::new(vec![HUB_SERVER_NAME.to_string()]).map_err(gen)?;
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, HUB_SERVER_NAME);
        params.distinguished_name = dn;
        params.not_before = date_time_ymd(1970, 1, 1);
        params.not_after = date_time_ymd(9999, 12, 31);
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let mut serial = crate::secret::random_bytes::<16>();
        serial[0] &= 0x7f; // a positive serial number
        params.serial_number = Some(SerialNumber::from_slice(&serial));
        let cert = params.self_signed(&key).map_err(gen)?;
        Self::from_parts(
            cert.der().clone(),
            cert.pem(),
            Zeroizing::new(key.serialize_der()),
        )
        .map_err(IdentityError::Generate)
    }

    fn from_parts(
        cert_der: CertificateDer<'static>,
        cert_pem: String,
        key_pkcs8: Zeroizing<Vec<u8>>,
    ) -> Result<Self, String> {
        let spki = spki_sha256(&cert_der).ok_or("the certificate can't be parsed")?;
        let key =
            KeyPair::try_from(key_pkcs8.as_slice()).map_err(|e| format!("the key can't be read: {e}"))?;
        let key_spki = ring::digest::digest(&ring::digest::SHA256, &key.subject_public_key_info());
        if key_spki.as_ref() != spki {
            return Err("the key doesn't belong to the certificate".into());
        }
        Ok(Self {
            cert_der,
            cert_pem,
            key_pkcs8,
            spki_sha256: spki,
        })
    }

    /// Loads the identity from `dir`, creating it only when `may_create` (a brand-new hub).
    pub fn load_or_create(dir: &Path, may_create: bool) -> Result<Self, IdentityError> {
        let (cert_path, key_path) = (dir.join(CERT_FILE), dir.join(KEY_FILE));
        match (cert_path.exists(), key_path.exists()) {
            (true, true) => Self::load(dir),
            (false, false) if may_create => {
                let id = Self::generate()?;
                std::fs::create_dir_all(dir).map_err(|source| IdentityError::Io {
                    path: dir.into(),
                    source,
                })?;
                restrict_dir(dir);
                // The key first: a certificate without its key is useless, and the load check
                // reports a lone key clearly.
                let key_pem = Zeroizing::new(
                    KeyPair::try_from(id.key_pkcs8.as_slice())
                        .map_err(|e| IdentityError::Generate(e.to_string()))?
                        .serialize_pem(),
                );
                write_atomically(&key_path, key_pem.as_bytes(), 0o600)?;
                write_atomically(&cert_path, id.cert_pem.as_bytes(), 0o644)?;
                Ok(id)
            }
            (false, false) => Err(IdentityError::Missing { dir: dir.into() }),
            _ => Err(IdentityError::Partial { dir: dir.into() }),
        }
    }

    fn load(dir: &Path) -> Result<Self, IdentityError> {
        let damaged = |detail: String| IdentityError::Damaged {
            dir: dir.into(),
            detail,
        };
        let cert_pem = std::fs::read_to_string(dir.join(CERT_FILE)).map_err(|e| damaged(e.to_string()))?;
        let cert_der = CertificateDer::from_pem_slice(cert_pem.as_bytes())
            .map_err(|e| damaged(format!("{CERT_FILE}: {e}")))?;
        let key_bytes =
            Zeroizing::new(std::fs::read(dir.join(KEY_FILE)).map_err(|e| damaged(e.to_string()))?);
        let key = PrivatePkcs8KeyDer::from_pem_slice(&key_bytes)
            .map_err(|e| damaged(format!("{KEY_FILE}: {e}")))?;
        Self::from_parts(
            cert_der,
            cert_pem,
            Zeroizing::new(key.secret_pkcs8_der().to_vec()),
        )
        .map_err(damaged)
    }

    pub fn private_key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_pkcs8.to_vec()))
    }

    pub fn info(&self) -> HubIdentity {
        HubIdentity {
            certificate_pem: self.cert_pem.clone(),
            spki_sha256: hex(&self.spki_sha256),
            server_name: HUB_SERVER_NAME.into(),
        }
    }

    pub fn file_names() -> [&'static str; 2] {
        [CERT_FILE, KEY_FILE]
    }
}

#[cfg(unix)]
fn restrict_dir(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn restrict_dir(_dir: &Path) {}

/// Writes a file so a power cut leaves either the old file or the whole new one: a temporary file
/// with the final permissions, flushed to disk, renamed over the target, then the directory synced.
pub fn write_atomically(path: &Path, bytes: &[u8], mode: u32) -> Result<(), IdentityError> {
    let io = |source| IdentityError::Io {
        path: path.into(),
        source,
    };
    let tmp = path.with_extension("tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    {
        let mut f = opts.open(&tmp).map_err(io)?;
        f.write_all(bytes).map_err(io)?;
        f.sync_all().map_err(io)?;
    }
    std::fs::rename(&tmp, path).map_err(io)?;
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)
            .and_then(|d| d.sync_all())
            .map_err(io)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_once_then_reloads_the_same_identity() {
        let dir = tempfile::tempdir().unwrap();
        let a = Identity::load_or_create(dir.path(), true).unwrap();
        let b = Identity::load_or_create(dir.path(), false).unwrap();
        assert_eq!(a.spki_sha256, b.spki_sha256);
        assert_eq!(a.cert_der, b.cert_der);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join(KEY_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn never_silently_replaces_a_missing_identity() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            Identity::load_or_create(dir.path(), false),
            Err(IdentityError::Missing { .. })
        ));
        Identity::load_or_create(dir.path(), true).unwrap();
        std::fs::remove_file(dir.path().join(KEY_FILE)).unwrap();
        assert!(matches!(
            Identity::load_or_create(dir.path(), true),
            Err(IdentityError::Partial { .. })
        ));
    }

    #[test]
    fn a_key_from_elsewhere_is_refused() {
        let (one, two) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        Identity::load_or_create(one.path(), true).unwrap();
        Identity::load_or_create(two.path(), true).unwrap();
        std::fs::copy(two.path().join(KEY_FILE), one.path().join(KEY_FILE)).unwrap();
        assert!(matches!(
            Identity::load_or_create(one.path(), false),
            Err(IdentityError::Damaged { .. })
        ));
        std::fs::write(one.path().join(CERT_FILE), "not a certificate").unwrap();
        assert!(matches!(
            Identity::load_or_create(one.path(), false),
            Err(IdentityError::Damaged { .. })
        ));
    }

    #[test]
    fn the_certificate_suits_esp_idf() {
        let id = Identity::generate().unwrap();
        assert!(id.cert_pem.starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(id.cert_pem.len() < 1024, "small enough for a display's flash");
        let ee = webpki::EndEntityCert::try_from(&id.cert_der).unwrap();
        let name = rustls::pki_types::ServerName::try_from(HUB_SERVER_NAME).unwrap();
        ee.verify_is_valid_for_subject_name(&name).unwrap();
    }
}
