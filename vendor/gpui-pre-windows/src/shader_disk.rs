//! Compiled shader bytecode kept on disk, so a later launch skips `D3DCompile`.
//!
//! Plain `std`, so its checks can be tested off Windows. The checksum catches damage and foreign
//! files. It is not a signature, so the directory is trusted as far as the user's own profile is.

use std::path::{Path, PathBuf};

const MAGIC: &[u8; 4] = b"DHMS";
const HEADER: usize = 4 + 8 + 8;

/// FNV-1a. The key names the shader and its source; the checksum guards the stored bytes.
pub(crate) fn hash(parts: &[&[u8]]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325_u64;
    for part in parts {
        for b in *part {
            h = (h ^ *b as u64).wrapping_mul(0x0000_0100_0000_01b3);
        }
        h = (h ^ 0xff).wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

pub(crate) fn file_for(dir: &Path, key: u64) -> PathBuf {
    dir.join(format!("{key:016x}.bin"))
}

/// The bytecode, if `file` is whole and written for `key`. Anything else is simply a miss.
pub(crate) fn decode(file: &[u8], key: u64) -> Option<&[u8]> {
    if file.len() <= HEADER || &file[..4] != MAGIC || file[4..12] != key.to_le_bytes() {
        return None;
    }
    let body = &file[HEADER..];
    (file[12..20] == hash(&[body]).to_le_bytes() && body.starts_with(b"DXBC")).then_some(body)
}

pub(crate) fn read(dir: &Path, key: u64) -> Option<Vec<u8>> {
    let file = std::fs::read(file_for(dir, key)).ok()?;
    decode(&file, key).map(<[u8]>::to_vec)
}

/// Best effort: a failure only means the next launch compiles. The rename keeps a reader, or a
/// second process writing the same file, from ever seeing half of it.
pub(crate) fn write(dir: &Path, key: u64, body: &[u8]) {
    let temp = dir.join(format!("{key:016x}.{}.tmp", std::process::id()));
    let mut file = Vec::with_capacity(HEADER + body.len());
    file.extend_from_slice(MAGIC);
    file.extend_from_slice(&key.to_le_bytes());
    file.extend_from_slice(&hash(&[body]).to_le_bytes());
    file.extend_from_slice(body);
    let saved = std::fs::create_dir_all(dir).and_then(|_| std::fs::write(&temp, &file));
    if saved.and_then(|_| std::fs::rename(&temp, file_for(dir, key))).is_err() {
        let _ = std::fs::remove_file(&temp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dh-shader-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    const BODY: &[u8] = b"DXBC and then some bytecode";

    #[test]
    fn a_written_file_reads_back() {
        let dir = temp_dir("round");
        write(&dir, 7, BODY);
        assert_eq!(read(&dir, 7).as_deref(), Some(BODY));
        assert!(std::fs::read_dir(&dir).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".tmp")));
    }

    #[test]
    fn no_file_or_directory_is_a_miss() {
        assert!(read(&temp_dir("none"), 7).is_none());
    }

    #[test]
    fn every_single_damaged_byte_is_a_miss() {
        let dir = temp_dir("flip");
        write(&dir, 7, BODY);
        let good = std::fs::read(file_for(&dir, 7)).unwrap();
        for i in 0..good.len() {
            let mut bad = good.clone();
            bad[i] ^= 0x40;
            assert!(decode(&bad, 7).is_none(), "byte {i}");
        }
    }

    #[test]
    fn every_truncation_is_a_miss() {
        let dir = temp_dir("cut");
        write(&dir, 7, BODY);
        let good = std::fs::read(file_for(&dir, 7)).unwrap();
        for n in 0..good.len() {
            assert!(decode(&good[..n], 7).is_none(), "length {n}");
        }
    }

    #[test]
    fn another_keys_file_or_foreign_bytes_are_a_miss() {
        let dir = temp_dir("foreign");
        write(&dir, 7, BODY);
        std::fs::rename(file_for(&dir, 7), file_for(&dir, 8)).unwrap();
        assert!(read(&dir, 8).is_none());
        std::fs::write(file_for(&dir, 9), vec![0x5a; 4096]).unwrap();
        assert!(read(&dir, 9).is_none());
        // Whole and correctly summed, but not bytecode.
        write(&dir, 10, b"not a shader");
        assert!(read(&dir, 10).is_none());
    }

    #[test]
    fn a_directory_in_the_way_is_a_miss_and_does_not_panic() {
        let dir = temp_dir("dir");
        std::fs::create_dir_all(file_for(&dir, 7)).unwrap();
        assert!(read(&dir, 7).is_none());
        write(&dir, 7, BODY);
        assert!(read(&dir, 7).is_none());
    }

    #[test]
    fn hash_separates_its_parts() {
        assert_ne!(hash(&[b"ab", b"c"]), hash(&[b"a", b"bc"]));
    }
}
