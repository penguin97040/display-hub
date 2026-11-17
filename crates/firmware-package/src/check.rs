//! Checking a whole package as the hub, the app and the signing tool do ("Checking a package"):
//! read as a stream, in the document's order, holding no more than the header and manifest.

use std::io::{ErrorKind, Read};
use std::path::Path;

use ring::digest::{Context, SHA256};

use crate::header::{verify_signature, Header, HeaderError};
use crate::hex;
use crate::key::{KeyId, PublicKey};
use crate::limits::*;
use crate::manifest::{Manifest, ManifestError, UsbInstall};

/// The size of the buffer the image and parts pass through while they are hashed.
const STREAM_CHUNK: usize = 16 * 1024;

/// The SHA-256 of a whole package file. Only a name for the file, never a security check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PackageId(pub [u8; SHA256_LEN]);

impl PackageId {
    /// 64 lower-case hex digits.
    pub fn to_hex(&self) -> String {
        hex::encode(&self.0)
    }

    pub fn from_hex(s: &str) -> Option<Self> {
        hex::decode_exact::<SHA256_LEN>(s).map(Self)
    }
}

/// A package that passed every check: well formed and consistent with the key inside it. That says
/// nothing about whether anyone should trust that key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckedPackage {
    pub header: Header,
    pub manifest: Manifest,
    /// The manifest exactly as signed.
    pub manifest_bytes: Vec<u8>,
    pub package_id: PackageId,
    /// The whole file's length.
    pub len: u64,
}

impl CheckedPackage {
    pub fn signer(&self) -> PublicKey {
        self.header.public_key
    }

    pub fn signer_key_id(&self) -> KeyId {
        self.header.public_key.key_id()
    }

    /// The length of the update form: header, manifest and image, without the USB-install parts.
    pub fn update_form_len(&self) -> u64 {
        update_form_len(self.header.manifest_len, self.manifest.image_size)
    }
}

/// `137 + manifest_len + image_size`: the first bytes of a package, which are all a display needs.
pub fn update_form_len(manifest_len: u16, image_size: u64) -> u64 {
    HEADER_LEN as u64 + u64::from(manifest_len) + image_size
}

/// The update form of a package being read from `package`, which must be positioned at its start:
/// a reader that ends after the image. Check the package first; this only counts bytes.
pub fn update_form<R: Read>(package: R, checked: &CheckedPackage) -> std::io::Take<R> {
    package.take(checked.update_form_len())
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LayoutError {
    #[error("the {0} doesn't fit inside usb_install.min_flash_bytes")]
    OutsideFlash(String),
    #[error("the {0} and the {1} overlap in flash")]
    Overlap(String, String),
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PackageError {
    #[error("the package is {0} bytes; a package is at least {MIN_PACKAGE_LEN} bytes")]
    TooShort(u64),
    #[error("the package is {0} bytes, over the limit of {MAX_PACKAGE_LEN}")]
    TooLarge(u64),
    #[error("the package's header is wrong: {0}")]
    Header(HeaderError),
    #[error("the signer's public key in the header isn't a valid P-256 key")]
    BadPublicKey,
    #[error("the signature doesn't verify with the key in the header")]
    BadSignature,
    #[error("{0}")]
    Manifest(ManifestError),
    #[error("the USB-install layout is wrong: {0}")]
    Layout(LayoutError),
    #[error("the package is {actual} bytes, but its header and manifest add up to {expected}")]
    LengthMismatch { expected: u64, actual: u64 },
    #[error("the package ended early")]
    Truncated,
    #[error("the package has bytes after its last part")]
    TrailingBytes,
    #[error("the app image doesn't match the image_sha256 in the manifest")]
    ImageHash,
    #[error("the USB-install part {0:?} doesn't match its sha256 in the manifest")]
    PartHash(String),
    #[error("couldn't read the package: {0}")]
    Io(String),
}

/// Checks a package of `len` bytes (its file size, or a request's `Content-Length`) read from
/// `reader`, in the document's order:
///
/// 1. `len` is from 139 bytes to 16 MiB;
/// 2. the header, and a signer's key that is a valid P-256 point;
/// 3. the signature, with the key in the header;
/// 4. the manifest and every rule for it;
/// 5. the USB-install regions, inside `min_flash_bytes` and not overlapping;
/// 6. `len` is exactly header + manifest + image + parts;
/// 7. the hashes of the image and each part, as they pass.
///
/// The reader must then yield exactly `len` bytes: fewer is [`PackageError::Truncated`], more is
/// [`PackageError::TrailingBytes`].
pub fn check<R: Read>(mut reader: R, len: u64) -> Result<CheckedPackage, PackageError> {
    // 1.
    if len < MIN_PACKAGE_LEN {
        return Err(PackageError::TooShort(len));
    }
    if len > MAX_PACKAGE_LEN {
        return Err(PackageError::TooLarge(len));
    }
    let mut whole = Context::new(&SHA256);

    // 2.
    let mut head = [0u8; HEADER_LEN];
    read_exact(&mut reader, &mut head)?;
    whole.update(&head);
    let header = Header::parse(&head).map_err(PackageError::Header)?;
    if !header.public_key.is_valid_point() {
        return Err(PackageError::BadPublicKey);
    }

    // 3. The manifest is verified as bytes before anything parses it. A manifest that runs past the
    // stated length is never read beyond it.
    if (HEADER_LEN + header.manifest_len as usize) as u64 > len {
        return Err(PackageError::Truncated);
    }
    let mut manifest_bytes = vec![0u8; header.manifest_len as usize];
    read_exact(&mut reader, &mut manifest_bytes)?;
    whole.update(&manifest_bytes);
    if !verify_signature(&header.public_key, &header, &manifest_bytes) {
        return Err(PackageError::BadSignature);
    }

    // 4.
    let manifest = Manifest::parse(&manifest_bytes).map_err(PackageError::Manifest)?;

    // 5.
    if let Some(usb) = &manifest.usb_install {
        check_layout(usb, manifest.image_size).map_err(PackageError::Layout)?;
    }

    // 6.
    let expected = update_form_len(header.manifest_len, manifest.image_size) + manifest.parts_len();
    if expected != len {
        return Err(PackageError::LengthMismatch {
            expected,
            actual: len,
        });
    }

    // 7.
    if hash_next(&mut reader, manifest.image_size, &mut whole)? != manifest.image_sha256 {
        return Err(PackageError::ImageHash);
    }
    for part in manifest.usb_install.iter().flat_map(|u| &u.parts) {
        if hash_next(&mut reader, part.size, &mut whole)? != part.sha256 {
            return Err(PackageError::PartHash(part.name.clone()));
        }
    }
    let mut extra = [0u8; 1];
    if read_some(&mut reader, &mut extra)? != 0 {
        return Err(PackageError::TrailingBytes);
    }

    Ok(CheckedPackage {
        header,
        manifest,
        manifest_bytes,
        package_id: PackageId(whole.finish().as_ref().try_into().expect("SHA-256 is 32 bytes")),
        len,
    })
}

/// [`check`] on a file, taking its length from the file system. A file that changes length while it
/// is read fails as truncated or as having trailing bytes.
pub fn check_file(path: &Path) -> Result<CheckedPackage, PackageError> {
    let file = std::fs::File::open(path).map_err(|e| PackageError::Io(e.to_string()))?;
    let meta = file.metadata().map_err(|e| PackageError::Io(e.to_string()))?;
    if !meta.is_file() {
        return Err(PackageError::Io(format!("{} isn't a file", path.display())));
    }
    check(std::io::BufReader::new(file), meta.len())
}

/// The app image and each part are regions of flash. Each must end inside `min_flash_bytes`, and no
/// two may overlap.
pub fn check_layout(usb: &UsbInstall, image_size: u64) -> Result<(), LayoutError> {
    let mut regions: Vec<(u64, u64, String)> = Vec::with_capacity(usb.parts.len() + 1);
    regions.push((usb.app_offset, image_size, "app image".to_string()));
    for p in &usb.parts {
        regions.push((p.offset, p.size, format!("part {:?}", p.name)));
    }
    for (start, size, name) in &regions {
        match start.checked_add(*size) {
            Some(end) if end <= usb.min_flash_bytes => {}
            _ => return Err(LayoutError::OutsideFlash(name.clone())),
        }
    }
    regions.sort_by_key(|r| r.0);
    for pair in regions.windows(2) {
        // Both ends were checked above, so this can't overflow.
        if pair[0].0 + pair[0].1 > pair[1].0 {
            return Err(LayoutError::Overlap(pair[0].2.clone(), pair[1].2.clone()));
        }
    }
    Ok(())
}

fn read_some<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<usize, PackageError> {
    loop {
        match r.read(buf) {
            Ok(n) => return Ok(n),
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(PackageError::Io(e.to_string())),
        }
    }
}

fn read_exact<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<(), PackageError> {
    let mut filled = 0;
    while filled < buf.len() {
        match read_some(r, &mut buf[filled..])? {
            0 => return Err(PackageError::Truncated),
            n => filled += n,
        }
    }
    Ok(())
}

/// Reads exactly `n` bytes, hashing them on their own and into `whole`, and returns their SHA-256.
fn hash_next<R: Read>(r: &mut R, mut n: u64, whole: &mut Context) -> Result<[u8; SHA256_LEN], PackageError> {
    let mut own = Context::new(&SHA256);
    let mut buf = [0u8; STREAM_CHUNK];
    while n > 0 {
        let want = n.min(STREAM_CHUNK as u64) as usize;
        let got = read_some(r, &mut buf[..want])?;
        if got == 0 {
            return Err(PackageError::Truncated);
        }
        own.update(&buf[..got]);
        whole.update(&buf[..got]);
        n -= got as u64;
    }
    Ok(own.finish().as_ref().try_into().expect("SHA-256 is 32 bytes"))
}
