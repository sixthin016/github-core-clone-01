// zlib.rs — inflate/deflate (RFC 1950 zlib wrapper + RFC 1951 DEFLATE), std-only.
// inflate: stored + fixed + dynamic Huffman. deflate: fixed Huffman + LZ77
// sederhana (hash satu kandidat). Dipakai packfile dan objek loose git.

use std::collections::HashMap;

// -------------------------------------------------------------------------------------------
// Bit reader (DEFLATE membaca bit LSB-first di dalam tiap byte)
// -------------------------------------------------------------------------------------------

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize, // byte berikutnya
    bitbuf: u32,
    bitcnt: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0, bitbuf: 0, bitcnt: 0 }
    }

    fn bits(&mut self, n: u32) -> Result<u32, String> {
        while self.bitcnt < n {
            if self.pos >= self.data.len() {
                return Err("deflate stream truncated".to_string());
            }
            self.bitbuf |= (self.data[self.pos] as u32) << self.bitcnt;
            self.pos += 1;
            self.bitcnt += 8;
        }
        let v = self.bitbuf & ((1u32 << n) - 1);
        self.bitbuf >>= n;
        self.bitcnt -= n;
        Ok(v)
    }

    /// Buang sisa bit sampai batas byte (dipakai blok stored).
    fn align_byte(&mut self) {
        self.bitbuf = 0;
        self.bitcnt = 0;
    }

    /// Byte yang sudah dikonsumsi dari stream (termasuk bit yang masih di buffer).
    fn byte_pos(&self) -> usize {
        self.pos - (self.bitcnt / 8) as usize
    }
}

// -------------------------------------------------------------------------------------------
// Huffman kanonik
// -------------------------------------------------------------------------------------------

struct Huffman {
    // counts[len] = jumlah simbol dengan panjang kode len (len 1..=15)
    counts: [u16; 16],
    // simbol diurutkan berdasarkan (panjang kode, nilai simbol)
    symbols: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Huffman {
        let mut counts = [0u16; 16];
        for &l in lengths {
            counts[l as usize] += 1;
        }
        counts[0] = 0;
        let mut offs = [0u16; 16];
        for i in 1..16 {
            offs[i] = offs[i - 1] + counts[i - 1];
        }
        let mut symbols = vec![0u16; lengths.iter().filter(|&&l| l != 0).count()];
        for (sym, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbols[offs[l as usize] as usize] = sym as u16;
                offs[l as usize] += 1;
            }
        }
        Huffman { counts, symbols }
    }

    fn decode(&self, br: &mut BitReader) -> Result<u16, String> {
        let mut code: i32 = 0;
        let mut first: i32 = 0;
        let mut index: i32 = 0;
        for len in 1..16 {
            code |= br.bits(1)? as i32;
            let count = self.counts[len] as i32;
            if code - first < count {
                return Ok(self.symbols[(index + (code - first)) as usize]);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err("invalid huffman code".to_string())
    }
}

fn fixed_lit_lengths() -> [u8; 288] {
    let mut l = [0u8; 288];
    for i in 0..144 {
        l[i] = 8;
    }
    for i in 144..256 {
        l[i] = 9;
    }
    for i in 256..280 {
        l[i] = 7;
    }
    for i in 280..288 {
        l[i] = 8;
    }
    l
}

fn fixed_dist_lengths() -> [u8; 32] {
    [5u8; 32]
}

// Tabel panjang (basis, bit ekstra) — RFC 1951 §3.2.5.
const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LEN_EXTRA: [u32; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u32; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// Urutan panjang kode untuk alfabet code-length (RFC 1951 §3.2.7).
const CL_ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

fn inflate_block(
    br: &mut BitReader,
    lit: &Huffman,
    dist: &Huffman,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    loop {
        let sym = lit.decode(br)?;
        if sym == 256 {
            return Ok(());
        }
        if sym < 256 {
            out.push(sym as u8);
            continue;
        }
        // Simbol panjang 257..285.
        let li = (sym - 257) as usize;
        if li >= 29 {
            return Err(format!("bad length symbol {}", sym));
        }
        let len = LEN_BASE[li] as usize + br.bits(LEN_EXTRA[li])? as usize;
        let dsym = dist.decode(br)? as usize;
        if dsym >= 30 {
            return Err(format!("bad distance symbol {}", dsym));
        }
        let d = DIST_BASE[dsym] as usize + br.bits(DIST_EXTRA[dsym])? as usize;
        if d == 0 || d > out.len() {
            return Err("deflate distance too far back".to_string());
        }
        let start = out.len() - d;
        for i in 0..len {
            let b = out[start + i];
            out.push(b);
        }
    }
}

fn inflate_deflate(data: &[u8]) -> Result<(Vec<u8>, usize), String> {
    let mut br = BitReader::new(data);
    let mut out: Vec<u8> = Vec::new();
    loop {
        let bfinal = br.bits(1)?;
        let btype = br.bits(2)?;
        match btype {
            0 => {
                // Stored: sejajarkan byte, LEN + NLEN (16-bit LE), lalu salin.
                br.align_byte();
                if br.pos + 4 > data.len() {
                    return Err("stored block header truncated".to_string());
                }
                let len = u16::from_le_bytes([data[br.pos], data[br.pos + 1]]) as usize;
                let nlen = u16::from_le_bytes([data[br.pos + 2], data[br.pos + 3]]) as usize;
                if len != (!nlen & 0xffff) {
                    return Err("stored block LEN/NLEN mismatch".to_string());
                }
                br.pos += 4;
                if br.pos + len > data.len() {
                    return Err("stored block data truncated".to_string());
                }
                out.extend_from_slice(&data[br.pos..br.pos + len]);
                br.pos += len;
            }
            1 => {
                let lit = Huffman::new(&fixed_lit_lengths());
                let dist = Huffman::new(&fixed_dist_lengths());
                inflate_block(&mut br, &lit, &dist, &mut out)?;
            }
            2 => {
                let hlit = br.bits(5)? as usize + 257;
                let hdist = br.bits(5)? as usize + 1;
                let hclen = br.bits(4)? as usize + 4;
                let mut cl_lengths = [0u8; 19];
                for i in 0..hclen {
                    cl_lengths[CL_ORDER[i]] = br.bits(3)? as u8;
                }
                let cl = Huffman::new(&cl_lengths);
                let total = hlit + hdist;
                let mut lengths = vec![0u8; total];
                let mut i = 0;
                while i < total {
                    let sym = cl.decode(&mut br)?;
                    match sym {
                        0..=15 => {
                            lengths[i] = sym as u8;
                            i += 1;
                        }
                        16 => {
                            if i == 0 {
                                return Err("repeat-previous with no previous length".to_string());
                            }
                            let prev = lengths[i - 1];
                            let rep = 3 + br.bits(2)? as usize;
                            if i + rep > total {
                                return Err("code length repeat overruns table".to_string());
                            }
                            for _ in 0..rep {
                                lengths[i] = prev;
                                i += 1;
                            }
                        }
                        17 => {
                            let rep = 3 + br.bits(3)? as usize;
                            if i + rep > total {
                                return Err("code length repeat overruns table".to_string());
                            }
                            i += rep;
                        }
                        18 => {
                            let rep = 11 + br.bits(7)? as usize;
                            if i + rep > total {
                                return Err("code length repeat overruns table".to_string());
                            }
                            i += rep;
                        }
                        _ => return Err(format!("bad code-length symbol {}", sym)),
                    }
                }
                let lit = Huffman::new(&lengths[..hlit]);
                let dist = Huffman::new(&lengths[hlit..]);
                inflate_block(&mut br, &lit, &dist, &mut out)?;
            }
            _ => return Err("bad deflate block type 3".to_string()),
        }
        if bfinal == 1 {
            break;
        }
    }
    Ok((out, br.byte_pos()))
}

fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for chunk in data.chunks(5552) {
        for &x in chunk {
            a += x as u32;
            b += a;
        }
        a %= MOD;
        b %= MOD;
    }
    (b << 16) | a
}

/// Inflate satu stream zlib (RFC 1950). Mengembalikan hasil dan jumlah byte
/// yang dikonsumsi (header + deflate + adler32) — packfile butuh posisi ini
/// untuk lanjut ke entry berikutnya.
pub fn inflate_with_consumed(data: &[u8]) -> Result<(Vec<u8>, usize), String> {
    if data.len() < 2 {
        return Err("zlib stream too short".to_string());
    }
    let cmf = data[0];
    let flg = data[1];
    if cmf & 0x0f != 8 {
        return Err("zlib: not a deflate stream".to_string());
    }
    if ((cmf as u32) << 8 | flg as u32) % 31 != 0 {
        return Err("zlib header check failed".to_string());
    }
    if flg & 0x20 != 0 {
        return Err("zlib preset dictionary not supported".to_string());
    }
    let (out, deflate_end) = inflate_deflate(&data[2..])?;
    let consumed = 2 + deflate_end;
    if consumed + 4 > data.len() {
        return Err("zlib stream truncated before adler32".to_string());
    }
    let want = u32::from_be_bytes([
        data[consumed],
        data[consumed + 1],
        data[consumed + 2],
        data[consumed + 3],
    ]);
    if want != adler32(&out) {
        return Err("zlib adler32 mismatch".to_string());
    }
    Ok((out, consumed + 4))
}

/// Inflate satu stream zlib utuh.
pub fn inflate(data: &[u8]) -> Result<Vec<u8>, String> {
    let (out, _) = inflate_with_consumed(data)?;
    Ok(out)
}

// -------------------------------------------------------------------------------------------
// deflate — fixed Huffman + LZ77 satu kandidat per hash 3 byte
// -------------------------------------------------------------------------------------------

struct BitWriter {
    out: Vec<u8>,
    bitbuf: u32,
    bitcnt: u32,
}

impl BitWriter {
    fn new() -> Self {
        BitWriter { out: Vec::new(), bitbuf: 0, bitcnt: 0 }
    }

    /// Tulis `n` bit dari `v`, LSB-first (urutan bit DEFLATE).
    fn bits(&mut self, v: u32, n: u32) {
        self.bitbuf |= v << self.bitcnt;
        self.bitcnt += n;
        while self.bitcnt >= 8 {
            self.out.push((self.bitbuf & 0xff) as u8);
            self.bitbuf >>= 8;
            self.bitcnt -= 8;
        }
    }

    /// Tulis kode Huffman: bit-bit kode dibalik (kode kanonik disimpan
    /// MSB-first, tapi DEFLATE mengirim bit paling signifikan dulu di dalam
    /// urutan bit LSB-first, jadi kode harus dibalik).
    fn huff(&mut self, code: u32, n: u32) {
        let rev = code.reverse_bits() >> (32 - n);
        self.bits(rev, n);
    }

    fn finish(mut self) -> Vec<u8> {
        if self.bitcnt > 0 {
            self.out.push((self.bitbuf & 0xff) as u8);
        }
        self.out
    }
}

/// Kode fixed-Huffman untuk literal/length: (kode, panjang bit).
fn fixed_lit_code(sym: u16) -> (u32, u32) {
    match sym {
        0..=143 => (0x30 + sym as u32, 8),
        144..=255 => (0x190 + (sym - 144) as u32, 9),
        256..=279 => ((sym - 256) as u32, 7),
        _ => (0xC0 + (sym - 280) as u32, 8),
    }
}

fn len_code(len: usize) -> (u16, u32, u32) {
    // (simbol, bit ekstra, nilai ekstra)
    for i in (0..29).rev() {
        if len >= LEN_BASE[i] as usize {
            let extra = len - LEN_BASE[i] as usize;
            return (257 + i as u16, LEN_EXTRA[i], extra as u32);
        }
    }
    (257, 0, 0)
}

fn dist_code(d: usize) -> (u16, u32, u32) {
    for i in (0..30).rev() {
        if d >= DIST_BASE[i] as usize {
            let extra = d - DIST_BASE[i] as usize;
            return (i as u16, DIST_EXTRA[i], extra as u32);
        }
    }
    (0, 0, 0)
}

/// Kompres ke format zlib (header 0x78 0x9C + deflate fixed + adler32).
/// Cocok dengan Python `zlib.compress(data, 6)` secara format (byte boleh
/// berbeda, hasil decompress sama).
pub fn deflate(data: &[u8]) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.bits(1, 1); // BFINAL
    w.bits(1, 2); // BTYPE = 01 fixed Huffman

    // Tabel hash 3 byte -> posisi terakhir. Satu kandidat saja: cepat dan
    // cukup untuk objek git (teks/JSON kecil-menengah).
    let mut table: HashMap<u32, usize> = HashMap::with_capacity(4096);
    let mut i = 0usize;
    while i < data.len() {
        let mut matched = false;
        if i + 3 <= data.len() {
            let key = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8) | data[i + 2] as u32;
            if let Some(&prev) = table.get(&key) {
                let dist = i - prev;
                if dist > 0 && dist <= 32768 {
                    // Panjang kecocokan, maks 258.
                    let mut len = 0usize;
                    while len < 258 && i + len < data.len() && data[prev + len] == data[i + len] {
                        len += 1;
                    }
                    if len >= 3 {
                        let (lsym, lextra_bits, lextra) = len_code(len);
                        let (code, nbits) = fixed_lit_code(lsym);
                        w.huff(code, nbits);
                        if lextra_bits > 0 {
                            w.bits(lextra, lextra_bits);
                        }
                        let (dsym, dextra_bits, dextra) = dist_code(dist);
                        w.huff(dsym as u32, 5);
                        if dextra_bits > 0 {
                            w.bits(dextra, dextra_bits);
                        }
                        // Daftarkan posisi yang dilewati agar match berikutnya
                        // tetap bisa ditemukan.
                        for j in i..i + len {
                            if j + 3 <= data.len() {
                                let k = ((data[j] as u32) << 16)
                                    | ((data[j + 1] as u32) << 8)
                                    | data[j + 2] as u32;
                                table.insert(k, j);
                            }
                        }
                        i += len;
                        matched = true;
                    }
                }
            }
            if !matched {
                table.insert(key, i);
            }
        }
        if !matched {
            let (code, nbits) = fixed_lit_code(data[i] as u16);
            w.huff(code, nbits);
            i += 1;
        }
    }

    // End of block (simbol 256, 7 bit, kode 0).
    w.huff(0, 7);
    let mut body = w.finish();

    let mut out = Vec::with_capacity(body.len() + 6);
    out.push(0x78);
    out.push(0x9c);
    out.append(&mut body);
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(data: &[u8]) {
        let compressed = deflate(data);
        let back = inflate(&compressed).expect("inflate own deflate");
        assert_eq!(back, data);
    }

    #[test]
    fn empty() {
        roundtrip(b"");
        // zlib.compress(b"") = 78 9c 03 00 00 00 00 01
        assert_eq!(inflate(&[0x78, 0x9c, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01]).unwrap(), b"");
    }

    #[test]
    fn hello() {
        roundtrip(b"hello world\n");
        // Python: zlib.compress(b"hello world\n", 6)
        let py: [u8; 20] = [
            0x78, 0x9c, 0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57, 0x28, 0xcf, 0x2f, 0xca, 0x49, 0xe1,
            0x02, 0x00, 0x1e, 0x72, 0x04, 0x67,
        ];
        assert_eq!(inflate(&py).unwrap(), b"hello world\n");
    }

    #[test]
    fn random_and_repetitive() {
        // Data acak (pseudo-random deterministik, tak terkompresi).
        let mut x: u32 = 0x12345678;
        let mut rnd = Vec::with_capacity(5000);
        for _ in 0..5000 {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            rnd.push((x >> 24) as u8);
        }
        roundtrip(&rnd);
        // Data repetitif (banyak match LZ77 + jarak jauh).
        let rep = b"abcdefgh".repeat(2000);
        roundtrip(&rep);
        // Teks campuran.
        let text = b"The quick brown fox jumps over the lazy dog. ".repeat(300);
        roundtrip(&text);
    }

    #[test]
    fn git_object_bytes() {
        // Blob nyata: commit git + tree, harus roundtrip.
        let commit = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nauthor A U Thor <a@example.com> 1700000000 +0000\ncommitter A U Thor <a@example.com> 1700000000 +0000\n\ninitial commit\n";
        roundtrip(commit);
    }

    #[test]
    fn stored_block_from_python() {
        // Python zlib.compressobj(0) memakai stored block untuk input kecil.
        let py = python_compress_level0(b"stored block test data 12345");
        assert_eq!(inflate(&py).unwrap(), b"stored block test data 12345");
    }

    #[test]
    fn dynamic_block_from_python() {
        // Teks panjang -> Python pakai dynamic Huffman (level 6).
        let data = b"Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. ".repeat(20);
        let py = python_compress(&data);
        assert_eq!(inflate(&py).unwrap(), data.as_slice());
    }

    #[test]
    fn bad_inputs() {
        assert!(inflate(b"").is_err());
        assert!(inflate(&[0x78, 0x9c, 0x03, 0x00]).is_err());
        assert!(inflate(&[0x78, 0x9c, 0xff, 0xff, 0xff, 0xff]).is_err());
    }

    // --- pembantu tes: panggil Python bila ada, untuk bandingkan format ---
    fn python_compress(data: &[u8]) -> Vec<u8> {
        python_zlib(data, 6)
    }
    fn python_compress_level0(data: &[u8]) -> Vec<u8> {
        python_zlib(data, 0)
    }
    fn python_zlib(data: &[u8], level: i32) -> Vec<u8> {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let mut child = Command::new("python3")
            .args(["-c", "import sys,zlib;sys.stdout.buffer.write(zlib.compress(sys.stdin.buffer.read(),int(sys.argv[1])))", &level.to_string()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("python3 untuk tes zlib");
        child.stdin.as_mut().unwrap().write_all(data).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        out.stdout
    }
}
