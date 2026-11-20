// object.rs — model objek git: hashing, commit, tree, tag, reachability,
// dan three-way merge level file. Format byte persis git.

use crate::sha1::{hex, sha1, unhex};
use std::collections::{HashMap, HashSet};

pub type Oid = String; // hex 40 lowercase

pub const ZERO_OID: &str = "0000000000000000000000000000000000000000";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjKind {
    Blob,
    Tree,
    Commit,
    Tag,
}

impl ObjKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ObjKind::Blob => "blob",
            ObjKind::Tree => "tree",
            ObjKind::Commit => "commit",
            ObjKind::Tag => "tag",
        }
    }

    pub fn from_str(s: &str) -> Option<ObjKind> {
        match s {
            "blob" => Some(ObjKind::Blob),
            "tree" => Some(ObjKind::Tree),
            "commit" => Some(ObjKind::Commit),
            "tag" => Some(ObjKind::Tag),
            _ => None,
        }
    }

    /// Kode tipe di packfile (1=commit, 2=tree, 3=blob, 4=tag).
    pub fn code(&self) -> u8 {
        match self {
            ObjKind::Commit => 1,
            ObjKind::Tree => 2,
            ObjKind::Blob => 3,
            ObjKind::Tag => 4,
        }
    }

    pub fn from_code(code: u8) -> Option<ObjKind> {
        match code {
            1 => Some(ObjKind::Commit),
            2 => Some(ObjKind::Tree),
            3 => Some(ObjKind::Blob),
            4 => Some(ObjKind::Tag),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct GitObject {
    pub kind: ObjKind,
    pub data: Vec<u8>,
}

/// SHA-1 atas `<kind> <len>\0` + data.
pub fn hash_object(kind: ObjKind, data: &[u8]) -> Oid {
    let mut input = Vec::with_capacity(data.len() + 32);
    input.extend_from_slice(kind.as_str().as_bytes());
    input.push(b' ');
    input.extend_from_slice(data.len().to_string().as_bytes());
    input.push(0);
    input.extend_from_slice(data);
    hex(&sha1(&input))
}

pub fn is_object_id(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

// -------------------------------------------------------------------------------------------
// commit
// -------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct CommitInfo {
    pub tree: Oid,
    pub parents: Vec<Oid>,
    pub author: String,
    pub committer: String,
    pub message: String,
}

/// Parse objek commit. Panic bila header tree tidak valid (seperti TS throw).
pub fn parse_commit(data: &[u8]) -> CommitInfo {
    let blank = find_subslice(data, b"\n\n");
    let header_end = blank.map(|i| i).unwrap_or(data.len());
    let message = match blank {
        Some(i) => String::from_utf8_lossy(&data[i + 2..]).into_owned(),
        None => String::new(),
    };
    let mut tree = String::new();
    let mut parents = Vec::new();
    let mut author = String::new();
    let mut committer = String::new();
    let header = String::from_utf8_lossy(&data[..header_end]);
    for line in header.split('\n') {
        if line.is_empty() {
            continue;
        }
        let (key, value) = match line.find(' ') {
            Some(sp) => (&line[..sp], &line[sp + 1..]),
            None => (line, ""),
        };
        match key {
            "tree" => tree = value.to_string(),
            "parent" => parents.push(value.to_string()),
            "author" => author = value.to_string(),
            "committer" => committer = value.to_string(),
            _ => {}
        }
    }
    if !is_object_id(&tree) {
        panic!("commit has no valid tree header");
    }
    CommitInfo { tree, parents, author, committer, message }
}

/// Bangun byte objek commit:
/// "tree <hex>\nparent ...\nauthor ...\ncommitter ...\n\nmessage".
pub fn build_commit(
    tree: &str,
    parents: &[String],
    message: &str,
    author: &str,
    committer: &str,
) -> Vec<u8> {
    let mut s = String::new();
    s.push_str("tree ");
    s.push_str(tree);
    s.push('\n');
    for p in parents {
        s.push_str("parent ");
        s.push_str(p);
        s.push('\n');
    }
    s.push_str("author ");
    s.push_str(author);
    s.push('\n');
    s.push_str("committer ");
    s.push_str(committer);
    s.push_str("\n\n");
    s.push_str(message);
    s.into_bytes()
}

// -------------------------------------------------------------------------------------------
// tag
// -------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct TagInfo {
    pub object: Oid,
    pub kind: ObjKind,
    pub tag: String,
    pub tagger: String,
    pub message: String,
}

pub fn parse_tag(data: &[u8]) -> TagInfo {
    let blank = find_subslice(data, b"\n\n");
    let header_end = blank.unwrap_or(data.len());
    let message = match blank {
        Some(i) => String::from_utf8_lossy(&data[i + 2..]).into_owned(),
        None => String::new(),
    };
    let mut headers: HashMap<String, String> = HashMap::new();
    let header = String::from_utf8_lossy(&data[..header_end]);
    for line in header.split('\n') {
        if line.is_empty() {
            continue;
        }
        if let Some(sp) = line.find(' ') {
            headers.insert(line[..sp].to_string(), line[sp + 1..].to_string());
        }
    }
    let object = headers.get("object").cloned().unwrap_or_default();
    let kind_s = headers.get("type").cloned().unwrap_or_default();
    if !is_object_id(&object) {
        panic!("tag has no valid object header");
    }
    let kind = ObjKind::from_str(&kind_s).expect("tag has bad type");
    TagInfo {
        object,
        kind,
        tag: headers.get("tag").cloned().unwrap_or_default(),
        tagger: headers.get("tagger").cloned().unwrap_or_default(),
        message,
    }
}

// -------------------------------------------------------------------------------------------
// tree
// -------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct TreeEntry {
    pub mode: String,
    pub name: String,
    pub oid: Oid,
}

/// Parse objek tree, dalam urutan tersimpan.
pub fn parse_tree(data: &[u8]) -> Vec<TreeEntry> {
    let mut entries = Vec::new();
    let mut offset = 0usize;
    while offset < data.len() {
        let space = match data[offset..].iter().position(|&b| b == b' ') {
            Some(p) => offset + p,
            None => panic!("tree entry has no mode separator"),
        };
        let nul = match data[space + 1..].iter().position(|&b| b == 0) {
            Some(p) => space + 1 + p,
            None => panic!("tree entry is truncated"),
        };
        if nul + 21 > data.len() {
            panic!("tree entry is truncated");
        }
        entries.push(TreeEntry {
            mode: String::from_utf8_lossy(&data[offset..space]).into_owned(),
            name: String::from_utf8_lossy(&data[space + 1..nul]).into_owned(),
            oid: hex(&data[nul + 1..nul + 21]),
        });
        offset = nul + 21;
    }
    entries
}

/// Urutan git: direktori dibandingkan seolah namanya berakhiran '/'.
fn compare_tree_names(a: &TreeEntry, b: &TreeEntry) -> std::cmp::Ordering {
    let an = if a.mode == "40000" { format!("{}/", a.name) } else { a.name.clone() };
    let bn = if b.mode == "40000" { format!("{}/", b.name) } else { b.name.clone() };
    an.as_bytes().cmp(bn.as_bytes())
}

/// Bangun byte objek tree dari entry, diurutkan cara git.
pub fn build_tree(entries: &[TreeEntry]) -> Vec<u8> {
    let mut sorted: Vec<&TreeEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| compare_tree_names(a, b));
    let mut out = Vec::new();
    for e in sorted {
        out.extend_from_slice(e.mode.as_bytes());
        out.push(b' ');
        out.extend_from_slice(e.name.as_bytes());
        out.push(0);
        let raw = unhex(&e.oid).expect("tree entry oid must be 40 hex chars");
        out.extend_from_slice(&raw);
    }
    out
}

/// Bangun tree bertingkat dari daftar file datar (path, oid, mode) yang
/// sudah terurut; tiap tree disimpan lewat `put`. Mengembalikan oid tree akar.
pub fn build_nested_tree(
    files: &[(String, String, String)],
    put: &mut dyn FnMut(ObjKind, &[u8]) -> Oid,
) -> Oid {
    struct Node {
        files: Vec<(String, String, String)>, // (nama, oid, mode)
        dirs: Vec<(String, Node)>,
    }
    fn insert(node: &mut Node, segments: &[&str], oid: &str, mode: &str) {
        if segments.len() == 1 {
            node.files.push((segments[0].to_string(), oid.to_string(), mode.to_string()));
            return;
        }
        let name = segments[0];
        let idx = match node.dirs.iter().position(|(n, _)| n == name) {
            Some(i) => i,
            None => {
                node.dirs.push((name.to_string(), Node { files: Vec::new(), dirs: Vec::new() }));
                node.dirs.len() - 1
            }
        };
        insert(&mut node.dirs[idx].1, &segments[1..], oid, mode);
    }
    fn build(node: &Node, put: &mut dyn FnMut(ObjKind, &[u8]) -> Oid) -> Oid {
        let mut entries: Vec<TreeEntry> = Vec::new();
        for (name, oid, mode) in &node.files {
            entries.push(TreeEntry { mode: mode.clone(), name: name.clone(), oid: oid.clone() });
        }
        for (name, child) in &node.dirs {
            let oid = build(child, put);
            entries.push(TreeEntry { mode: "40000".to_string(), name: name.clone(), oid });
        }
        put(ObjKind::Tree, &build_tree(&entries))
    }

    let mut root = Node { files: Vec::new(), dirs: Vec::new() };
    for (path, oid, mode) in files {
        let segments: Vec<&str> = path.split('/').collect();
        insert(&mut root, &segments, oid, mode);
    }
    build(&root, put)
}

/// Ratakan tree (rekursif) menjadi (path, oid, mode), terurut berdasarkan path.
pub fn flatten_tree(
    tree_oid: &str,
    lookup: &dyn Fn(&str) -> Option<GitObject>,
) -> Vec<(String, String, String)> {
    let mut out: Vec<(String, String, String)> = Vec::new();
    fn walk(
        oid: &str,
        prefix: &str,
        lookup: &dyn Fn(&str) -> Option<GitObject>,
        out: &mut Vec<(String, String, String)>,
    ) {
        let obj = match lookup(oid) {
            Some(o) if o.kind == ObjKind::Tree => o,
            _ => return,
        };
        for entry in parse_tree(&obj.data) {
            let path = if prefix.is_empty() {
                entry.name.clone()
            } else {
                format!("{}/{}", prefix, entry.name)
            };
            if entry.mode == "40000" {
                walk(&entry.oid, &path, lookup, out);
            } else {
                out.push((path, entry.oid, entry.mode));
            }
        }
    }
    walk(tree_oid, "", lookup, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

// -------------------------------------------------------------------------------------------
// reachability
// -------------------------------------------------------------------------------------------

/// Semua objek yang terjangkau dari `roots` (commit, tree, tag, blob).
pub fn collect_reachable(
    roots: &[String],
    lookup: &dyn Fn(&str) -> Option<GitObject>,
) -> HashSet<Oid> {
    let mut seen: HashSet<Oid> = HashSet::new();
    let mut queue: Vec<Oid> = roots.iter().filter(|r| is_object_id(r)).cloned().collect();
    while let Some(oid) = queue.pop() {
        if seen.contains(&oid) {
            continue;
        }
        let obj = match lookup(&oid) {
            Some(o) => o,
            None => continue,
        };
        seen.insert(oid);
        match obj.kind {
            ObjKind::Commit => {
                let c = parse_commit(&obj.data);
                queue.push(c.tree);
                queue.extend(c.parents);
            }
            ObjKind::Tag => {
                queue.push(parse_tag(&obj.data).object);
            }
            ObjKind::Tree => {
                for e in parse_tree(&obj.data) {
                    if e.mode != "160000" {
                        queue.push(e.oid);
                    }
                }
            }
            ObjKind::Blob => {}
        }
    }
    seen
}

/// True bila `ancestor` terjangkau dari `commit` lewat parent (atau sama).
pub fn is_ancestor(
    ancestor: &str,
    commit: &str,
    lookup: &dyn Fn(&str) -> Option<GitObject>,
) -> bool {
    if ancestor == commit {
        return true;
    }
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue = vec![commit.to_string()];
    while let Some(oid) = queue.pop() {
        if !seen.insert(oid.clone()) {
            continue;
        }
        let obj = match lookup(&oid) {
            Some(o) if o.kind == ObjKind::Commit => o,
            _ => continue,
        };
        for parent in parse_commit(&obj.data).parents {
            if parent == ancestor {
                return true;
            }
            queue.push(parent);
        }
    }
    false
}

/// Merge base dua commit: commit pertama yang terjangkau dari kedua ujung,
/// jalan BFS dari kedua tip. None bila histori tidak berbagi apa pun.
pub fn merge_base(
    a: &str,
    b: &str,
    lookup: &dyn Fn(&str) -> Option<GitObject>,
) -> Option<Oid> {
    if a == b {
        return Some(a.to_string());
    }
    let mut from_a: HashSet<String> = HashSet::new();
    let mut queue_a = vec![a.to_string()];
    let mut head_a = 0usize;
    while head_a < queue_a.len() {
        let oid = queue_a[head_a].clone();
        head_a += 1;
        if !from_a.insert(oid.clone()) {
            continue;
        }
        if let Some(obj) = lookup(&oid) {
            if obj.kind == ObjKind::Commit {
                queue_a.extend(parse_commit(&obj.data).parents);
            }
        }
    }
    let mut seen_b: HashSet<String> = HashSet::new();
    let mut queue_b = vec![b.to_string()];
    let mut head_b = 0usize;
    while head_b < queue_b.len() {
        let oid = queue_b[head_b].clone();
        head_b += 1;
        if !seen_b.insert(oid.clone()) {
            continue;
        }
        if from_a.contains(&oid) {
            return Some(oid);
        }
        if let Some(obj) = lookup(&oid) {
            if obj.kind == ObjKind::Commit {
                queue_b.extend(parse_commit(&obj.data).parents);
            }
        }
    }
    None
}

// -------------------------------------------------------------------------------------------
// three-way merge level file
// -------------------------------------------------------------------------------------------

pub struct MergeOut {
    pub files: Vec<(String, String, String)>,
    pub conflicts: Vec<String>,
}

/// Merge tiga arah level file. Path yang tak berubah di kedua sisi ikut tip;
/// yang berubah di satu sisi ikut sisi itu; yang berubah sama di kedua sisi
/// ikut salah satunya; sisanya konflik.
pub fn three_way_merge(
    base: &[(String, String, String)],
    head: &[(String, String, String)],
    tip: &[(String, String, String)],
) -> MergeOut {
    fn to_map<'a>(v: &'a [(String, String, String)]) -> HashMap<&'a str, (&'a str, &'a str)> {
        v.iter().map(|(p, o, m)| (p.as_str(), (o.as_str(), m.as_str()))).collect()
    }
    let b = to_map(base);
    let h = to_map(head);
    let t = to_map(tip);
    let mut paths: HashSet<&str> = HashSet::new();
    for m in [&b, &h, &t] {
        for k in m.keys() {
            paths.insert(k);
        }
    }
    let mut files: Vec<(String, String, String)> = Vec::new();
    let mut conflicts: Vec<String> = Vec::new();
    for path in paths {
        let bv = b.get(path).map(|(o, _)| *o);
        let hv = h.get(path).map(|(o, _)| *o);
        let tv = t.get(path).map(|(o, _)| *o);
        let head_changed = hv != bv;
        let tip_changed = tv != bv;
        if !head_changed {
            if let Some((o, m)) = t.get(path) {
                files.push((path.to_string(), o.to_string(), m.to_string()));
            }
        } else if !tip_changed {
            if let Some((o, m)) = h.get(path) {
                files.push((path.to_string(), o.to_string(), m.to_string()));
            }
        } else if hv == tv {
            if let Some((o, m)) = h.get(path) {
                files.push((path.to_string(), o.to_string(), m.to_string()));
            }
        } else {
            conflicts.push(path.to_string());
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    conflicts.sort();
    MergeOut { files, conflicts }
}

/// Jumlah baris ditambah/dihapus antara dua teks (LCS atas baris).
pub fn line_diff(old_text: &str, new_text: &str) -> (usize, usize) {
    if old_text == new_text {
        return (0, 0);
    }
    let mut a: Vec<&str> = if old_text.is_empty() { vec![] } else { old_text.split('\n').collect() };
    let mut b: Vec<&str> = if new_text.is_empty() { vec![] } else { new_text.split('\n').collect() };
    if !a.is_empty() && a[a.len() - 1].is_empty() {
        a.pop();
    }
    if !b.is_empty() && b[b.len() - 1].is_empty() {
        b.pop();
    }
    let (n, m) = (a.len(), b.len());
    let mut prev = vec![0usize; m + 1];
    let mut cur = vec![0usize; m + 1];
    for i in 1..=n {
        for j in 1..=m {
            cur[j] = if a[i - 1] == b[j - 1] { prev[j - 1] + 1 } else { prev[j].max(cur[j - 1]) };
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    let common = prev[m];
    (m - common, n - common)
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup_map(map: &HashMap<String, GitObject>) -> impl Fn(&str) -> Option<GitObject> + '_ {
        move |oid: &str| map.get(oid).cloned()
    }

    #[test]
    fn hash_matches_git() {
        // `echo -n "hello world" | git hash-object --stdin` tanpa newline:
        // blob 11 byte -> 95d09f2b10159347eece71399a7e2e907ea3df4f
        assert_eq!(hash_object(ObjKind::Blob, b"hello world"), "95d09f2b10159347eece71399a7e2e907ea3df4f");
        assert!(is_object_id("95d09f2b10159347eece71399a7e2e907ea3df4f"));
        assert!(!is_object_id("95D09F2B10159347EECE71399A7E2E907EA3DF4F"));
        assert!(!is_object_id("xyz"));
    }

    #[test]
    fn commit_roundtrip() {
        let tree = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
        let bytes = build_commit(
            tree,
            &["1111111111111111111111111111111111111111".to_string()],
            "initial commit\n",
            "A U Thor <a@example.com> 1700000000 +0000",
            "A U Thor <a@example.com> 1700000000 +0000",
        );
        let c = parse_commit(&bytes);
        assert_eq!(c.tree, tree);
        assert_eq!(c.parents, vec!["1111111111111111111111111111111111111111"]);
        assert_eq!(c.message, "initial commit\n");
        assert!(c.author.contains("A U Thor"));
        // Byte persis format git.
        assert!(bytes.starts_with(b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nparent "));
        assert!(bytes.windows(2).any(|w| w == b"\n\n"));
    }

    #[test]
    fn tree_roundtrip_and_sort() {
        let entries = vec![
            TreeEntry { mode: "100644".into(), name: "b.txt".into(), oid: "a".repeat(40) },
            TreeEntry { mode: "40000".into(), name: "a-dir".into(), oid: "b".repeat(40) },
            TreeEntry { mode: "100644".into(), name: "a.txt".into(), oid: "c".repeat(40) },
        ];
        let bytes = build_tree(&entries);
        let back = parse_tree(&bytes);
        // "a-dir/" < "a.txt" < "b.txt" dalam urutan git.
        assert_eq!(back[0].name, "a-dir");
        assert_eq!(back[1].name, "a.txt");
        assert_eq!(back[2].name, "b.txt");
        assert_eq!(back[0].oid, "b".repeat(40));
    }

    #[test]
    fn nested_tree_and_flatten() {
        let mut store: HashMap<String, GitObject> = HashMap::new();
        let blob_oid = hash_object(ObjKind::Blob, b"hi");
        store.insert(blob_oid.clone(), GitObject { kind: ObjKind::Blob, data: b"hi".to_vec() });
        let files = vec![
            ("a/b/c.txt".to_string(), blob_oid.clone(), "100644".to_string()),
            ("README.md".to_string(), blob_oid.clone(), "100644".to_string()),
        ];
        let mut put = |kind: ObjKind, data: &[u8]| -> Oid {
            let oid = hash_object(kind, data);
            store.insert(oid.clone(), GitObject { kind, data: data.to_vec() });
            oid
        };
        let root = build_nested_tree(&files, &mut put);
        let flat = flatten_tree(&root, &lookup_map(&store));
        assert_eq!(flat.len(), 2);
        assert_eq!(flat[0].0, "README.md");
        assert_eq!(flat[1].0, "a/b/c.txt");
        assert_eq!(flat[1].1, blob_oid);
    }

    #[test]
    fn merge_base_and_merge() {
        // rantai: c1 <- c2 <- c3 ; c1 <- c2' (branch)
        let mut store: HashMap<String, GitObject> = HashMap::new();
        let tree = "4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_string();
        let mut mk_commit = |parents: &[String], msg: &str| -> Oid {
            let data = build_commit(&tree, parents, msg, "A <a@x> 1 +0000", "A <a@x> 1 +0000");
            let oid = hash_object(ObjKind::Commit, &data);
            store.insert(oid.clone(), GitObject { kind: ObjKind::Commit, data });
            oid
        };
        let c1 = mk_commit(&[], "c1");
        let c2 = mk_commit(&[c1.clone()], "c2");
        let c3 = mk_commit(&[c2.clone()], "c3");
        let c2b = mk_commit(&[c1.clone()], "c2b");
        assert_eq!(merge_base(&c3, &c2b, &lookup_map(&store)), Some(c1.clone()));
        assert_eq!(merge_base(&c3, &c3, &lookup_map(&store)), Some(c3.clone()));
        assert!(is_ancestor(&c1, &c3, &lookup_map(&store)));
        assert!(!is_ancestor(&c3, &c1, &lookup_map(&store)));

        let base = vec![("f.txt".to_string(), "aa".repeat(20), "100644".to_string())];
        let head = vec![("f.txt".to_string(), "bb".repeat(20), "100644".to_string())];
        let tip = vec![("f.txt".to_string(), "aa".repeat(20), "100644".to_string())];
        let out = three_way_merge(&base, &head, &tip);
        assert!(out.conflicts.is_empty());
        assert_eq!(out.files[0].1, "bb".repeat(20));
        // Kedua sisi ubah berbeda -> konflik.
        let tip2 = vec![("f.txt".to_string(), "cc".repeat(20), "100644".to_string())];
        let out2 = three_way_merge(&base, &head, &tip2);
        assert_eq!(out2.conflicts, vec!["f.txt"]);
    }

    #[test]
    fn diff_counts() {
        assert_eq!(line_diff("a\nb\nc\n", "a\nx\nc\nd\n"), (2, 1));
        assert_eq!(line_diff("", ""), (0, 0));
        assert_eq!(line_diff("same\n", "same\n"), (0, 0));
    }
}
