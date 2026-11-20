// githttp.rs — the git smart-HTTP surface: info/refs advertisements (protocol
// v0 and v2), upload-pack (ls-refs and fetch, v0 and v2) and receive-pack
// (push with report-status in sideband-64k).
//
// The wire shapes follow the suite's own client: a v2 advertisement opens with
// the service preamble and `version 2`; a v0 advertisement opens with the
// service preamble and lists refs with capabilities after a NUL on the first
// line; a v2 fetch answers its sections (acknowledgments, shallow-info,
// packfile) separated by flush packets, the pack carried in sideband band 1.
//
// Ported from git-http.ts (same behaviour, Rust idioms). Uses the pktline,
// object and pack modules per SPEC; the tag parser, reachability walk and
// ancestor check are local helpers built on the object module's primitives.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

use crate::http::{error_response, Request, Response};
use crate::object::{is_object_id, parse_commit, parse_tree, GitObject, ObjKind};
use crate::pack::{read_pack, write_pack};
use crate::pktline::{decode_pkt_lines, encode_pkt_line, encode_pkt_lines, Packet as Pkt};
use crate::store::{AdvTipCache, GitCacheKind, Repo, Store, VIEWER};

const AGENT: &str = "makefun-git-forge/1.0";
const UPLOAD_CAPS_V0: &str = "symref=HEAD:refs/heads/main side-band-64k thin-pack ofs-delta multi_ack multi_ack_detailed shallow agent=makefun-git-forge/1.0";
const RECEIVE_CAPS_V0: &str = "symref=HEAD:refs/heads/main report-status side-band-64k delete-refs ofs-delta agent=makefun-git-forge/1.0";
const ZERO_OID: &str = "0000000000000000000000000000000000000000";

// -------------------------------------------------------------------------------------------
// small helpers
// -------------------------------------------------------------------------------------------

/// Case-insensitive header lookup.
fn req_header<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    req.headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                match (hex_value(bytes[i + 1]), hex_value(bytes[i + 2])) {
                    (Some(hi), Some(lo)) => {
                        out.push((hi << 4) | lo);
                        i += 3;
                    }
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// One query parameter from the request's raw query string.
fn query_get(req: &Request, name: &str) -> Option<String> {
    for pair in req.query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        if percent_decode(key) == name {
            return Some(percent_decode(value));
        }
    }
    None
}

/// A raw (non-JSON) response, mirroring sendRaw: content-type, content-length
/// and a fresh X-GitHub-Request-Id.
fn raw_response(store: &mut Store, status: u16, body: Vec<u8>, content_type: &str) -> Response {
    Response {
        status,
        headers: vec![
            ("content-type".to_string(), content_type.to_string()),
            ("content-length".to_string(), body.len().to_string()),
            (
                "x-github-request-id".to_string(),
                store.next_request_id(),
            ),
        ],
        body,
    }
}

/// A plain-text 400, mirroring the direct writeHead paths in git-http.ts
/// (no request id, no JSON body).
fn plain_error(status: u16, message: &str) -> Response {
    let body = message.as_bytes().to_vec();
    Response {
        status,
        headers: vec![
            (
                "content-type".to_string(),
                "text/plain; charset=utf-8".to_string(),
            ),
            ("content-length".to_string(), body.len().to_string()),
        ],
        body,
    }
}

/// Escape a string the way JSON.stringify would quote it (for the unknown
/// v2 command error line).
fn json_quote(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Escape a string for the fetch/ls-refs cache keys (JSON.stringify of the
/// request arrays, as in git-http.ts).
fn json_string_list(items: &[String]) -> String {
    let mut out = String::from("[");
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&json_quote(item));
    }
    out.push(']');
    out
}

fn is_hex_byte(byte: u8) -> bool {
    matches!(byte, b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F')
}

/// True when `value` is exactly 40 lowercase hex digits.
fn is_hex40(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
}

/// True when `value` has no whitespace or control characters (a ref name).
fn is_ref_name(value: &str) -> bool {
    value.bytes().all(|c| c > 0x20 && c != 0x7f)
}

// -------------------------------------------------------------------------------------------
// tag parsing, reachability, ancestry (local, on top of the object module)
// -------------------------------------------------------------------------------------------

/// The object an annotated tag points at, or None when malformed.
fn parse_tag_target(data: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(data);
    let header_end = text.find("\n\n").unwrap_or(text.len());
    for line in text[..header_end].lines() {
        if let Some(rest) = line.strip_prefix("object ") {
            let target = rest.trim();
            if is_object_id(target) {
                return Some(target.to_string());
            }
            return None;
        }
    }
    None
}

/// Every object reachable from `roots` (commits, trees, tags, blobs).
fn collect_reachable(repo: &Repo, roots: &[String]) -> HashSet<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut stack: Vec<String> = Vec::new();
    for root in roots {
        if is_object_id(root) {
            stack.push(root.clone());
        }
    }
    while let Some(oid) = stack.pop() {
        if seen.contains(&oid) {
            continue;
        }
        let object = match repo.objects.get(&oid) {
            Some(object) => object,
            None => continue,
        };
        seen.insert(oid);
        match object.kind {
            ObjKind::Commit => {
                let commit = parse_commit(&object.data);
                stack.push(commit.tree);
                for parent in commit.parents {
                    stack.push(parent);
                }
            }
            ObjKind::Tag => {
                if let Some(target) = parse_tag_target(&object.data) {
                    stack.push(target);
                }
            }
            ObjKind::Tree => {
                for entry in parse_tree(&object.data) {
                    if entry.mode == "160000" {
                        continue;
                    }
                    stack.push(entry.oid);
                }
            }
            ObjKind::Blob => {}
        }
    }
    seen
}

/// True when `ancestor` is reachable from `commit` by following parents (or equal).
fn is_ancestor(repo: &Repo, ancestor: &str, commit: &str) -> bool {
    if ancestor == commit {
        return true;
    }
    let mut seen: HashSet<String> = HashSet::new();
    let mut stack: Vec<String> = vec![commit.to_string()];
    while let Some(oid) = stack.pop() {
        if !seen.insert(oid.clone()) {
            continue;
        }
        let object = match repo.objects.get(&oid) {
            Some(object) => object,
            None => continue,
        };
        if object.kind != ObjKind::Commit {
            continue;
        }
        for parent in parse_commit(&object.data).parents {
            if parent == ancestor {
                return true;
            }
            stack.push(parent);
        }
    }
    false
}

// -------------------------------------------------------------------------------------------
// advertisements
// -------------------------------------------------------------------------------------------

struct AdvertisedRef {
    oid: String,
    name: String,
    peeled: Option<String>,
}

/// The refs a repository advertises, in advertisement order.
fn advertised_refs(repo: &Repo) -> Vec<AdvertisedRef> {
    let mut out: Vec<AdvertisedRef> = Vec::new();
    let mut heads: Vec<&String> = repo
        .refs
        .keys()
        .filter(|r| r.starts_with("refs/heads/"))
        .collect();
    heads.sort();
    let mut tags: Vec<&String> = repo
        .refs
        .keys()
        .filter(|r| r.starts_with("refs/tags/"))
        .collect();
    tags.sort();
    for r in heads {
        out.push(AdvertisedRef {
            oid: repo.refs[r].clone(),
            name: r.clone(),
            peeled: None,
        });
    }
    for r in tags {
        let oid = repo.refs[r].clone();
        let mut peeled = None;
        if let Some(object) = repo.objects.get(&oid) {
            if object.kind == ObjKind::Tag {
                peeled = parse_tag_target(&object.data);
            }
        }
        out.push(AdvertisedRef {
            oid,
            name: r.clone(),
            peeled,
        });
    }
    out
}

fn v0_advertisement(service: &str, repo: &mut Repo) -> Vec<u8> {
    // Cached per git fingerprint: the clone round re-advertises one unchanged
    // repository many times, and each push re-advertises its own new tip once.
    let key = format!("v0:{}:{}", service, repo.git_fingerprint());
    if let Some(cached) = repo.adv_cache.get(&key) {
        return cached.clone();
    }
    let refs = advertised_refs(repo);
    let tip = repo.default_tip();
    // Fast path (the push workload: one head ref, no tags): only the tip moved
    // since the last advertisement, so patch the two 40-hex occurrences of
    // the old tip (the HEAD line and the ref line) instead of rebuilding.
    // Anything else (new ref, tag, different service) falls through to the
    // full build.
    if tip.is_some()
        && refs.len() == 1
        && refs[0].name == "refs/heads/main"
        && refs[0].peeled.is_none()
    {
        let tip_str = tip.clone().unwrap();
        let mut patched: Option<Vec<u8>> = None;
        if let Some(prev) = repo.adv_tip_cache.clone() {
            if prev.service == service && prev.refs_key == "refs/heads/main" && prev.tip != tip_str {
                let old_tip = prev.tip.as_bytes();
                let buf = &prev.buf;
                let first = find_subslice(buf, old_tip);
                let second = first.and_then(|f| {
                    find_subslice(&buf[f + 40..], old_tip).map(|s| f + 40 + s)
                });
                let third = second.and_then(|s| {
                    find_subslice(&buf[s + 40..], old_tip).map(|t| s + 40 + t)
                });
                if let (Some(first), Some(second)) = (first, second) {
                    if third.is_none() {
                        let mut out = buf.clone();
                        out[first..first + 40].copy_from_slice(tip_str.as_bytes());
                        out[second..second + 40].copy_from_slice(tip_str.as_bytes());
                        patched = Some(out);
                    }
                }
            }
        }
        if let Some(out) = patched {
            repo.adv_tip_cache = Some(AdvTipCache {
                service: service.to_string(),
                refs_key: "refs/heads/main".to_string(),
                tip: tip_str,
                buf: out.clone(),
            });
            repo.cache_git(GitCacheKind::Adv, key, out.clone());
            return out;
        }
    }
    let caps = if service == "git-upload-pack" {
        UPLOAD_CAPS_V0
    } else {
        RECEIVE_CAPS_V0
    };
    let mut lines: Vec<Pkt> = vec![
        Pkt::Data(format!("# service={}\n", service).into_bytes()),
        Pkt::Flush,
    ];
    let default_tip = repo.default_tip();
    if refs.is_empty() {
        lines.push(Pkt::Data(
            format!("{} capabilities^{{}}\0{}\n", ZERO_OID, caps).into_bytes(),
        ));
    } else {
        lines.push(Pkt::Data(
            format!(
                "{} HEAD\0{}\n",
                default_tip.as_deref().unwrap_or(ZERO_OID),
                caps
            )
            .into_bytes(),
        ));
        for r in &refs {
            lines.push(Pkt::Data(format!("{} {}\n", r.oid, r.name).into_bytes()));
            if let Some(peeled) = &r.peeled {
                lines.push(Pkt::Data(
                    format!("{} {}^{{}}\n", peeled, r.name).into_bytes(),
                ));
            }
        }
    }
    lines.push(Pkt::Flush);
    let out = encode_pkt_lines(&lines);
    if tip.is_some()
        && refs.len() == 1
        && refs[0].name == "refs/heads/main"
        && refs[0].peeled.is_none()
    {
        repo.adv_tip_cache = Some(AdvTipCache {
            service: service.to_string(),
            refs_key: "refs/heads/main".to_string(),
            tip: tip.unwrap(),
            buf: out.clone(),
        });
    }
    repo.cache_git(GitCacheKind::Adv, key, out.clone());
    out
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// The v2 capability advertisement never depends on the repository: one buffer.
fn v2_advertisement() -> Vec<u8> {
    static V2: OnceLock<Vec<u8>> = OnceLock::new();
    V2.get_or_init(|| {
        encode_pkt_lines(&[
            Pkt::Data(b"# service=git-upload-pack\n".to_vec()),
            Pkt::Flush,
            Pkt::Data(b"version 2\n".to_vec()),
            Pkt::Data(format!("agent={}\n", AGENT).into_bytes()),
            Pkt::Data(b"ls-refs=unborn\n".to_vec()),
            Pkt::Data(b"fetch=shallow filter\n".to_vec()),
            Pkt::Data(b"server-option\n".to_vec()),
            Pkt::Data(b"object-format=sha1\n".to_vec()),
            Pkt::Flush,
        ])
    })
    .clone()
}

// -------------------------------------------------------------------------------------------
// fetch planning
// -------------------------------------------------------------------------------------------

struct FetchRequest {
    wants: Vec<String>,
    haves: Vec<String>,
    depth: Option<u64>,
    /// `deepen-relative`: the depth counts from the current shallow boundary.
    deepen_relative: bool,
    old_shallow: Vec<String>,
}

struct FetchPlan {
    objects: HashMap<String, GitObject>,
    shallow: Vec<String>,
    unshallow: Vec<String>,
    acks: Vec<String>,
}

/// The objects the client already holds, from its `have` lines: the closure of
/// the haves, truncated at its shallow boundaries — a boundary commit's parents
/// are not held, so they must not be subtracted from what the fetch sends.
fn have_closure(repo: &Repo, haves: &[String], old_shallow: &[String]) -> HashSet<String> {
    let boundary: HashSet<&str> = old_shallow.iter().map(|s| s.as_str()).collect();
    let mut held: HashSet<String> = HashSet::new();
    let mut queue: Vec<String> = Vec::new();
    for oid in haves {
        if is_object_id(oid) && repo.objects.contains_key(oid) && !held.contains(oid) {
            held.insert(oid.clone());
            queue.push(oid.clone());
        }
    }
    let mut head = 0;
    while head < queue.len() {
        let oid = queue[head].clone();
        head += 1;
        let object = match repo.objects.get(&oid) {
            Some(object) => object,
            None => continue,
        };
        if object.kind == ObjKind::Commit {
            let commit = parse_commit(&object.data);
            for child in collect_reachable(repo, &[commit.tree.clone()]) {
                held.insert(child);
            }
            if boundary.contains(oid.as_str()) {
                continue; // shallow boundary: parents are not held
            }
            for parent in commit.parents {
                if !held.contains(&parent) {
                    held.insert(parent.clone());
                    queue.push(parent);
                }
            }
        } else if object.kind == ObjKind::Tag {
            if let Some(target) = parse_tag_target(&object.data) {
                if !held.contains(&target) {
                    held.insert(target.clone());
                    queue.push(target);
                }
            }
        }
    }
    held
}

/// Work out what a fetch sends: the reachable closure of the wants, minus what
/// the client already holds, honouring a depth limit (shallow) with its
/// boundary list. `old_shallow` is the client's current shallow boundary.
fn plan_fetch(repo: &Repo, request: &FetchRequest) -> FetchPlan {
    let wants: Vec<String> = request
        .wants
        .iter()
        .filter(|oid| is_object_id(oid) && repo.objects.contains_key(*oid))
        .cloned()
        .collect();
    let haves: Vec<String> = request
        .haves
        .iter()
        .filter(|oid| is_object_id(oid) && repo.objects.contains_key(*oid))
        .cloned()
        .collect();
    let held = have_closure(repo, &haves, &request.old_shallow);

    // The effective depth: an absolute `deepen N`, or, with `deepen-relative`,
    // N commits beyond the want's current shallow boundary.
    let mut depth = request.depth;
    if let Some(d) = depth {
        if request.deepen_relative {
            let boundary: HashSet<&str> =
                request.old_shallow.iter().map(|s| s.as_str()).collect();
            let mut best: u64 = 0;
            for want in &wants {
                // BFS distance (1-based) from the want to its nearest old boundary.
                let mut seen: HashSet<String> = HashSet::new();
                seen.insert(want.clone());
                let mut queue: Vec<(String, u64)> = vec![(want.clone(), 1)];
                let mut qhead = 0;
                while qhead < queue.len() {
                    let (oid, dist) = queue[qhead].clone();
                    qhead += 1;
                    if boundary.contains(oid.as_str()) {
                        best = best.max(dist + d);
                        break;
                    }
                    let object = match repo.objects.get(&oid) {
                        Some(object) => object,
                        None => continue,
                    };
                    if object.kind != ObjKind::Commit {
                        continue;
                    }
                    for parent in parse_commit(&object.data).parents {
                        if !seen.contains(&parent) {
                            seen.insert(parent.clone());
                            queue.push((parent, dist + 1));
                        }
                    }
                }
            }
            if best > 0 {
                depth = Some(best);
            }
        }
    }

    let mut included: HashSet<String> = HashSet::new();
    let mut boundaries: Vec<String> = Vec::new();
    match depth {
        None => {
            for oid in collect_reachable(repo, &wants) {
                included.insert(oid);
            }
        }
        Some(depth) => {
            // BFS over commit parents: distance 1 is the want itself. Only
            // commits within the depth and their trees are included; parents
            // past a boundary commit are never reached.
            let mut distance: HashMap<String, u64> = HashMap::new();
            let mut queue: Vec<String> = Vec::new();
            for want in &wants {
                if !distance.contains_key(want) {
                    distance.insert(want.clone(), 1);
                    queue.push(want.clone());
                }
            }
            let mut commit_set: HashSet<String> = HashSet::new();
            let mut qhead = 0;
            while qhead < queue.len() {
                let oid = queue[qhead].clone();
                qhead += 1;
                let dist = distance[&oid];
                let object = match repo.objects.get(&oid) {
                    Some(object) => object,
                    None => continue,
                };
                commit_set.insert(oid.clone());
                if object.kind == ObjKind::Tag {
                    if let Some(target) = parse_tag_target(&object.data) {
                        if !distance.contains_key(&target) {
                            distance.insert(target.clone(), dist);
                            queue.push(target);
                        }
                    }
                    continue;
                }
                if object.kind != ObjKind::Commit {
                    continue;
                }
                if dist == depth {
                    boundaries.push(oid);
                    continue;
                }
                for parent in parse_commit(&object.data).parents {
                    if !distance.contains_key(&parent) {
                        distance.insert(parent.clone(), dist + 1);
                        queue.push(parent);
                    }
                }
            }
            // Each included commit contributes itself and its tree closure only.
            for oid in &commit_set {
                included.insert(oid.clone());
                if let Some(object) = repo.objects.get(oid) {
                    if object.kind == ObjKind::Commit {
                        let tree = parse_commit(&object.data).tree;
                        for child in collect_reachable(repo, &[tree]) {
                            included.insert(child);
                        }
                    }
                }
            }
            // Tags elsewhere in the repo that point at an included commit.
            for (oid, object) in &repo.objects {
                if object.kind != ObjKind::Tag {
                    continue;
                }
                if let Some(target) = parse_tag_target(&object.data) {
                    if commit_set.contains(&target) {
                        included.insert(oid.clone());
                    }
                }
            }
        }
    }

    let mut objects: HashMap<String, GitObject> = HashMap::new();
    for oid in &included {
        if held.contains(oid) {
            continue;
        }
        if let Some(object) = repo.objects.get(oid) {
            objects.insert(oid.clone(), object.clone());
        }
    }

    // Shallow-info: new boundaries, surviving old boundaries, deepened old ones.
    let mut shallow: Vec<String> = Vec::new();
    let mut unshallow: Vec<String> = Vec::new();
    if request.depth.is_some() {
        let boundary_set: HashSet<&str> = boundaries.iter().map(|s| s.as_str()).collect();
        for oid in &boundaries {
            shallow.push(oid.clone());
        }
        for old in &request.old_shallow {
            if !is_object_id(old) {
                continue;
            }
            if boundary_set.contains(old.as_str()) {
                continue;
            }
            // Deepened: the old boundary now lies strictly inside the new
            // depth, so its parents are delivered and it is no longer a
            // boundary. (The boundary commit itself is always held by the
            // client; that is what makes it a boundary, so holding it does
            // not block unshallowing.)
            let deepened = included.contains(old) && !boundary_set.contains(old.as_str());
            if deepened {
                if !unshallow.contains(old) {
                    unshallow.push(old.clone());
                }
                continue;
            }
            // Still a boundary when it is an ancestor of a want and its
            // parents were not delivered.
            if wants.iter().any(|want| is_ancestor(repo, old, want))
                && !shallow.contains(old)
            {
                shallow.push(old.clone());
            }
        }
    }

    let mut acks: Vec<String> = Vec::new();
    for have in &haves {
        if wants.iter().any(|want| is_ancestor(repo, have, want)) {
            acks.push(have.clone());
        }
    }
    FetchPlan {
        objects,
        shallow,
        unshallow,
        acks,
    }
}

/// Split pack bytes into sideband band-1 pkt-lines (65515 data bytes each).
fn sideband_pack(pack: &[u8]) -> Vec<Vec<u8>> {
    const CHUNK: usize = 65515;
    let chunks = ((pack.len() + CHUNK - 1) / CHUNK).max(1);
    let mut parts: Vec<Vec<u8>> = Vec::with_capacity(chunks);
    for i in 0..chunks {
        let offset = i * CHUNK;
        let len = CHUNK.min(pack.len() - offset);
        let mut out = Vec::with_capacity(4 + 1 + len);
        out.extend_from_slice(format!("{:04x}", 1 + len + 4).as_bytes());
        out.push(1);
        out.extend_from_slice(&pack[offset..offset + len]);
        parts.push(out);
    }
    parts
}

/// Cache key for a fetch response: the request plus the git fingerprint.
fn fetch_cache_key(repo: &mut Repo, request: &FetchRequest, version: &str) -> String {
    format!(
        "{}:{}:[{},{},{},{},{}]",
        version,
        repo.git_fingerprint(),
        json_string_list(&request.wants),
        json_string_list(&request.haves),
        match request.depth {
            Some(d) => d.to_string(),
            None => "null".to_string(),
        },
        request.deepen_relative,
        json_string_list(&request.old_shallow),
    )
}

fn sorted_pack(objects: &HashMap<String, GitObject>) -> Vec<u8> {
    let mut entries: Vec<(String, GitObject)> = objects
        .iter()
        .map(|(oid, object)| (oid.clone(), object.clone()))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    write_pack(&entries)
}

fn v2_fetch_response(repo: &mut Repo, request: &FetchRequest) -> Vec<u8> {
    let key = fetch_cache_key(repo, request, "v2");
    if let Some(cached) = repo.fetch_cache.get(&key) {
        return cached.clone();
    }
    let out = build_v2_fetch_response(repo, request);
    repo.cache_git(GitCacheKind::Fetch, key, out.clone());
    out
}

fn build_v2_fetch_response(repo: &Repo, request: &FetchRequest) -> Vec<u8> {
    let plan = plan_fetch(repo, request);
    let ordered_pack = sorted_pack(&plan.objects);
    let mut parts: Vec<Vec<u8>> = Vec::new();
    // Real git reads the acknowledgments section only when the request carried
    // haves to negotiate: with no haves it reads the packfile section header
    // first, so an acknowledgments section here would be read as the section
    // header and die with "expected 'packfile'". The kit client accepts both
    // shapes (it treats a missing acknowledgments section as NAK).
    if !request.haves.is_empty() {
        let mut ack_lines: Vec<Pkt> = vec![Pkt::Data(b"acknowledgments\n".to_vec())];
        if !plan.acks.is_empty() {
            for ack in &plan.acks {
                ack_lines.push(Pkt::Data(format!("ACK {}\n", ack).into_bytes()));
            }
            ack_lines.push(Pkt::Data(b"ready\n".to_vec()));
        } else {
            ack_lines.push(Pkt::Data(b"NAK\n".to_vec()));
        }
        ack_lines.push(Pkt::Delim);
        parts.push(encode_pkt_lines(&ack_lines));
    }
    if request.depth.is_some() {
        let mut shallow_lines: Vec<Pkt> = vec![Pkt::Data(b"shallow-info\n".to_vec())];
        for oid in &plan.shallow {
            shallow_lines.push(Pkt::Data(format!("shallow {}\n", oid).into_bytes()));
        }
        for oid in &plan.unshallow {
            shallow_lines.push(Pkt::Data(format!("unshallow {}\n", oid).into_bytes()));
        }
        shallow_lines.push(Pkt::Delim);
        parts.push(encode_pkt_lines(&shallow_lines));
    }
    parts.push(encode_pkt_line(b"packfile\n"));
    for chunk in sideband_pack(&ordered_pack) {
        parts.push(chunk);
    }
    parts.push(encode_pkt_lines(&[Pkt::Flush]));
    parts.concat()
}

fn v0_fetch_response(repo: &mut Repo, request: &FetchRequest) -> Vec<u8> {
    let key = fetch_cache_key(repo, request, "v0");
    if let Some(cached) = repo.fetch_cache.get(&key) {
        return cached.clone();
    }
    let out = build_v0_fetch_response(repo, request);
    repo.cache_git(GitCacheKind::Fetch, key, out.clone());
    out
}

fn build_v0_fetch_response(repo: &Repo, request: &FetchRequest) -> Vec<u8> {
    let plan = plan_fetch(repo, request);
    let pack = sorted_pack(&plan.objects);
    let mut lines: Vec<Pkt> = Vec::new();
    // v0 knows no `ready` line: only ACK lines or NAK.
    if !plan.acks.is_empty() {
        for ack in &plan.acks {
            lines.push(Pkt::Data(format!("ACK {}\n", ack).into_bytes()));
        }
    } else {
        lines.push(Pkt::Data(b"NAK\n".to_vec()));
    }
    if request.depth.is_some() {
        for oid in &plan.shallow {
            lines.push(Pkt::Data(format!("shallow {}\n", oid).into_bytes()));
        }
        for oid in &plan.unshallow {
            lines.push(Pkt::Data(format!("unshallow {}\n", oid).into_bytes()));
        }
    }
    let mut parts: Vec<Vec<u8>> = vec![encode_pkt_lines(&lines)];
    for chunk in sideband_pack(&pack) {
        parts.push(chunk);
    }
    parts.push(encode_pkt_lines(&[Pkt::Flush]));
    parts.concat()
}

fn parse_fetch_args(lines: &[String]) -> FetchRequest {
    let mut request = FetchRequest {
        wants: Vec::new(),
        haves: Vec::new(),
        depth: None,
        deepen_relative: false,
        old_shallow: Vec::new(),
    };
    for line in lines {
        let text = line.strip_suffix('\n').unwrap_or(line);
        if let Some(rest) = text.strip_prefix("want ") {
            if let Some(first) = rest.split(' ').next() {
                request.wants.push(first.to_string());
            }
        } else if let Some(rest) = text.strip_prefix("have ") {
            request.haves.push(rest.trim().to_string());
        } else if let Some(rest) = text.strip_prefix("shallow ") {
            request.old_shallow.push(rest.trim().to_string());
        } else if text == "deepen-relative" {
            request.deepen_relative = true;
        } else if let Some(rest) = text.strip_prefix("deepen ") {
            if let Ok(depth) = rest.trim().parse::<i64>() {
                if depth > 0 {
                    request.depth = Some(depth as u64);
                }
            }
        }
    }
    request
}

fn handle_ls_refs(repo: &mut Repo, lines: &[String]) -> Vec<u8> {
    let key = format!("ls:{}:{}", repo.git_fingerprint(), json_string_list(lines));
    if let Some(cached) = repo.ls_refs_cache.get(&key) {
        return cached.clone();
    }
    let out = build_ls_refs(repo, lines);
    repo.cache_git(GitCacheKind::LsRefs, key, out.clone());
    out
}

fn build_ls_refs(repo: &Repo, lines: &[String]) -> Vec<u8> {
    let args: Vec<&str> = lines
        .iter()
        .map(|line| line.strip_suffix('\n').unwrap_or(line))
        .collect();
    let peel = args.contains(&"peel");
    let symrefs = args.contains(&"symrefs");
    let mut prefixes: Vec<&str> = Vec::new();
    for arg in &args {
        if let Some(prefix) = arg.strip_prefix("ref-prefix ") {
            prefixes.push(prefix);
        }
    }
    let keep = |name: &str| prefixes.is_empty() || prefixes.iter().any(|p| name.starts_with(p));
    let mut out: Vec<Pkt> = Vec::new();
    let default_tip = repo.default_tip();
    if default_tip.is_some() && keep("HEAD") {
        let symref = if symrefs {
            " symref-target:refs/heads/main"
        } else {
            ""
        };
        out.push(Pkt::Data(
            format!("{} HEAD{}\n", default_tip.unwrap(), symref).into_bytes(),
        ));
    }
    for r in advertised_refs(repo) {
        if !keep(&r.name) {
            continue;
        }
        let peeled = if peel {
            match &r.peeled {
                Some(p) => format!(" peeled:{}", p),
                None => String::new(),
            }
        } else {
            String::new()
        };
        out.push(Pkt::Data(
            format!("{} {}{}\n", r.oid, r.name, peeled).into_bytes(),
        ));
    }
    out.push(Pkt::Flush);
    encode_pkt_lines(&out)
}

// -------------------------------------------------------------------------------------------
// receive-pack
// -------------------------------------------------------------------------------------------

struct PushCommand {
    old_oid: String,
    new_oid: String,
    ref_name: String,
}

/// Parse the receive-pack command list: pkt-lines up to the first flush packet.
/// The pack follows the flush as raw binary, so the body must not be decoded
/// as pkt-lines past it.
fn parse_receive_pack_commands(body: &[u8]) -> Result<(Vec<PushCommand>, usize), String> {
    let mut commands: Vec<PushCommand> = Vec::new();
    let mut offset = 0;
    loop {
        if offset + 4 > body.len() {
            return Err("truncated pkt-line stream".to_string());
        }
        for i in 0..4 {
            if !is_hex_byte(body[offset + i]) {
                return Err("bad pkt-line length".to_string());
            }
        }
        let header = std::str::from_utf8(&body[offset..offset + 4])
            .map_err(|_| "bad pkt-line length".to_string())?;
        if header == "0000" {
            offset += 4;
            break;
        }
        let length = usize::from_str_radix(header, 16)
            .map_err(|_| format!("bad pkt-line length {:?}", header))?;
        if length < 4 || offset + length > body.len() {
            return Err(format!("bad pkt-line length {:?}", header));
        }
        let payload = &body[offset + 4..offset + length];
        let text = String::from_utf8_lossy(payload);
        let nul = text.find('\0');
        let head = match nul {
            Some(idx) => &text[..idx],
            None => &text,
        };
        // `<40-hex> <40-hex> <ref>\n?` parsed by hand: two spaces at fixed offsets.
        let head_bytes = head.as_bytes();
        if head_bytes.len() < 83 || head_bytes[40] != b' ' || head_bytes[81] != b' ' {
            return Err(format!(
                "bad receive-pack command line {:?}",
                &head[..head.len().min(60)]
            ));
        }
        let old_oid = head[..40].to_string();
        let new_oid = head[41..81].to_string();
        let ref_name = head[82..].strip_suffix('\n').unwrap_or(&head[82..]).to_string();
        if ref_name.is_empty() || !is_hex40(&old_oid) || !is_hex40(&new_oid) || !is_ref_name(&ref_name)
        {
            return Err(format!(
                "bad receive-pack command line {:?}",
                &head[..head.len().min(60)]
            ));
        }
        commands.push(PushCommand {
            old_oid,
            new_oid,
            ref_name,
        });
        offset += length;
    }
    Ok((commands, offset))
}

/// Fully sideband-framed report-status buffers, keyed by the report's lines:
/// the push workload answers the same single-ok-ref report many times in a
/// row, so rebuilding its pkt-lines + sideband framing per push is pure waste.
/// The buffer is never mutated after it is built, so sharing it across
/// requests is safe. Capped like the other git caches.
fn receive_report_cache() -> &'static Mutex<HashMap<String, Vec<u8>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Vec<u8>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn framed_receive_report(
    unpack_error: Option<&str>,
    results: &[(String, bool, String)],
) -> Vec<u8> {
    let mut key = match unpack_error {
        Some(err) => format!("unpack {}\n", err),
        None => "unpack ok\n".to_string(),
    };
    for (ref_name, ok, reason) in results {
        if *ok {
            key.push_str(&format!("ok {}\n", ref_name));
        } else {
            key.push_str(&format!("ng {} {}\n", ref_name, reason));
        }
    }
    {
        let cache = receive_report_cache().lock().unwrap();
        if let Some(cached) = cache.get(&key) {
            return cached.clone();
        }
    }
    let mut report_lines: Vec<Pkt> = vec![Pkt::Data(
        match unpack_error {
            Some(err) => format!("unpack {}\n", err),
            None => "unpack ok\n".to_string(),
        }
        .into_bytes(),
    )];
    for (ref_name, ok, reason) in results {
        if *ok {
            report_lines.push(Pkt::Data(format!("ok {}\n", ref_name).into_bytes()));
        } else {
            report_lines.push(Pkt::Data(
                format!("ng {} {}\n", ref_name, reason).into_bytes(),
            ));
        }
    }
    report_lines.push(Pkt::Flush);
    let report = encode_pkt_lines(&report_lines);
    // Sideband band 1 around the report, one allocation per chunk.
    let mut chunks: Vec<Vec<u8>> = sideband_pack(&report);
    // The sideband stream ends with a flush packet of its own: without it a
    // real git client reports a sideband demultiplexer error.
    chunks.push(encode_pkt_lines(&[Pkt::Flush]));
    let out = chunks.concat();
    let mut cache = receive_report_cache().lock().unwrap();
    if cache.len() >= 64 {
        cache.clear();
    }
    cache.insert(key, out.clone());
    out
}

fn handle_receive_pack(repo: &mut Repo, body: &[u8]) -> Result<Vec<u8>, String> {
    let (commands, pack_start) = parse_receive_pack_commands(body)?;
    if commands.is_empty() {
        return Err("receive-pack sent no commands".to_string());
    }

    let pack_bytes = &body[pack_start..];
    let mut unpack_error: Option<String> = None;
    if !pack_bytes.is_empty() {
        let lookup = |oid: &str| repo.objects.get(oid).cloned();
        match read_pack(pack_bytes, &lookup) {
            Ok(read) => {
                for (oid, object) in read {
                    repo.add_object(oid, object);
                }
            }
            Err(err) => unpack_error = Some(err),
        }
    }

    let mut results: Vec<(String, bool, String)> = Vec::new();
    if unpack_error.is_some() {
        for command in &commands {
            results.push((command.ref_name.clone(), false, "unpack failed".to_string()));
        }
    } else {
        for command in &commands {
            let current = repo.refs.get(&command.ref_name).cloned();
            if command.new_oid == ZERO_OID {
                // Delete.
                match current {
                    None => results.push((
                        command.ref_name.clone(),
                        false,
                        "ref does not exist".to_string(),
                    )),
                    Some(current) if current != command.old_oid => results.push((
                        command.ref_name.clone(),
                        false,
                        "old oid mismatch".to_string(),
                    )),
                    Some(_) => {
                        repo.delete_ref(&command.ref_name);
                        results.push((command.ref_name.clone(), true, String::new()));
                    }
                }
                continue;
            }
            if command.old_oid == ZERO_OID {
                // Create.
                if current.is_some() {
                    results.push((
                        command.ref_name.clone(),
                        false,
                        "ref already exists".to_string(),
                    ));
                    continue;
                }
            } else {
                match current {
                    None => {
                        results.push((
                            command.ref_name.clone(),
                            false,
                            "ref does not exist".to_string(),
                        ));
                        continue;
                    }
                    Some(current) if current != command.old_oid => {
                        results.push((
                            command.ref_name.clone(),
                            false,
                            "old oid mismatch".to_string(),
                        ));
                        continue;
                    }
                    _ => {}
                }
            }
            let target = repo.objects.get(&command.new_oid);
            if target.is_none() {
                results.push((
                    command.ref_name.clone(),
                    false,
                    "object not found".to_string(),
                ));
                continue;
            }
            if command.old_oid != ZERO_OID && command.ref_name.starts_with("refs/heads/") {
                let current = current.unwrap();
                if !is_ancestor(repo, &current, &command.new_oid) {
                    results.push((
                        command.ref_name.clone(),
                        false,
                        "non-fast-forward".to_string(),
                    ));
                    continue;
                }
            }
            repo.set_ref(&command.ref_name, &command.new_oid);
            results.push((command.ref_name.clone(), true, String::new()));
        }
    }

    Ok(framed_receive_report(
        unpack_error.as_deref(),
        &results,
    ))
}

// -------------------------------------------------------------------------------------------
// router
// -------------------------------------------------------------------------------------------

/// Handle one git smart-HTTP request. Returns None when the request is not a
/// git route (the caller tries the next handler).
pub fn handle_git(
    store: &mut Store,
    req: &Request,
    segments: &[String],
) -> Option<Response> {
    if segments.len() < 2 {
        return None;
    }
    let owner = segments[0].clone();
    let repo_segment = segments[1].clone();
    if !repo_segment.ends_with(".git") {
        return None;
    }
    let repo_name = repo_segment[..repo_segment.len() - 4].to_string();
    let method = req.method.as_str();
    let rest = &segments[2..];

    // info/refs
    if method == "GET" && rest.len() == 2 && rest[0] == "info" && rest[1] == "refs" {
        let service = query_get(req, "service");
        let service = match service.as_deref() {
            Some("git-upload-pack") => "git-upload-pack",
            Some("git-receive-pack") => "git-receive-pack",
            _ => return Some(error_response(store, 400, "unknown service")),
        };
        let repo_exists = store.get_repo(&owner, &repo_name).is_some();
        if !repo_exists || owner != VIEWER.login {
            return Some(error_response(store, 404, "Not Found"));
        }
        let want_v2 =
            service == "git-upload-pack" && req_header(req, "git-protocol") == Some("version=2");
        let body = if want_v2 {
            v2_advertisement()
        } else {
            let repo = store.get_repo(&owner, &repo_name).unwrap();
            v0_advertisement(service, repo)
        };
        let content_type = format!("application/x-{}-advertisement", service);
        return Some(raw_response(store, 200, body, &content_type));
    }

    // upload-pack / receive-pack POST
    if method == "POST"
        && rest.len() == 1
        && (rest[0] == "git-upload-pack" || rest[0] == "git-receive-pack")
    {
        let service = rest[0].clone();
        let repo_exists = store.get_repo(&owner, &repo_name).is_some();
        if !repo_exists || owner != VIEWER.login {
            return Some(error_response(store, 404, "Not Found"));
        }
        let body = req.body.clone();
        if service == "git-receive-pack" {
            let outcome = {
                let repo = store.get_repo(&owner, &repo_name).unwrap();
                handle_receive_pack(repo, &body)
            };
            return Some(match outcome {
                Ok(report) => raw_response(
                    store,
                    200,
                    report,
                    "application/x-git-receive-pack-result",
                ),
                Err(err) => plain_error(400, &err),
            });
        }
        // upload-pack: v2 when the client asked for it, else v0.
        let decoded = match decode_pkt_lines(&body) {
            Some(packets) => packets,
            None => return Some(plain_error(400, "malformed pkt-line stream")),
        };
        let texts: Vec<String> = decoded
            .0
            .iter()
            .filter_map(|packet| match packet {
                Pkt::Data(payload) => Some(String::from_utf8_lossy(payload).into_owned()),
                _ => None,
            })
            .collect();
        let is_v2 = req_header(req, "git-protocol") == Some("version=2");
        if is_v2 {
            let command = texts
                .first()
                .map(|t| t.strip_suffix('\n').unwrap_or(t).to_string())
                .unwrap_or_default();
            if command == "command=ls-refs" {
                let out = {
                    let repo = store.get_repo(&owner, &repo_name).unwrap();
                    handle_ls_refs(repo, &texts[1..])
                };
                return Some(raw_response(
                    store,
                    200,
                    out,
                    "application/x-git-upload-pack-result",
                ));
            }
            if command == "command=fetch" {
                let request = parse_fetch_args(&texts[1..]);
                let out = {
                    let repo = store.get_repo(&owner, &repo_name).unwrap();
                    v2_fetch_response(repo, &request)
                };
                return Some(raw_response(
                    store,
                    200,
                    out,
                    "application/x-git-upload-pack-result",
                ));
            }
            return Some(plain_error(
                400,
                &format!("unknown v2 command {}", json_quote(&command)),
            ));
        }
        let request = parse_fetch_args(&texts);
        let out = {
            let repo = store.get_repo(&owner, &repo_name).unwrap();
            v0_fetch_response(repo, &request)
        };
        return Some(raw_response(
            store,
            200,
            out,
            "application/x-git-upload-pack-result",
        ));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::ObjKind;
    use crate::pktline::encode_pkt_lines;

    fn test_store() -> Store {
        let mut store = Store::new();
        let repo = store.create_repo("adaiversen", "demo", "demo", false);
        repo.commit_file("main", "hello.txt", b"hello\n", "add hello");
        store
    }

    fn segments(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| p.to_string()).collect()
    }

    fn get_req(query: &str, headers: Vec<(String, String)>) -> Request {
        Request {
            method: "GET".to_string(),
            path: "/adaiversen/demo.git/info/refs".to_string(),
            query: query.to_string(),
            headers,
            body: Vec::new(),
        }
    }

    #[test]
    fn info_refs_v0_advertises_head_and_ref() {
        let mut store = test_store();
        let tip = store
            .get_repo("adaiversen", "demo")
            .unwrap()
            .default_tip()
            .unwrap();
        let req = get_req("service=git-upload-pack", vec![]);
        let res = handle_git(
            &mut store,
            &req,
            &segments(&["adaiversen", "demo.git", "info", "refs"]),
        )
        .unwrap();
        assert_eq!(res.status, 200);
        let body = String::from_utf8_lossy(&res.body);
        assert!(body.contains("# service=git-upload-pack\n"));
        assert!(body.contains(&format!("{} HEAD", tip)));
        assert!(body.contains("refs/heads/main"));
        assert!(body.contains("side-band-64k"));
        assert_eq!(
            res.headers
                .iter()
                .find(|(k, _)| k == "content-type")
                .map(|(_, v)| v.as_str()),
            Some("application/x-git-upload-pack-advertisement")
        );
    }

    #[test]
    fn info_refs_v2_advertisement() {
        let mut store = test_store();
        let req = get_req(
            "service=git-upload-pack",
            vec![("git-protocol".to_string(), "version=2".to_string())],
        );
        let res = handle_git(
            &mut store,
            &req,
            &segments(&["adaiversen", "demo.git", "info", "refs"]),
        )
        .unwrap();
        assert_eq!(res.status, 200);
        let body = String::from_utf8_lossy(&res.body);
        assert!(body.contains("version 2\n"));
        assert!(body.contains("ls-refs=unborn\n"));
    }

    #[test]
    fn info_refs_unknown_service_and_owner_are_errors() {
        let mut store = test_store();
        let req = get_req("service=git-nope", vec![]);
        let res = handle_git(
            &mut store,
            &req,
            &segments(&["adaiversen", "demo.git", "info", "refs"]),
        )
        .unwrap();
        assert_eq!(res.status, 400);
        let req = get_req("service=git-upload-pack", vec![]);
        let res = handle_git(
            &mut store,
            &req,
            &segments(&["someone", "demo.git", "info", "refs"]),
        )
        .unwrap();
        assert_eq!(res.status, 404);
    }

    #[test]
    fn v2_ls_refs_lists_head() {
        let mut store = test_store();
        let tip = store
            .get_repo("adaiversen", "demo")
            .unwrap()
            .default_tip()
            .unwrap();
        let body = encode_pkt_lines(&[
            Pkt::Data(b"command=ls-refs\n".to_vec()),
            Pkt::Data(b"peel\n".to_vec()),
            Pkt::Flush,
        ]);
        let req = Request {
            method: "POST".to_string(),
            path: "/adaiversen/demo.git/git-upload-pack".to_string(),
            query: String::new(),
            headers: vec![("git-protocol".to_string(), "version=2".to_string())],
            body,
        };
        let res = handle_git(
            &mut store,
            &req,
            &segments(&["adaiversen", "demo.git", "git-upload-pack"]),
        )
        .unwrap();
        assert_eq!(res.status, 200);
        let text = String::from_utf8_lossy(&res.body);
        assert!(text.contains(&format!("{} HEAD", tip)));
        assert!(text.contains("refs/heads/main"));
    }

    #[test]
    fn v0_fetch_returns_pack_with_wanted_objects() {
        let mut store = test_store();
        let tip = store
            .get_repo("adaiversen", "demo")
            .unwrap()
            .default_tip()
            .unwrap();
        let body = encode_pkt_lines(&[
            Pkt::Data(format!("want {}\n", tip).into_bytes()),
            Pkt::Data(b"done\n".to_vec()),
            Pkt::Flush,
        ]);
        let req = Request {
            method: "POST".to_string(),
            path: "/adaiversen/demo.git/git-upload-pack".to_string(),
            query: String::new(),
            headers: vec![],
            body,
        };
        let res = handle_git(
            &mut store,
            &req,
            &segments(&["adaiversen", "demo.git", "git-upload-pack"]),
        )
        .unwrap();
        assert_eq!(res.status, 200);
        // NAK first (no haves), then a sideband-framed pack starting with PACK.
        let text = String::from_utf8_lossy(&res.body);
        assert!(text.contains("NAK\n"));
        assert!(res.body.windows(4).any(|w| w == b"PACK"));
    }

    #[test]
    fn receive_pack_creates_a_branch() {
        let mut store = test_store();
        // Build a pack holding the existing objects, then push it as a new ref.
        let (tip, objects) = {
            let repo = store.get_repo("adaiversen", "demo").unwrap();
            let tip = repo.default_tip().unwrap();
            let mut objects: Vec<(String, GitObject)> = Vec::new();
            let mut seen: HashSet<String> = HashSet::new();
            let mut stack = vec![tip.clone()];
            while let Some(oid) = stack.pop() {
                if !seen.insert(oid.clone()) {
                    continue;
                }
                let object = repo.objects.get(&oid).unwrap().clone();
                if object.kind == ObjKind::Commit {
                    let commit = parse_commit(&object.data);
                    stack.push(commit.tree);
                    for parent in commit.parents {
                        stack.push(parent);
                    }
                } else if object.kind == ObjKind::Tree {
                    for entry in parse_tree(&object.data) {
                        stack.push(entry.oid);
                    }
                }
                objects.push((oid, object));
            }
            (tip, objects)
        };
        let pack = write_pack(&objects);
        let mut body = encode_pkt_lines(&[
            Pkt::Data(
                format!(
                    "{} {} refs/heads/feature\n",
                    ZERO_OID, tip
                )
                .into_bytes(),
            ),
            Pkt::Flush,
        ]);
        body.extend_from_slice(&pack);
        let req = Request {
            method: "POST".to_string(),
            path: "/adaiversen/demo.git/git-receive-pack".to_string(),
            query: String::new(),
            headers: vec![],
            body,
        };
        let res = handle_git(
            &mut store,
            &req,
            &segments(&["adaiversen", "demo.git", "git-receive-pack"]),
        )
        .unwrap();
        assert_eq!(res.status, 200);
        let text = String::from_utf8_lossy(&res.body);
        assert!(text.contains("unpack ok\n"));
        assert!(text.contains("ok refs/heads/feature\n"));
        let repo = store.get_repo("adaiversen", "demo").unwrap();
        assert_eq!(repo.branch_tip("feature"), Some(tip));
    }

    #[test]
    fn non_git_paths_return_none() {
        let mut store = test_store();
        let req = get_req("", vec![]);
        assert!(handle_git(&mut store, &req, &segments(&["adaiversen", "demo"]))
            .is_none());
        assert!(handle_git(
            &mut store,
            &req,
            &segments(&["adaiversen", "demo.git", "other"])
        )
        .is_none());
    }
}
