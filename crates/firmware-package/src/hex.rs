//! Lower-case hex, the only form the format uses. Upper-case digits are refused when reading.

const DIGITS: &[u8; 16] = b"0123456789abcdef";

pub fn encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 0x0f) as usize] as char);
    }
    s
}

/// Exactly `N` bytes from `2 * N` lower-case hex digits.
pub fn decode_exact<const N: usize>(s: &str) -> Option<[u8; N]> {
    let s = s.as_bytes();
    if s.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for (i, pair) in s.as_chunks::<2>().0.iter().enumerate() {
        out[i] = (digit(pair[0])? << 4) | digit(pair[1])?;
    }
    Some(out)
}

/// Any number of bytes from an even number of lower-case hex digits.
pub fn decode(s: &str) -> Option<Vec<u8>> {
    let s = s.as_bytes();
    if !s.len().is_multiple_of(2) {
        return None;
    }
    s.as_chunks::<2>()
        .0
        .iter()
        .map(|pair| Some((digit(pair[0])? << 4) | digit(pair[1])?))
        .collect()
}

fn digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_refuses_upper_case() {
        assert_eq!(encode(&[0x00, 0x3f, 0xa0, 0xff]), "003fa0ff");
        assert_eq!(decode_exact::<2>("3fa0"), Some([0x3f, 0xa0]));
        assert_eq!(decode_exact::<2>("3FA0"), None);
        assert_eq!(decode_exact::<2>("3fa"), None);
        assert_eq!(decode_exact::<2>("3fa0aa"), None);
        assert_eq!(decode_exact::<1>("g0"), None);
        assert_eq!(decode("0a0b"), Some(vec![10, 11]));
        assert_eq!(decode("0a0"), None);
    }
}
