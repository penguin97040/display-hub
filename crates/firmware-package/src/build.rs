//! Making a package: what the signing tool's `sign` does.

use std::io::Cursor;

use crate::check::{check, CheckedPackage, LayoutError, PackageError};
use crate::header::{signed_bytes, signed_prefix, Header};
use crate::key::{sha256, PublicKey, SigningKey};
use crate::limits::*;
use crate::manifest::{Manifest, ManifestError, UsbInstall, UsbPart};

/// What a maker says about a release. The image's size and hash, and each part's, are worked out
/// from the bytes.
#[derive(Clone, Debug, Default)]
pub struct Release {
    pub device_type: String,
    pub hardware_revisions: Vec<String>,
    pub firmware_version: String,
    pub security_counter: u32,
    pub summary: Option<String>,
    pub key_set: Option<Vec<PublicKey>>,
    pub usb_install: Option<UsbLayout>,
}

/// Where a first USB install writes things, and the extra parts it writes.
#[derive(Clone, Debug)]
pub struct UsbLayout {
    pub chip: String,
    pub min_flash_bytes: u64,
    pub app_offset: u64,
    /// In the order they go in the package.
    pub parts: Vec<UsbPartFile>,
}

#[derive(Clone, Debug)]
pub struct UsbPartFile {
    pub name: String,
    pub offset: u64,
    pub bytes: Vec<u8>,
}

/// A package, and the result of checking it.
#[derive(Clone, Debug)]
pub struct Built {
    pub bytes: Vec<u8>,
    pub checked: CheckedPackage,
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("the app image is {0} bytes; it must be from {MIN_IMAGE_SIZE} to {MAX_IMAGE_SIZE}")]
    ImageSize(u64),
    #[error("{0}")]
    Manifest(ManifestError),
    #[error("the USB-install layout is wrong: {0}")]
    Layout(LayoutError),
    #[error("the manifest would be {0} bytes, over the limit of {MAX_MANIFEST_LEN}; shorten the summary or list fewer revisions")]
    ManifestTooLong(usize),
    #[error("the package would be {0} bytes, over the limit of {MAX_PACKAGE_LEN}")]
    PackageTooLarge(u64),
    #[error("signing failed")]
    Signing,
    /// The package built didn't pass its own check. This would be a fault in this crate.
    #[error("the package built doesn't pass its own check: {0}")]
    SelfCheck(PackageError),
}

/// Builds and signs a package, then checks it as the hub would. Nothing it returns has failed a check.
pub fn build_and_sign(key: &SigningKey, release: &Release, image: &[u8]) -> Result<Built, BuildError> {
    let image_size = image.len() as u64;
    if !(MIN_IMAGE_SIZE..=MAX_IMAGE_SIZE).contains(&image_size) {
        return Err(BuildError::ImageSize(image_size));
    }
    let manifest = Manifest {
        device_type: release.device_type.clone(),
        hardware_revisions: release.hardware_revisions.clone(),
        firmware_version: release.firmware_version.clone(),
        security_counter: release.security_counter,
        image_size,
        image_sha256: sha256(image),
        summary: release.summary.clone(),
        key_set: release.key_set.clone(),
        usb_install: release.usb_install.as_ref().map(|u| UsbInstall {
            chip: u.chip.clone(),
            min_flash_bytes: u.min_flash_bytes,
            app_offset: u.app_offset,
            parts: u
                .parts
                .iter()
                .map(|p| UsbPart {
                    name: p.name.clone(),
                    offset: p.offset,
                    size: p.bytes.len() as u64,
                    sha256: sha256(&p.bytes),
                })
                .collect(),
        }),
    };
    let json = manifest.to_json();
    if json.len() > MAX_MANIFEST_LEN {
        return Err(BuildError::ManifestTooLong(json.len()));
    }
    // Every rule is checked before signing, so a mistake is reported plainly rather than as a failed
    // self-check.
    Manifest::parse(&json).map_err(BuildError::Manifest)?;
    if let Some(usb) = &manifest.usb_install {
        crate::check::check_layout(usb, image_size).map_err(BuildError::Layout)?;
    }

    let manifest_len = u16::try_from(json.len()).expect("checked against MAX_MANIFEST_LEN");
    let total = crate::check::update_form_len(manifest_len, image_size) + manifest.parts_len();
    if total > MAX_PACKAGE_LEN {
        return Err(BuildError::PackageTooLarge(total));
    }
    let public_key = key.public_key();
    let prefix = signed_prefix(manifest_len, &public_key);
    let signature = key
        .sign(&signed_bytes(&prefix, &json))
        .map_err(|_| BuildError::Signing)?;
    let header = Header {
        manifest_len,
        public_key,
        signature,
    };

    let mut bytes = Vec::with_capacity(total as usize);
    bytes.extend_from_slice(&header.to_bytes());
    bytes.extend_from_slice(&json);
    bytes.extend_from_slice(image);
    for p in release.usb_install.iter().flat_map(|u| &u.parts) {
        bytes.extend_from_slice(&p.bytes);
    }
    let checked = check(Cursor::new(&bytes), bytes.len() as u64).map_err(BuildError::SelfCheck)?;
    Ok(Built { bytes, checked })
}
