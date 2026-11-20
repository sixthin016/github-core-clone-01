// sha1.rs — SHA-1 murni, std-only (FIPS 180-1).
// Dipakai untuk hashing objek git dan checksum packfile.

/// SHA-1 dari `data`, 20 byte digest.
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];

    // Padding: 0x80, nol sampai panjang ≡ 56 (mod 64), lalu panjang bit big-endian.
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = Vec::with_capacity(data.len() + 72);
    msg.extend_from_slice(data);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([chunk[i * 4], chunk[i * 4 + 1], chunk[i * 4 + 2], chunk[i * 4 + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for (i, &v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

/// Byte ke hex lowercase.
pub fn hex(b: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for &byte in b {
        s.push(HEX[(byte >> 4) as usize] as char);
        s.push(HEX[(byte & 0x0f) as usize] as char);
    }
    s
}

/// Hex (40 char) ke 20 byte. None bila bukan hex valid.
pub fn unhex(s: &str) -> Option<[u8; 20]> {
    let bytes = s.as_bytes();
    if bytes.len() != 40 {
        return None;
    }
    let nib = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let mut out = [0u8; 20];
    for i in 0..20 {
        out[i] = (nib(bytes[i * 2])? << 4) | nib(bytes[i * 2 + 1])?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty() {
        assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    }

    #[test]
    fn abc() {
        assert_eq!(hex(&sha1(b"abc")), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }

    #[test]
    fn long_message() {
        // 1.000.000x 'a' — vektor uji klasik.
        let data = vec![b'a'; 1_000_000];
        assert_eq!(hex(&sha1(&data)), "34aa973cd4c4daa4f61eeb2bdbad27316534016f");
    }

    #[test]
    fn git_blob_header() {
        // sha1("blob 12\0hello world\n") — cocok dengan `git hash-object`.
        let mut input = b"blob 12\0".to_vec();
        input.extend_from_slice(b"hello world\n");
        assert_eq!(hex(&sha1(&input)), "3b18e512dba79e4c8300dd08aeb37f8e728b8dad");
    }

    #[test]
    fn unhex_roundtrip() {
        let h = hex(&sha1(b"test"));
        assert_eq!(hex(&unhex(&h).unwrap()), h);
        assert!(unhex("zz").is_none());
        assert!(unhex("abcd").is_none());
    }
}
