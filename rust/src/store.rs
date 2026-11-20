// store.rs — the forge's in-memory state: repositories, git objects and refs,
// issues, pull requests, labels and comments.
//
// One process, one store, no persistence: the trial starts a fresh server and
// the suite creates everything it needs through the API. All ids are counters
// and all timestamps are fixed constants, so the same requests in the same
// order always leave the same state.
//
// Ported from store.ts (same behaviour, Rust idioms). Depends on the object
// module for hashing, commit/tree parsing and building, and merges.

use std::collections::HashMap;

use crate::object::{
    build_commit, build_nested_tree, flatten_tree, hash_object, is_object_id, merge_base,
    parse_commit, parse_tree, three_way_merge, GitObject, ObjKind, Oid,
};

/// The login the bearer token names: the seeded scenario's viewer.
pub struct Viewer {
    pub id: u64,
    pub login: &'static str,
    pub name: &'static str,
    pub email: &'static str,
}

pub const VIEWER: Viewer = Viewer {
    id: 1000,
    login: "adaiversen",
    name: "ada iversen",
    email: "adaiversen@example.invalid",
};

/// The fixed identity every commit this forge authors carries.
pub const FORGE_IDENTITY: &str = "adaiversen <adaiversen@example.invalid>";
/// The fixed commit timestamp (seconds, +0000) for every forge-authored commit.
pub const FORGE_EPOCH: u64 = 1700000000;
/// The fixed ISO timestamp for created_at / updated_at / merged_at fields.
pub const FIXED_ISO: &str = "2025-01-01T00:00:00Z";

pub fn forge_author_line() -> String {
    format!("{} {} +0000", FORGE_IDENTITY, FORGE_EPOCH)
}

// -------------------------------------------------------------------------------------------
// base64url without padding (std has no base64)
// -------------------------------------------------------------------------------------------

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn base64url_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() / 3 * 4 + 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64_ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(B64_ALPHABET[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(B64_ALPHABET[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(B64_ALPHABET[n as usize & 63] as char);
        }
    }
    out
}

fn b64_value(byte: u8) -> Option<u32> {
    match byte {
        b'A'..=b'Z' => Some((byte - b'A') as u32),
        b'a'..=b'z' => Some((byte - b'a') as u32 + 26),
        b'0'..=b'9' => Some((byte - b'0') as u32 + 52),
        b'-' | b'+' => Some(62),
        b'_' | b'/' => Some(63),
        _ => None,
    }
}

fn base64url_decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    let mut len = bytes.len();
    while len > 0 && bytes[len - 1] == b'=' {
        len -= 1;
    }
    let body = &bytes[..len];
    if body.is_empty() {
        return Some(Vec::new());
    }
    if body.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(body.len() / 4 * 3 + 3);
    for chunk in body.chunks(4) {
        let mut vals = [0u32; 4];
        for (i, &c) in chunk.iter().enumerate() {
            vals[i] = b64_value(c)?;
        }
        let triple = (vals[0] << 18) | (vals[1] << 12) | (vals[2] << 6) | vals[3];
        out.push((triple >> 16) as u8);
        if chunk.len() > 2 {
            out.push((triple >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(triple as u8);
        }
    }
    Some(out)
}

/// A base64url global (Relay) id: opaque, stable, never the plain database id.
pub fn global_id(kind: &str, key: &str) -> String {
    base64url_encode(format!("{}:{}", kind, key).as_bytes())
}

pub fn decode_global_id(id: &str) -> Option<(String, String)> {
    let bytes = base64url_decode(id)?;
    let text = String::from_utf8(bytes).ok()?;
    let colon = text.find(':')?;
    if colon == 0 {
        return None;
    }
    Some((text[..colon].to_string(), text[colon + 1..].to_string()))
}

// -------------------------------------------------------------------------------------------
// issues, pull requests, labels
// -------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Label {
    pub name: String,
    pub color: String,
}

/// Open/closed state shared by issues and pull requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Open,
    Closed,
}

impl State {
    pub fn as_str(&self) -> &'static str {
        match self {
            State::Open => "open",
            State::Closed => "closed",
        }
    }
}

impl std::fmt::Display for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug)]
pub struct Comment {
    pub id: u64,
    pub body: String,
}

#[derive(Clone, Debug)]
pub struct Issue {
    pub id: u64,
    pub number: u64,
    pub title: String,
    pub body: String,
    pub state: State,
    pub labels: Vec<Label>,
    pub created_seq: u64,
    pub comments: Vec<Comment>,
    /// Mutation counter; json_cache holds the serialised body per revision.
    pub rev: u64,
    /// Cached serialised body as bytes (avoids re-serialising per GET), or
    /// None when stale. Filled in from rest.rs through this field.
    pub json_cache: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
pub struct PullRequest {
    pub id: u64,
    pub number: u64,
    pub title: String,
    pub body: String,
    pub state: State,
    pub head_ref: String,
    pub base_ref: String,
    pub head_sha: String,
    pub base_sha: String,
    pub merged: bool,
    pub merged_at: Option<String>,
    pub merge_commit_sha: Option<String>,
    pub created_seq: u64,
}

/// Which of a repo's git response caches an entry belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitCacheKind {
    Fetch,
    LsRefs,
    Adv,
}

/// Last v0 advertisement for the tip-patch fast path (one head ref, no tags):
/// when only the tip moved, the two 40-hex occurrences of the old tip in the
/// previous advertisement buffer are patched in place instead of rebuilding.
#[derive(Clone, Debug)]
pub struct AdvTipCache {
    pub service: String,
    pub refs_key: String,
    pub tip: String,
    pub buf: Vec<u8>,
}

pub struct Repo {
    pub database_id: u64,
    pub name: String,
    pub owner: String,
    pub description: String,
    pub is_private: bool,
    pub default_branch: String,
    pub objects: HashMap<Oid, GitObject>,
    pub refs: HashMap<String, Oid>,
    pub issues: HashMap<u64, Issue>,
    pub pulls: HashMap<u64, PullRequest>,
    pub labels: HashMap<String, Label>,
    pub next_issue_number: u64,
    pub next_pull_number: u64,
    pub next_comment_id: u64,
    /// Response caches for the git surface (clone workload: many concurrent
    /// identical fetches of one unchanged repository). Every entry is keyed
    /// by the git fingerprint plus the request, so a key can only hit while
    /// the repository's refs and object set are exactly the ones the response
    /// was built from; objects are content-addressed and only ever added
    /// within a repository's life, refs are named in the fingerprint, and
    /// deleting a repository drops its Repo (and its caches) outright. Maps
    /// are capped so stale fingerprints cannot grow them without bound.
    pub fetch_cache: HashMap<String, Vec<u8>>,
    pub ls_refs_cache: HashMap<String, Vec<u8>>,
    pub adv_cache: HashMap<String, Vec<u8>>,
    /// Previous single-head v0 advertisement, for the tip-patch fast path.
    pub adv_tip_cache: Option<AdvTipCache>,
    seq: u64,
    /// The git state a cached git response was built from. Memoised and
    /// bumped by the mutators below.
    fingerprint_cache: Option<String>,
}

impl Repo {
    pub fn new(
        database_id: u64,
        owner: &str,
        name: &str,
        description: &str,
        is_private: bool,
    ) -> Repo {
        Repo {
            database_id,
            owner: owner.to_string(),
            name: name.to_string(),
            description: description.to_string(),
            is_private,
            default_branch: "main".to_string(),
            objects: HashMap::new(),
            refs: HashMap::new(),
            issues: HashMap::new(),
            pulls: HashMap::new(),
            labels: HashMap::new(),
            next_issue_number: 1,
            next_pull_number: 1,
            next_comment_id: 1,
            fetch_cache: HashMap::new(),
            ls_refs_cache: HashMap::new(),
            adv_cache: HashMap::new(),
            adv_tip_cache: None,
            seq: 0,
            fingerprint_cache: None,
        }
    }

    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    pub fn global_id(&self) -> String {
        global_id("repo", &self.full_name())
    }

    pub fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    pub fn git_fingerprint(&mut self) -> String {
        if let Some(cached) = &self.fingerprint_cache {
            return cached.clone();
        }
        let mut names: Vec<&String> = self.refs.keys().collect();
        names.sort();
        let mut out = format!("{}|", self.objects.len());
        for name in names {
            out.push_str(&format!("{}={};", name, self.refs[name]));
        }
        self.fingerprint_cache = Some(out.clone());
        out
    }

    fn bump_fingerprint(&mut self) {
        self.fingerprint_cache = None;
    }

    /// Store a cached git response, clearing the map when it grows too large.
    pub fn cache_git(&mut self, kind: GitCacheKind, key: String, value: Vec<u8>) {
        let cache = match kind {
            GitCacheKind::Fetch => &mut self.fetch_cache,
            GitCacheKind::LsRefs => &mut self.ls_refs_cache,
            GitCacheKind::Adv => &mut self.adv_cache,
        };
        if cache.len() >= 64 {
            cache.clear();
        }
        cache.insert(key, value);
    }

    pub fn put(&mut self, kind: ObjKind, data: &[u8]) -> Oid {
        let oid = hash_object(kind, data);
        if !self.objects.contains_key(&oid) {
            self.objects.insert(
                oid.clone(),
                GitObject {
                    kind,
                    data: data.to_vec(),
                },
            );
            self.bump_fingerprint();
        }
        oid
    }

    /// Store a pack object under its id (receive-pack's path into the store).
    pub fn add_object(&mut self, oid: Oid, object: GitObject) {
        if !self.objects.contains_key(&oid) {
            self.objects.insert(oid, object);
            self.bump_fingerprint();
        }
    }

    /// Set any ref (heads, tags, …).
    pub fn set_ref(&mut self, ref_name: &str, oid: &str) {
        if self.refs.get(ref_name).map(|v| v.as_str()) != Some(oid) {
            self.refs.insert(ref_name.to_string(), oid.to_string());
            self.bump_fingerprint();
        }
    }

    /// Delete any ref; false when it did not exist.
    pub fn delete_ref(&mut self, ref_name: &str) -> bool {
        let deleted = self.refs.remove(ref_name).is_some();
        if deleted {
            self.bump_fingerprint();
        }
        deleted
    }

    pub fn lookup(&self, oid: &str) -> Option<&GitObject> {
        self.objects.get(oid)
    }

    pub fn ref_tip(&self, ref_name: &str) -> Option<Oid> {
        self.refs.get(ref_name).cloned()
    }

    pub fn branch_tip(&self, branch: &str) -> Option<Oid> {
        self.refs.get(&format!("refs/heads/{}", branch)).cloned()
    }

    pub fn set_branch(&mut self, branch: &str, oid: &str) {
        self.refs
            .insert(format!("refs/heads/{}", branch), oid.to_string());
    }

    /// The default branch's tip, or None while the repository is unborn.
    pub fn default_tip(&self) -> Option<Oid> {
        self.refs
            .get(&format!("refs/heads/{}", self.default_branch))
            .cloned()
    }

    /// Resolve a branch name or a commit oid to a commit oid.
    pub fn resolve_ref(&self, ref_name: Option<&str>) -> Option<Oid> {
        match ref_name {
            None => self.default_tip(),
            Some(r) => {
                if is_object_id(r) {
                    if self.objects.contains_key(r) {
                        Some(r.to_string())
                    } else {
                        None
                    }
                } else {
                    self.branch_tip(r)
                }
            }
        }
    }

    /// Create a commit object on top of `parents` with the given tree, and store it.
    pub fn make_commit(&mut self, tree_oid: &str, parents: &[String], message: &str) -> Oid {
        let who = forge_author_line();
        let text = if message.ends_with('\n') {
            message.to_string()
        } else {
            format!("{}\n", message)
        };
        let data = build_commit(tree_oid, parents, &text, &who, &who);
        self.put(ObjKind::Commit, &data)
    }

    /// The commit object behind a tip, when the tip names a commit.
    fn tip_commit(&self, tip: &str) -> Option<(String, Vec<(String, String, String)>)> {
        let object = self.objects.get(tip)?;
        if object.kind != ObjKind::Commit {
            return None;
        }
        let info = parse_commit(&object.data);
        let tree = info.tree.clone();
        let lookup = |oid: &str| self.objects.get(oid).cloned();
        let files = flatten_tree(&info.tree, &lookup);
        Some((tree, files))
    }

    /// Commit one file change on `branch`: write the blob, rebuild the trees
    /// from the branch tip (or an empty tree when unborn), create the commit
    /// and move the branch. A branch that does not exist is created from the
    /// default tip (or as the root when the repository is unborn).
    ///
    /// Returns (commit_sha, blob_sha).
    pub fn commit_file(
        &mut self,
        branch: &str,
        path: &str,
        content: &[u8],
        message: &str,
    ) -> (Oid, Oid) {
        let blob_sha = self.put(ObjKind::Blob, content);
        let mut base_files: Vec<(String, String, String)> = Vec::new();
        let mut parents: Vec<String> = Vec::new();
        if let Some(tip) = self.branch_tip(branch) {
            if let Some((_, files)) = self.tip_commit(&tip) {
                base_files = files;
                parents = vec![tip];
            }
        }
        if parents.is_empty() && branch != self.default_branch {
            if let Some(default_tip) = self.default_tip() {
                if let Some((_, files)) = self.tip_commit(&default_tip) {
                    base_files = files;
                    parents = vec![default_tip];
                }
            }
        }
        let mut files = base_files;
        match files.iter().position(|(p, _, _)| p == path) {
            Some(idx) => files[idx] = (path.to_string(), blob_sha.clone(), "100644".to_string()),
            None => files.push((path.to_string(), blob_sha.clone(), "100644".to_string())),
        }
        files.sort();
        let tree_oid = build_nested_tree(&files, &mut |kind, data| self.put(kind, data));
        let commit_sha = self.make_commit(&tree_oid, &parents, message);
        self.set_branch(branch, &commit_sha);
        (commit_sha, blob_sha)
    }

    /// Commit the deletion of one file on `branch`.
    pub fn delete_file(&mut self, branch: &str, path: &str, message: &str) -> Result<Oid, String> {
        let tip = self.branch_tip(branch).ok_or_else(|| "branch has no tip".to_string())?;
        let (_, files) = self
            .tip_commit(&tip)
            .ok_or_else(|| "branch has no tip".to_string())?;
        let mut files: Vec<(String, String, String)> =
            files.into_iter().filter(|(p, _, _)| p != path).collect();
        files.sort();
        let tree_oid = build_nested_tree(&files, &mut |kind, data| self.put(kind, data));
        let commit_sha = self.make_commit(&tree_oid, &[tip], message);
        self.set_branch(branch, &commit_sha);
        Ok(commit_sha)
    }

    /// The blob oid of `path` at `commit_oid`, or None when absent.
    pub fn blob_at(&self, commit_oid: &str, path: &str) -> Option<Oid> {
        let object = self.objects.get(commit_oid)?;
        if object.kind != ObjKind::Commit {
            return None;
        }
        let info = parse_commit(&object.data);
        let lookup = |oid: &str| self.objects.get(oid).cloned();
        flatten_tree(&info.tree, &lookup)
            .into_iter()
            .find(|(p, _, _)| p == path)
            .map(|(_, oid, _)| oid)
    }

    /// The tree oid of `path` (a directory) at `commit_oid`, or None.
    pub fn tree_at(&self, commit_oid: &str, path: &str) -> Option<Oid> {
        let object = self.objects.get(commit_oid)?;
        if object.kind != ObjKind::Commit {
            return None;
        }
        let mut current = parse_commit(&object.data).tree;
        if path.is_empty() || path == "/" {
            return Some(current);
        }
        for segment in path.split('/').filter(|part| !part.is_empty()) {
            let tree = self.objects.get(&current)?;
            if tree.kind != ObjKind::Tree {
                return None;
            }
            let mut found: Option<Oid> = None;
            for entry in parse_tree(&tree.data) {
                if entry.name == segment && entry.mode == "40000" {
                    found = Some(entry.oid);
                }
            }
            current = found?;
        }
        Some(current)
    }
}

pub struct Store {
    pub repos: HashMap<String, Repo>,
    pub next_repo_id: u64,
    pub request_seq: u64,
}

impl Store {
    pub fn new() -> Store {
        Store {
            repos: HashMap::new(),
            next_repo_id: 1,
            request_seq: 0,
        }
    }

    pub fn next_request_id(&mut self) -> String {
        self.request_seq += 1;
        format!("req-{}", self.request_seq)
    }

    pub fn create_repo(
        &mut self,
        owner: &str,
        name: &str,
        description: &str,
        is_private: bool,
    ) -> &mut Repo {
        let repo = Repo::new(self.next_repo_id, owner, name, description, is_private);
        self.next_repo_id += 1;
        let key = format!("{}/{}", owner, name);
        self.repos.insert(key.clone(), repo);
        self.repos.get_mut(&key).unwrap()
    }

    pub fn get_repo(&mut self, owner: &str, name: &str) -> Option<&mut Repo> {
        self.repos.get_mut(&format!("{}/{}", owner, name))
    }

    /// The viewer's repositories, in creation order.
    pub fn viewer_repos(&self) -> Vec<&Repo> {
        let mut repos: Vec<&Repo> = self
            .repos
            .values()
            .filter(|repo| repo.owner == VIEWER.login)
            .collect();
        repos.sort_by_key(|repo| repo.database_id);
        repos
    }
}

/// Squash-merge a pull request: three-way merge onto the base tip, one commit.
///
/// Ok(commit_sha) on success; Err(conflicts) when the merge conflicts (or a
/// tip is missing, with an empty conflict list).
pub fn squash_merge(repo: &mut Repo, pr: &mut PullRequest) -> Result<Oid, Vec<String>> {
    let head_tip = match repo.branch_tip(&pr.head_ref) {
        Some(tip) => tip,
        None => return Err(Vec::new()),
    };
    let base_tip = match repo.branch_tip(&pr.base_ref) {
        Some(tip) => tip,
        None => return Err(Vec::new()),
    };
    let (base_tree, head_files, tip_files) = merge_inputs(repo, &base_tip, &head_tip);
    let merged = match (base_tree, head_files, tip_files) {
        (Some(b), Some(h), Some(t)) => three_way_merge(&b, &h, &t),
        _ => return Err(Vec::new()),
    };
    if !merged.conflicts.is_empty() {
        return Err(merged.conflicts);
    }
    let mut files = merged.files;
    files.sort();
    let tree_oid = build_nested_tree(&files, &mut |kind, data| repo.put(kind, data));
    let message = format!("Merge pull request #{} from {}", pr.number, pr.head_ref);
    let commit_sha = repo.make_commit(&tree_oid, &[base_tip.clone()], &message);
    repo.set_branch(&pr.base_ref, &commit_sha);
    pr.state = State::Closed;
    pr.merged = true;
    pr.merged_at = Some(FIXED_ISO.to_string());
    pr.merge_commit_sha = Some(commit_sha.clone());
    Ok(commit_sha)
}

/// Whether merging `pr` would conflict (without changing anything).
pub fn merge_would_conflict(repo: &Repo, pr: &PullRequest) -> bool {
    let head_tip = match repo.branch_tip(&pr.head_ref) {
        Some(tip) => tip,
        None => return false,
    };
    let base_tip = match repo.branch_tip(&pr.base_ref) {
        Some(tip) => tip,
        None => return false,
    };
    let (base_tree, head_files, tip_files) = merge_inputs(repo, &base_tip, &head_tip);
    match (base_tree, head_files, tip_files) {
        (Some(b), Some(h), Some(t)) => !three_way_merge(&b, &h, &t).conflicts.is_empty(),
        _ => false,
    }
}

/// Flattened file maps for a merge: (base files, head files, base-tip files).
/// Each is None when the corresponding commit cannot be read.
fn merge_inputs(
    repo: &Repo,
    base_tip: &str,
    head_tip: &str,
) -> (
    Option<Vec<(String, String, String)>>,
    Option<Vec<(String, String, String)>>,
    Option<Vec<(String, String, String)>>,
) {
    let lookup = |oid: &str| repo.objects.get(oid).cloned();
    let flatten_at = |tip: &str| -> Option<Vec<(String, String, String)>> {
        let object = repo.objects.get(tip)?;
        if object.kind != ObjKind::Commit {
            return None;
        }
        let info = parse_commit(&object.data);
        Some(flatten_tree(&info.tree, &lookup))
    };
    let base = merge_base(base_tip, head_tip, &lookup);
    let base_tree = match base {
        None => Some(Vec::new()),
        Some(base_oid) => flatten_at(&base_oid),
    };
    let head_files = flatten_at(head_tip);
    let tip_files = flatten_at(base_tip);
    (base_tree, head_files, tip_files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::ObjKind;

    fn test_repo() -> Repo {
        Repo::new(1, "adaiversen", "demo", "demo repo", false)
    }

    fn test_pr(head_ref: &str, base_ref: &str, number: u64) -> PullRequest {
        PullRequest {
            id: number,
            number,
            title: "change".to_string(),
            body: String::new(),
            state: State::Open,
            head_ref: head_ref.to_string(),
            base_ref: base_ref.to_string(),
            head_sha: String::new(),
            base_sha: String::new(),
            merged: false,
            merged_at: None,
            merge_commit_sha: None,
            created_seq: 1,
        }
    }

    #[test]
    fn global_id_roundtrips() {
        let id = global_id("repo", "adaiversen/demo");
        assert!(!id.contains('='));
        assert!(!id.contains('+'));
        assert!(!id.contains('/'));
        let (kind, key) = decode_global_id(&id).unwrap();
        assert_eq!(kind, "repo");
        assert_eq!(key, "adaiversen/demo");
        assert_eq!(decode_global_id("!!!"), None);
        assert_eq!(decode_global_id(""), None);
    }

    #[test]
    fn commit_file_creates_branch_and_blob() {
        let mut repo = test_repo();
        let (commit, blob) = repo.commit_file("main", "hello.txt", b"hello\n", "add hello");
        assert_eq!(repo.branch_tip("main"), Some(commit.clone()));
        assert_eq!(repo.default_tip(), Some(commit.clone()));
        assert_eq!(repo.blob_at(&commit, "hello.txt"), Some(blob));
        assert_eq!(repo.blob_at(&commit, "missing.txt"), None);
        assert_eq!(repo.full_name(), "adaiversen/demo");
        assert_eq!(repo.resolve_ref(Some("main")), Some(commit.clone()));
        assert_eq!(repo.resolve_ref(None), Some(commit.clone()));
        assert_eq!(repo.resolve_ref(Some(&commit)), Some(commit));
    }

    #[test]
    fn commit_file_second_commit_has_parent() {
        let mut repo = test_repo();
        let (first, _) = repo.commit_file("main", "a.txt", b"one", "first");
        let (second, _) = repo.commit_file("main", "b.txt", b"two", "second");
        let info = parse_commit(&repo.objects.get(&second).unwrap().data);
        assert_eq!(info.parents, vec![first]);
        assert!(repo.blob_at(&second, "a.txt").is_some());
        assert!(repo.blob_at(&second, "b.txt").is_some());
    }

    #[test]
    fn commit_file_new_branch_starts_from_default_tip() {
        let mut repo = test_repo();
        let (base, _) = repo.commit_file("main", "a.txt", b"base", "base");
        let (on_branch, _) = repo.commit_file("feature", "b.txt", b"feat", "feat");
        let info = parse_commit(&repo.objects.get(&on_branch).unwrap().data);
        assert_eq!(info.parents, vec![base]);
        assert!(repo.blob_at(&on_branch, "a.txt").is_some());
    }

    #[test]
    fn delete_file_removes_it() {
        let mut repo = test_repo();
        repo.commit_file("main", "a.txt", b"one", "first");
        let (second, _) = repo.commit_file("main", "b.txt", b"two", "second");
        let third = repo.delete_file("main", "a.txt", "drop a").unwrap();
        let info = parse_commit(&repo.objects.get(&third).unwrap().data);
        assert_eq!(info.parents, vec![second]);
        assert_eq!(repo.blob_at(&third, "a.txt"), None);
        assert!(repo.blob_at(&third, "b.txt").is_some());
        let mut empty = test_repo();
        assert!(empty.delete_file("main", "x.txt", "nope").is_err());
    }

    #[test]
    fn fingerprint_changes_when_ref_moves_and_is_stable() {
        let mut repo = test_repo();
        let before = repo.git_fingerprint();
        repo.commit_file("main", "a.txt", b"x", "one");
        let after = repo.git_fingerprint();
        assert_ne!(before, after);
        assert_eq!(after, repo.git_fingerprint());
        repo.put(ObjKind::Blob, b"unreferenced");
        assert_ne!(after, repo.git_fingerprint());
    }

    #[test]
    fn squash_merge_merges_cleanly() {
        let mut repo = test_repo();
        let (base, _) = repo.commit_file("main", "a.txt", b"base", "base");
        repo.set_branch("feature", &base);
        repo.commit_file("feature", "b.txt", b"feat", "feat");
        repo.commit_file("main", "c.txt", b"main2", "main2");
        let mut pr = test_pr("feature", "main", 1);
        let merged = squash_merge(&mut repo, &mut pr).unwrap();
        assert!(pr.merged);
        assert_eq!(pr.state, State::Closed);
        assert_eq!(pr.merged_at.as_deref(), Some(FIXED_ISO));
        assert_eq!(pr.merge_commit_sha.as_deref(), Some(merged.as_str()));
        assert_eq!(repo.branch_tip("main"), Some(merged.clone()));
        assert!(repo.blob_at(&merged, "a.txt").is_some());
        assert!(repo.blob_at(&merged, "b.txt").is_some());
        assert!(repo.blob_at(&merged, "c.txt").is_some());
        let info = parse_commit(&repo.objects.get(&merged).unwrap().data);
        assert_eq!(info.parents.len(), 1);
    }

    #[test]
    fn squash_merge_conflicts_when_both_sides_change_one_file() {
        let mut repo = test_repo();
        let (base, _) = repo.commit_file("main", "a.txt", b"base", "base");
        repo.set_branch("feature", &base);
        repo.commit_file("feature", "a.txt", b"feature side", "feat");
        repo.commit_file("main", "a.txt", b"main side", "main2");
        let mut pr = test_pr("feature", "main", 1);
        assert!(merge_would_conflict(&repo, &pr));
        let err = squash_merge(&mut repo, &mut pr).unwrap_err();
        assert_eq!(err, vec!["a.txt".to_string()]);
        assert!(!pr.merged);
        assert_eq!(pr.state, State::Open);
    }

    #[test]
    fn store_request_ids_and_viewer_repos() {
        let mut store = Store::new();
        assert_eq!(store.next_request_id(), "req-1");
        assert_eq!(store.next_request_id(), "req-2");
        store.create_repo("adaiversen", "one", "", false);
        store.create_repo("someone", "two", "", false);
        store.create_repo("adaiversen", "three", "", false);
        let viewer: Vec<String> = store
            .viewer_repos()
            .iter()
            .map(|r| r.name.clone())
            .collect();
        assert_eq!(viewer, vec!["one".to_string(), "three".to_string()]);
        assert!(store.get_repo("adaiversen", "one").is_some());
        assert!(store.get_repo("adaiversen", "nope").is_none());
    }
}
