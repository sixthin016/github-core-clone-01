// pack.rs — packfile git v2/v3: baca (termasuk OFS_DELTA + REF_DELTA)
// dan tulis (tanpa delta). Objek diidentifikasi ulang lewat SHA-1-nya,
// jadi entry korup tidak bisa menyamar sebagai objek lain.

use crate::object::{hash_object, ObjKind, GitObject, Oid};
use crate::sha1::{hex, sha1};
use crate::zlib::{deflate, inflate_with_consumed};
use std::collections::HashMap;

fn read_varint_size(buf: &[u8], start: usize) -> Result<(usize, usize, u8), String> {
    if start >= buf.len() {
        return Err("pack entry size varint is truncated".to_string());
    }
    let mut byte = buf[start];
    let first = byte;
    let mut size = (byte & 0x0f) as usize;
    let mut shift = 4u32;
    let mut offset = start + 1;
    while byte & 0x80 != 0 {
        if offset >= buf.len() {
            return Err("pack entry size varint is truncated".to_string());
        }
        byte = buf[offset];
        offset += 1;
        size += ((byte & 0x7f) as usize) << shift;
        shift += 7;
        if shift > 63 {
            return Err("pack entry size varint is too long".to_string());
        }
    }
    Ok((size, offset, first))
}

fn read_ofs_delta_base(buf: &[u8], start: usize) -> Result<(usize, usize), String> {
    if start >= buf.len() {
        return Err("ofs-delta base offset is truncated".to_string());
    }
    let mut byte = buf[start];
    let mut offset = start + 1;
    let mut value = (byte & 0x7f) as usize;
    while byte & 0x80 != 0 {
        if offset >= buf.len() {
            return Err("ofs-delta base offset is truncated".to_string());
        }
        byte = buf[offset];
        offset += 1;
        value = ((value + 1) << 7) + ((byte & 0x7f) as usize);
    }
    Ok((value, offset))
}

/// Terapkan stream instruksi delta git ke byte base.
pub fn apply_delta(base: &[u8], delta: &[u8]) -> Result<Vec<u8>, String> {
    let mut offset = 0usize;
    let read_size = |offset: &mut usize| -> Result<usize, String> {
        let mut value = 0usize;
        let mut shift = 0u32;
        loop {
            if *offset >= delta.len() {
                return Err("delta size varint is truncated".to_string());
            }
            let byte = delta[*offset];
            *offset += 1;
            value += ((byte & 0x7f) as usize) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                break;
            }
            if shift > 63 {
                return Err("delta size varint is too long".to_string());
            }
        }
        Ok(value)
    };
    let source_size = read_size(&mut offset)?;
    let target_size = read_size(&mut offset)?;
    if source_size != base.len() {
        return Err(format!(
            "delta expects a {}-byte base, got {}",
            source_size,
            base.len()
        ));
    }
    let mut out = vec![0u8; target_size];
    let mut written = 0usize;
    while offset < delta.len() {
        let op = delta[offset];
        offset += 1;
        if op & 0x80 != 0 {
            let mut copy_offset = 0usize;
            let mut copy_size = 0usize;
            let mut shift = 0u32;
            for bit in 0..4 {
                if op & (1 << bit) != 0 {
                    if offset >= delta.len() {
                        return Err("delta copy offset is truncated".to_string());
                    }
                    copy_offset += (delta[offset] as usize) << shift;
                    offset += 1;
                }
                shift += 8;
            }
            shift = 0;
            for bit in 4..7 {
                if op & (1 << bit) != 0 {
                    if offset >= delta.len() {
                        return Err("delta copy size is truncated".to_string());
                    }
                    copy_size += (delta[offset] as usize) << shift;
                    offset += 1;
                }
                shift += 8;
            }
            if copy_size == 0 {
                copy_size = 0x10000;
            }
            if copy_offset + copy_size > base.len() {
                return Err("delta copy is out of the base".to_string());
            }
            if written + copy_size > target_size {
                return Err("delta writes past its target size".to_string());
            }
            out[written..written + copy_size]
                .copy_from_slice(&base[copy_offset..copy_offset + copy_size]);
            written += copy_size;
        } else if op != 0 {
            let literal = (op & 0x7f) as usize;
            if offset + literal > delta.len() {
                return Err("delta insert is truncated".to_string());
            }
            if written + literal > target_size {
                return Err("delta writes past its target size".to_string());
            }
            out[written..written + literal].copy_from_slice(&delta[offset..offset + literal]);
            offset += literal;
            written += literal;
        } else {
            return Err("delta opcode 0 is invalid".to_string());
        }
    }
    if written != target_size {
        return Err(format!("delta produced {} of {} bytes", written, target_size));
    }
    Ok(out)
}

struct RawEntry {
    offset: usize,
    type_code: u8,
    size: usize,
    data: Vec<u8>,
    base_offset: Option<usize>,
    base_oid: Option<String>,
    resolved: Option<(ObjKind, Vec<u8>, Oid)>,
}

/// Baca packfile v2/v3, selesaikan OFS_DELTA dan REF_DELTA. `lookup`
/// memasok base yang ditinggalkan thin pack. Mengembalikan objek lengkap
/// (oid, GitObject). Err pada masalah struktural apa pun.
pub fn read_pack(
    data: &[u8],
    lookup: &dyn Fn(&str) -> Option<GitObject>,
) -> Result<Vec<(Oid, GitObject)>, String> {
    if data.len() < 32 {
        return Err(format!("pack is {} bytes, too short", data.len()));
    }
    if &data[0..4] != b"PACK" {
        return Err("pack does not start with PACK".to_string());
    }
    let version = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    if version != 2 && version != 3 {
        return Err(format!("unsupported pack version {}", version));
    }
    let declared = u32::from_be_bytes([data[8], data[9], data[10], data[11]]) as usize;
    let trailer_start = data.len() - 20;
    let trailer = hex(&data[trailer_start..]);
    let want = hex(&sha1(&data[..trailer_start]));
    if trailer != want {
        return Err("pack trailer does not match its bytes".to_string());
    }

    let body = &data[..trailer_start];
    let mut raw: Vec<RawEntry> = Vec::with_capacity(declared);
    let mut offset = 12usize;
    let mut has_delta = false;
    for index in 0..declared {
        let entry_offset = offset;
        let (size, next, first) = read_varint_size(data, offset)?;
        offset = next;
        let type_code = (first >> 4) & 0x07;
        let mut entry = RawEntry {
            offset: entry_offset,
            type_code,
            size,
            data: Vec::new(),
            base_offset: None,
            base_oid: None,
            resolved: None,
        };
        if type_code == 6 {
            has_delta = true;
            let (base_off, next) = read_ofs_delta_base(data, offset)?;
            offset = next;
            if base_off == 0 || base_off > entry_offset {
                return Err(format!(
                    "ofs-delta at {} names an invalid base offset {}",
                    entry_offset, base_off
                ));
            }
            entry.base_offset = Some(entry_offset - base_off);
        } else if type_code == 7 {
            has_delta = true;
            if offset + 20 > trailer_start {
                return Err("ref-delta base id is truncated".to_string());
            }
            entry.base_oid = Some(hex(&data[offset..offset + 20]));
            offset += 20;
        } else if ObjKind::from_code(type_code).is_none() {
            return Err(format!("pack entry {} has unknown type code {}", index, type_code));
        }
        let (inflated, consumed) = inflate_with_consumed(&body[offset..])
            .map_err(|e| format!("zlib stream at offset {} did not inflate: {}", offset, e))?;
        if inflated.len() != size {
            return Err(format!(
                "pack entry at {} declares {} bytes but inflated to {}",
                entry_offset,
                size,
                inflated.len()
            ));
        }
        entry.data = inflated;
        offset += consumed;
        raw.push(entry);
    }
    if offset != trailer_start {
        return Err(format!(
            "pack has {} stray bytes before its trailer",
            trailer_start - offset
        ));
    }

    let mut objects: Vec<(Oid, GitObject)> = Vec::with_capacity(raw.len());
    let mut seen: HashMap<Oid, ()> = HashMap::new();
    let push = |oid: Oid, obj: GitObject,
                    objects: &mut Vec<(Oid, GitObject)>,
                    seen: &mut HashMap<Oid, ()>| {
        if seen.insert(oid.clone(), ()).is_none() {
            objects.push((oid, obj));
        }
    };

    if !has_delta {
        for entry in &raw {
            let kind = ObjKind::from_code(entry.type_code).unwrap();
            let oid = hash_object(kind, &entry.data);
            push(oid, GitObject { kind, data: entry.data.clone() }, &mut objects, &mut seen);
        }
        return Ok(objects);
    }

    let mut by_offset: HashMap<usize, usize> = HashMap::new();
    for (i, e) in raw.iter().enumerate() {
        by_offset.insert(e.offset, i);
    }
    let mut resolved_by_oid: HashMap<Oid, usize> = HashMap::new();
    let mut resolving: HashMap<usize, ()> = HashMap::new();

    fn resolve(
        raw: &mut Vec<RawEntry>,
        idx: usize,
        by_offset: &HashMap<usize, usize>,
        resolved_by_oid: &mut HashMap<Oid, usize>,
        resolving: &mut HashMap<usize, ()>,
        lookup: &dyn Fn(&str) -> Option<GitObject>,
    ) -> Result<(ObjKind, Vec<u8>, Oid), String> {
        if let Some(r) = &raw[idx].resolved {
            return Ok(r.clone());
        }
        if resolving.contains_key(&raw[idx].offset) {
            return Err(format!("delta cycle at pack offset {}", raw[idx].offset));
        }
        resolving.insert(raw[idx].offset, ());
        let (kind, data) = match raw[idx].type_code {
            6 => {
                let base_off = raw[idx].base_offset.unwrap();
                let base_idx = *by_offset
                    .get(&base_off)
                    .ok_or_else(|| format!("ofs-delta base at offset {} is not in the pack", base_off))?;
                let (bk, bd, _) = resolve(
                    &mut *raw,
                    base_idx,
                    by_offset,
                    &mut *resolved_by_oid,
                    &mut *resolving,
                    lookup,
                )?;
                let delta = raw[idx].data.clone();
                (bk, apply_delta(&bd, &delta)?)
            }
            7 => {
                let wanted = raw[idx].base_oid.clone().unwrap();
                if let Some(&base_idx) = resolved_by_oid.get(&wanted) {
                    let (bk, bd, _) = resolve(
                        &mut *raw,
                        base_idx,
                        by_offset,
                        &mut *resolved_by_oid,
                        &mut *resolving,
                        lookup,
                    )?;
                    let delta = raw[idx].data.clone();
                    (bk, apply_delta(&bd, &delta)?)
                } else {
                    let external = lookup(&wanted).ok_or_else(|| {
                        format!("ref-delta base {} is neither in the pack nor in the store", wanted)
                    })?;
                    let delta = raw[idx].data.clone();
                    (external.kind, apply_delta(&external.data, &delta)?)
                }
            }
            code => {
                let kind = ObjKind::from_code(code).unwrap();
                (kind, raw[idx].data.clone())
            }
        };
        let oid = hash_object(kind, &data);
        let out = (kind, data, oid.clone());
        raw[idx].resolved = Some(out.clone());
        resolved_by_oid.insert(oid, idx);
        resolving.remove(&raw[idx].offset);
        Ok(out)
    }

    let mut raw = raw;
    for i in 0..raw.len() {
        let (kind, data, oid) = resolve(
            &mut raw,
            i,
            &by_offset,
            &mut resolved_by_oid,
            &mut resolving,
            lookup,
        )?;
        push(oid, GitObject { kind, data }, &mut objects, &mut seen);
    }
    Ok(objects)
}

fn write_pack_object_header(kind: ObjKind, size: usize) -> Vec<u8> {
    let code = kind.code();
    let mut bytes = Vec::new();
    let mut remaining = size;
    let mut first = (code << 4) | ((remaining & 0x0f) as u8);
    remaining >>= 4;
    while remaining > 0 {
        bytes.push(first | 0x80);
        first = (remaining & 0x7f) as u8;
        remaining >>= 7;
    }
    bytes.push(first);
    bytes
}

/// Bangun packfile v2 tanpa delta dari objek polos.
pub fn write_pack(objects: &[(Oid, GitObject)]) -> Vec<u8> {
    let mut encoded: Vec<Vec<u8>> = Vec::with_capacity(objects.len());
    let mut total = 12 + 20;
    for (_, obj) in objects {
        let deflated = deflate(&obj.data);
        let header = write_pack_object_header(obj.kind, obj.data.len());
        let mut entry = Vec::with_capacity(header.len() + deflated.len());
        entry.extend_from_slice(&header);
        entry.extend_from_slice(&deflated);
        total += entry.len();
        encoded.push(entry);
    }
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(b"PACK");
    out.extend_from_slice(&2u32.to_be_bytes());
    out.extend_from_slice(&(objects.len() as u32).to_be_bytes());
    for entry in &encoded {
        out.extend_from_slice(entry);
    }
    let checksum = sha1(&out);
    out.extend_from_slice(&checksum);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{build_commit, hash_object, parse_tree};

    fn no_lookup(_oid: &str) -> Option<GitObject> {
        None
    }

    #[test]
    fn write_then_read_roundtrip() {
        let blob = GitObject { kind: ObjKind::Blob, data: b"hello pack world\n".to_vec() };
        let tree_data = {
            let mut v = Vec::new();
            v.extend_from_slice(b"100644 f.txt\0");
            v.extend_from_slice(&crate::sha1::unhex(&hash_object(ObjKind::Blob, &blob.data)).unwrap());
            v
        };
        let tree = GitObject { kind: ObjKind::Tree, data: tree_data };
        let commit_data = build_commit(
            &hash_object(ObjKind::Tree, &tree.data),
            &[],
            "first\n",
            "A <a@x> 1 +0000",
            "A <a@x> 1 +0000",
        );
        let commit = GitObject { kind: ObjKind::Commit, data: commit_data };
        let objs: Vec<(Oid, GitObject)> = vec![
            (hash_object(ObjKind::Blob, &blob.data), blob.clone()),
            (hash_object(ObjKind::Tree, &tree.data), tree.clone()),
            (hash_object(ObjKind::Commit, &commit.data), commit.clone()),
        ];
        let pack = write_pack(&objs);
        let back = read_pack(&pack, &no_lookup).expect("read own pack");
        assert_eq!(back.len(), 3);
        for (oid, obj) in &objs {
            let found = back.iter().find(|(o, _)| o == oid).expect("object in pack");
            assert_eq!(&found.1, obj);
        }
    }

    #[test]
    fn read_real_git_pack() {
        // Pack nyata dari `git pack-objects`, dibuat oleh skrip tes bila git ada.
        let pack = git_pack_fixture();
        let back = read_pack(&pack, &no_lookup).expect("read git pack");
        assert_eq!(back.len(), 3);
        let blob = back.iter().find(|(_, o)| o.kind == ObjKind::Blob).unwrap();
        assert_eq!(blob.1.data, b"pack fixture blob\n");
        let tree = back.iter().find(|(_, o)| o.kind == ObjKind::Tree).unwrap();
        let entries = parse_tree(&tree.1.data);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "f.txt");
    }

    #[test]
    fn read_deltified_git_pack() {
        // Pack dengan delta (git pack-objects default mendelta blob mirip).
        let pack = git_pack_fixture_delta();
        let back = read_pack(&pack, &no_lookup).expect("read deltified pack");
        let texts: Vec<String> = back
            .iter()
            .filter(|(_, o)| o.kind == ObjKind::Blob)
            .map(|(_, o)| String::from_utf8_lossy(&o.data).into_owned())
            .collect();
        assert!(texts.iter().any(|t| t.contains("line one")));
        assert!(texts.iter().any(|t| t.contains("line two")));
    }

    #[test]
    fn bad_packs_rejected() {
        assert!(read_pack(b"", &no_lookup).is_err());
        assert!(read_pack(b"PACK", &no_lookup).is_err());
        let mut pack = write_pack(&[(
            hash_object(ObjKind::Blob, b"x"),
            GitObject { kind: ObjKind::Blob, data: b"x".to_vec() },
        )]);
        // Rusak trailer.
        let n = pack.len();
        pack[n - 1] ^= 0xff;
        assert!(read_pack(&pack, &no_lookup).is_err());
    }

    #[test]
    fn delta_apply() {
        // Delta: salin "hello " (6 byte dari offset 0), sisip "Rust".
        // base "hello world", target "hello Rust".
        let base = b"hello world";
        let mut delta = vec![11u8, 10u8]; // ukuran sumber 11, target 10
        delta.push(0x80 | 0x10); // copy: size bit (bit 4) saja, offset 0
        delta.push(6); // size 6
        delta.push(4); // insert 4 byte
        delta.extend_from_slice(b"Rust");
        let out = apply_delta(base, &delta).unwrap();
        assert_eq!(out, b"hello Rust");
    }

    // --- fixture: bangun pack nyata dengan git CLI ---
    fn git_pack_fixture() -> Vec<u8> {
        git_pack(&["f.txt"], &[b"pack fixture blob\n"])
    }

    fn git_pack_fixture_delta() -> Vec<u8> {
        // Dua blob besar yang hampir sama -> git mendelta yang kedua.
        let a = ("a.txt".to_string(), vec![b"line one\nline two\nline three\n".repeat(50)].concat());
        let b = ("b.txt".to_string(), vec![b"line one\nline two\nline three\nline four\n".repeat(50)].concat());
        git_pack_multi(&[a, b], "delta")
    }

    fn git_pack(names: &[&str], datas: &[&[u8]]) -> Vec<u8> {
        // Satu commit lengkap (blob -> tree -> commit), lalu pack commit itu:
        // pack-objects mengikutkan semua objek yang terjangkau (3 objek).
        use std::io::Write;
        use std::process::{Command, Stdio};
        let dir = std::env::temp_dir().join(format!("gcc-pack-test-{}-plain", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let init = Command::new("git").args(["init", "-q"]).current_dir(&dir).output().unwrap();
        assert!(init.status.success(), "git init gagal");
        let mut tree_input = String::new();
        for (name, data) in names.iter().zip(datas.iter()) {
            let mut child = Command::new("git")
                .args(["hash-object", "-w", "--stdin"])
                .current_dir(&dir)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .expect("git hash-object");
            child.stdin.as_mut().unwrap().write_all(data).unwrap();
            let out = child.wait_with_output().unwrap();
            assert!(out.status.success());
            let oid = String::from_utf8(out.stdout).unwrap();
            tree_input.push_str(&format!("100644 blob {}\t{}\n", oid.trim(), name));
        }
        let mut child = Command::new("git")
            .args(["mktree"])
            .current_dir(&dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("git mktree");
        child.stdin.as_mut().unwrap().write_all(tree_input.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "git mktree gagal");
        let tree_oid = String::from_utf8(out.stdout).unwrap();
        let mut child = Command::new("git")
            .args(["commit-tree", tree_oid.trim(), "-m", "test"])
            .current_dir(&dir)
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@x.invalid")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@x.invalid")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .expect("git commit-tree");
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "git commit-tree gagal");
        let commit_oid = String::from_utf8(out.stdout).unwrap();
        let mut child = Command::new("git")
            .args(["pack-objects", "--revs", "--stdout"])
            .current_dir(&dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("git pack-objects");
        child.stdin.as_mut().unwrap().write_all(commit_oid.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "git pack-objects gagal");
        let _ = std::fs::remove_dir_all(&dir);
        out.stdout
    }

    fn git_pack_multi(pairs: &[(String, Vec<u8>)], tag: &str) -> Vec<u8> {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let dir = std::env::temp_dir().join(format!("gcc-pack-test-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // hash-object -w dan pack-objects butuh repo.
        let init = Command::new("git").args(["init", "-q"]).current_dir(&dir).output().unwrap();
        assert!(init.status.success(), "git init gagal");
        let mut oids = String::new();
        for (_name, data) in pairs {
            let mut child = Command::new("git")
                .args(["hash-object", "-w", "--stdin"])
                .current_dir(&dir)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .expect("git hash-object");
            child.stdin.as_mut().unwrap().write_all(data).unwrap();
            let out = child.wait_with_output().unwrap();
            assert!(out.status.success());
            let oid = String::from_utf8(out.stdout).unwrap();
            oids.push_str(oid.trim());
            oids.push('\n');
        }
        // Buat pack dari objek-objek itu (tanpa commit/tree — cukup blob).
        let mut child = Command::new("git")
            .args(["pack-objects", "--stdout"])
            .current_dir(&dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("git pack-objects");
        child.stdin.as_mut().unwrap().write_all(oids.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "git pack-objects gagal");
        let _ = std::fs::remove_dir_all(&dir);
        out.stdout
    }
}
