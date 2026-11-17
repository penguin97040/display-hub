//! Display Hub signed firmware packages (`.dhfw`), as specified in `docs/firmware-updates.md`.
//!
//! - [`check`] reads a package as a stream and checks it in the order the hub, the app and the
//!   signing tool use ("Checking a package"), holding no more than the header and manifest in memory.
//! - [`build_and_sign`] makes a package from an image, a release description and optional USB-install
//!   parts, then checks its own output.
//! - [`display`] is what a display does before installing ("Checks before installing", steps 1 to 7),
//!   for the reference simulator. It verifies with the display's stored keys, never the header's.
//! - [`keyfile`] makes and opens a maker's passphrase-protected signing key file. Its module
//!   documentation describes the file layout.
//! - [`version`] compares firmware versions, as the hub and the app show them.
//! - [`label`] says which names the owner may not give a signing key, as the hub and the app refuse
//!   them.
//!
//! Nothing here decides whether a key should be trusted. Passing [`check`] means a package is well
//! formed and consistent with the key inside it; only a display's own stored keys say more.

pub mod build;
pub mod check;
pub mod display;
pub mod header;
pub mod hex;
pub mod json;
pub mod key;
pub mod keyfile;
pub mod label;
pub mod limits;
pub mod manifest;
pub mod version;

pub use build::{build_and_sign, BuildError, Built, Release, UsbLayout, UsbPartFile};
pub use check::{
    check, check_file, update_form, update_form_len, CheckedPackage, LayoutError, PackageError, PackageId,
};
pub use display::{drops_key_unraised, Display, DisplayHeader, Refusal};
pub use header::{signed_bytes, verify_signature, Header, HeaderError};
pub use key::{KeyId, PublicKey, SigningKey};
pub use keyfile::KeyFileError;
pub use manifest::{Manifest, ManifestError, UsbInstall, UsbPart};
