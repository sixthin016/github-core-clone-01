// pktline.rs — pkt-line git (format wire git-upload-pack / git-receive-pack).
// Satu paket: 4 digit hex panjang (termasuk 4 byte header) + payload.
// "0000" = flush, "0001" = delim, "0002" = response-end.

pub const MAX_PAYLOAD: usize = 65516;
pub const MAX_PACKET: usize = 65520;

/// Satu paket hasil decode.
#[derive(Debug, Clone, PartialEq)]
pub enum Packet {
    Data(Vec<u8>),
    Flush,
    Delim,
    ResponseEnd,
}

/// Encode satu pkt-line. Panic bila payload > 65516 byte.
pub fn encode_pkt_line(payload: &[u8]) -> Vec<u8> {
    assert!(payload.len() <= MAX_PAYLOAD, "pkt-line payload over the 65516-byte maximum");
    let mut out = Vec::with_capacity(4 + payload.len());
    let len = payload.len() + 4;
    out.extend_from_slice(format!("{:04x}", len).as_bytes());
    out.extend_from_slice(payload);
    out
}

/// Encode satu pkt-line dari teks.
pub fn encode_pkt_line_str(payload: &str) -> Vec<u8> {
    encode_pkt_line(payload.as_bytes())
}

/// Paket flush "0000".
pub fn pkt_flush() -> Vec<u8> {
    b"0000".to_vec()
}

/// Paket delim "0001".
pub fn pkt_delim() -> Vec<u8> {
    b"0001".to_vec()
}

/// Paket response-end "0002".
pub fn pkt_response_end() -> Vec<u8> {
    b"0002".to_vec()
}

/// Encode daftar item: Data / Flush / Delim / ResponseEnd.
pub fn encode_pkt_lines(items: &[Packet]) -> Vec<u8> {
    let mut out = Vec::new();
    for item in items {
        match item {
            Packet::Data(p) => out.extend_from_slice(&encode_pkt_line(p)),
            Packet::Flush => out.extend_from_slice(b"0000"),
            Packet::Delim => out.extend_from_slice(b"0001"),
            Packet::ResponseEnd => out.extend_from_slice(b"0002"),
        }
    }
    out
}

fn is_hex_byte(b: u8) -> bool {
    b.is_ascii_hexdigit()
}

/// Decode seluruh buffer pkt-line. None bila stream tidak well-formed
/// (panjang buruk, paket terpotong, panjang > 65520). Mengembalikan paket
/// dan jumlah byte yang dikonsumsi — pemanggil yang butuh byte mentah
/// setelahnya (mis. packfile) membaca dari offset itu sendiri.
pub fn decode_pkt_lines(bytes: &[u8]) -> Option<(Vec<Packet>, usize)> {
    let mut packets = Vec::new();
    let mut offset = 0usize;
    while offset < bytes.len() {
        if offset + 4 > bytes.len() {
            return None;
        }
        for i in 0..4 {
            if !is_hex_byte(bytes[offset + i]) {
                return None;
            }
        }
        let head = std::str::from_utf8(&bytes[offset..offset + 4]).ok()?;
        let length = usize::from_str_radix(head, 16).ok()?;
        match length {
            0 => {
                packets.push(Packet::Flush);
                offset += 4;
            }
            1 => {
                packets.push(Packet::Delim);
                offset += 4;
            }
            2 => {
                packets.push(Packet::ResponseEnd);
                offset += 4;
            }
            3 | 65521.. => return None,
            _ => {
                if offset + length > bytes.len() {
                    return None;
                }
                packets.push(Packet::Data(bytes[offset + 4..offset + length].to_vec()));
                offset += length;
            }
        }
    }
    Some((packets, offset))
}

/// Teks (UTF-8, lossy) dari semua paket data, berurutan.
pub fn pkt_texts(packets: &[Packet]) -> Vec<String> {
    let mut out = Vec::new();
    for p in packets {
        if let Packet::Data(d) = p {
            out.push(String::from_utf8_lossy(d).into_owned());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip() {
        let items = vec![
            Packet::Data(b"hello\n".to_vec()),
            Packet::Flush,
            Packet::Data(b"side-band".to_vec()),
            Packet::Delim,
            Packet::ResponseEnd,
        ];
        let encoded = encode_pkt_lines(&items);
        let (decoded, consumed) = decode_pkt_lines(&encoded).unwrap();
        assert_eq!(decoded, items);
        assert_eq!(consumed, encoded.len());
    }

    #[test]
    fn header_format() {
        assert_eq!(encode_pkt_line(b"abc"), b"0007abc");
        assert_eq!(encode_pkt_line_str(""), b"0004");
        assert_eq!(pkt_flush(), b"0000");
    }

    #[test]
    fn bad_streams_rejected() {
        assert!(decode_pkt_lines(b"000").is_none()); // terpotong
        assert!(decode_pkt_lines(b"0003abc").is_none()); // panjang 3 ilegal
        assert!(decode_pkt_lines(b"0009ab").is_none()); // payload terpotong
        assert!(decode_pkt_lines(b"zzzz").is_none()); // bukan hex
        assert!(decode_pkt_lines(b"fff0").is_none()); // > 65520
    }

    #[test]
    fn texts_only_data() {
        let (pkts, _) = decode_pkt_lines(&encode_pkt_lines(&[
            Packet::Data(b"a\n".to_vec()),
            Packet::Flush,
            Packet::Data(b"b\n".to_vec()),
        ]))
        .unwrap();
        assert_eq!(pkt_texts(&pkts), vec!["a\n", "b\n"]);
    }
}
