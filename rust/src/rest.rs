// rest.rs — the GitHub REST API v3 surface under /api/v3.
//
// Every reply is application/json with a non-empty X-GitHub-Request-Id. A
// missing or empty Bearer token is 401; a repository name under any owner
// other than the token's viewer is 404 (never 403); a malformed JSON body
// is 400; an unknown route is a JSON 404. A duplicate repository name is
// 422. Ported from rest.ts; status codes, bodies and headers match it.

use std::collections::{HashMap, HashSet};

use crate::http::{
    error_response, header, json_bytes_response, json_response, json_response_extra, path_segments,
    query_int, query_value, Request, Response,
};
use crate::json::Json;
use crate::object::{flatten_tree, parse_commit, parse_tree, ObjKind};
use crate::store::{
    global_id, merge_would_conflict, squash_merge, Issue, Label, PullRequest, Repo, State, Store,
    FIXED_ISO, VIEWER,
};

// ---------------------------------------------------------------------------
// Small JSON construction helpers
// ---------------------------------------------------------------------------

fn s(v: &str) -> Json {
    Json::Str(v.to_string())
}

fn n(v: u64) -> Json {
    Json::Num(v as f64)
}

fn obj(pairs: Vec<(&str, Json)>) -> Json {
    Json::Obj(
        pairs
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    )
}

fn body_str(body: &Json, field: &str) -> String {
    body.get(field).and_then(|v| v.as_str()).unwrap_or("").to_string()
}

fn body_str_opt(body: &Json, field: &str) -> Option<String> {
    body.get(field)
        .and_then(|v| v.as_str())
        .map(|v| v.to_string())
}

/// Leading-integer parse like Number.parseInt(s, 10): "1x" -> 1.
/// Issue/PR numbers are never negative, so a leading '-' is not a number.
fn parse_int_prefix(text: &str) -> Option<u64> {
    let text = text.trim_start();
    if text.starts_with('-') {
        return None;
    }
    let rest = text.strip_prefix('+').unwrap_or(text);
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse::<u64>().ok()
}

// ---------------------------------------------------------------------------
// Serialisers
// ---------------------------------------------------------------------------

pub fn repo_json(repo: &Repo) -> Json {
    let open_issues = repo
        .issues
        .values()
        .filter(|issue| issue.state == State::Open)
        .count();
    obj(vec![
        ("id", n(repo.database_id)),
        ("node_id", s(&repo.global_id())),
        ("name", s(&repo.name)),
        ("full_name", s(&repo.full_name())),
        ("private", Json::Bool(repo.is_private)),
        (
            "owner",
            obj(vec![
                ("login", s(&repo.owner)),
                ("id", n(VIEWER.id)),
                ("type", s("User")),
            ]),
        ),
        ("description", s(&repo.description)),
        ("default_branch", s(&repo.default_branch)),
        ("created_at", s(FIXED_ISO)),
        ("updated_at", s(FIXED_ISO)),
        ("pushed_at", s(FIXED_ISO)),
        (
            "html_url",
            s(&format!("https://example.invalid/{}", repo.full_name())),
        ),
        ("url", s(&format!("/api/v3/repos/{}", repo.full_name()))),
        (
            "clone_url",
            s(&format!("https://example.invalid/{}.git", repo.full_name())),
        ),
        (
            "git_url",
            s(&format!("git://example.invalid/{}.git", repo.full_name())),
        ),
        (
            "ssh_url",
            s(&format!("git@example.invalid:{}.git", repo.full_name())),
        ),
        ("fork", Json::Bool(false)),
        ("archived", Json::Bool(false)),
        ("disabled", Json::Bool(false)),
        (
            "visibility",
            s(if repo.is_private { "private" } else { "public" }),
        ),
        ("has_issues", Json::Bool(true)),
        ("has_projects", Json::Bool(true)),
        ("has_wiki", Json::Bool(true)),
        ("has_pages", Json::Bool(false)),
        ("has_downloads", Json::Bool(true)),
        ("license", Json::Null),
        ("topics", Json::Arr(vec![])),
        ("open_issues_count", n(open_issues as u64)),
    ])
}

pub fn issue_json(full_name: &str, issue: &Issue) -> Json {
    obj(vec![
        ("id", n(issue.id)),
        (
            "node_id",
            s(&global_id("issue", &format!("{}#{}", full_name, issue.number))),
        ),
        ("number", n(issue.number)),
        ("title", s(&issue.title)),
        ("body", s(&issue.body)),
        ("state", s(issue.state.as_str())),
        (
            "labels",
            Json::Arr(
                issue
                    .labels
                    .iter()
                    .map(|label| obj(vec![("name", s(&label.name)), ("color", s(&label.color))]))
                    .collect(),
            ),
        ),
        (
            "user",
            obj(vec![
                ("login", s(VIEWER.login)),
                ("id", n(VIEWER.id)),
                ("type", s("User")),
            ]),
        ),
        ("created_at", s(FIXED_ISO)),
        ("updated_at", s(FIXED_ISO)),
        (
            "closed_at",
            if issue.state == State::Closed {
                s(FIXED_ISO)
            } else {
                Json::Null
            },
        ),
        ("comments", n(issue.comments.len() as u64)),
        (
            "html_url",
            s(&format!(
                "https://example.invalid/{}/issues/{}",
                full_name, issue.number
            )),
        ),
        (
            "url",
            s(&format!(
                "/api/v3/repos/{}/issues/{}",
                full_name, issue.number
            )),
        ),
        ("repository_url", s(&format!("/api/v3/repos/{}", full_name))),
    ])
}

/// The serialised GET-issue body, cached on the issue and invalidated by
/// every mutation: the read workload GETs one unchanged issue thousands of
/// times in a row.
pub fn issue_json_bytes(full_name: &str, issue: &mut Issue) -> Vec<u8> {
    if let Some(cached) = &issue.json_cache {
        return cached.clone();
    }
    let mut text = issue_json(full_name, issue).to_string();
    text.push('\n');
    let bytes = text.into_bytes();
    issue.json_cache = Some(bytes.clone());
    bytes
}

/// Invalidate an issue's cached body after a mutation.
pub fn touch_issue(issue: &mut Issue) {
    issue.rev += 1;
    issue.json_cache = None;
}

pub fn pull_json(repo: &Repo, pr: &PullRequest) -> Json {
    let full_name = repo.full_name();
    obj(vec![
        ("id", n(pr.id)),
        (
            "node_id",
            s(&global_id("pull", &format!("{}#{}", full_name, pr.number))),
        ),
        ("number", n(pr.number)),
        ("title", s(&pr.title)),
        ("body", s(&pr.body)),
        ("state", s(pr.state.as_str())),
        (
            "head",
            obj(vec![
                ("ref", s(&pr.head_ref)),
                ("sha", s(&pr.head_sha)),
                ("repo", repo_json(repo)),
            ]),
        ),
        (
            "base",
            obj(vec![
                ("ref", s(&pr.base_ref)),
                ("sha", s(&pr.base_sha)),
                ("repo", repo_json(repo)),
            ]),
        ),
        ("merged", Json::Bool(pr.merged)),
        (
            "mergeable",
            if pr.state == State::Open {
                Json::Bool(!merge_would_conflict(repo, pr))
            } else {
                Json::Null
            },
        ),
        (
            "merged_at",
            match &pr.merged_at {
                Some(t) => s(t),
                None => Json::Null,
            },
        ),
        (
            "merge_commit_sha",
            match &pr.merge_commit_sha {
                Some(sha) => s(sha),
                None => Json::Null,
            },
        ),
        (
            "user",
            obj(vec![
                ("login", s(VIEWER.login)),
                ("id", n(VIEWER.id)),
                ("type", s("User")),
            ]),
        ),
        ("created_at", s(FIXED_ISO)),
        ("updated_at", s(FIXED_ISO)),
        (
            "closed_at",
            if pr.state == State::Closed {
                s(FIXED_ISO)
            } else {
                Json::Null
            },
        ),
        (
            "html_url",
            s(&format!(
                "https://example.invalid/{}/pull/{}",
                full_name, pr.number
            )),
        ),
        (
            "url",
            s(&format!(
                "/api/v3/repos/{}/pulls/{}",
                full_name, pr.number
            )),
        ),
    ])
}

// ---------------------------------------------------------------------------
// Git helpers for the commits / pull-files endpoints
// ---------------------------------------------------------------------------

/// The commits reachable from `tip`, newest first (breadth-first walk).
fn commits_from(repo: &Repo, tip: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue: Vec<String> = vec![tip.to_string()];
    let mut head = 0usize;
    while head < queue.len() {
        let oid = queue[head].clone();
        head += 1;
        if !seen.insert(oid.clone()) {
            continue;
        }
        let object = match repo.objects.get(&oid) {
            Some(o) if matches!(o.kind, ObjKind::Commit) => o,
            _ => continue,
        };
        let commit = parse_commit(&object.data);
        out.push((oid, commit.message.trim_end_matches('\n').to_string()));
        for parent in commit.parents {
            queue.push(parent);
        }
    }
    out
}

/// Count the lines of a text blob (a trailing newline ends the last line).
fn line_count(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let mut parts: Vec<&str> = text.split('\n').collect();
    if parts.last() == Some(&"") {
        parts.pop();
    }
    parts.len()
}

/// Line diff counts via LCS lengths, two rows (same algorithm as git.ts).
fn line_diff(old_text: &str, new_text: &str) -> (usize, usize) {
    if old_text == new_text {
        return (0, 0);
    }
    let a: Vec<&str> = {
        let mut v: Vec<&str> = if old_text.is_empty() {
            vec![]
        } else {
            old_text.split('\n').collect()
        };
        if v.last() == Some(&"") {
            v.pop();
        }
        v
    };
    let b: Vec<&str> = {
        let mut v: Vec<&str> = if new_text.is_empty() {
            vec![]
        } else {
            new_text.split('\n').collect()
        };
        if v.last() == Some(&"") {
            v.pop();
        }
        v
    };
    let m = b.len();
    let mut prev = vec![0usize; m + 1];
    let mut cur = vec![0usize; m + 1];
    for i in 1..=a.len() {
        for j in 1..=m {
            cur[j] = if a[i - 1] == b[j - 1] {
                prev[j - 1] + 1
            } else {
                prev[j].max(cur[j - 1])
            };
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    let common = prev[m];
    (m - common, a.len() - common)
}

fn files_at(repo: &Repo, commit_oid: &str) -> Option<HashMap<String, (String, String)>> {
    let object = repo.objects.get(commit_oid)?;
    if !matches!(object.kind, ObjKind::Commit) {
        return None;
    }
    let tree = parse_commit(&object.data).tree;
    let lookup = |oid: &str| repo.objects.get(oid).cloned();
    Some(
        flatten_tree(&tree, &lookup)
            .into_iter()
            .map(|(path, oid, mode)| (path, (oid, mode)))
            .collect(),
    )
}

/// The changed files between the PR's base and head tips.
fn pull_files(repo: &Repo, pr: &PullRequest) -> Option<Vec<Json>> {
    let base_files: HashMap<String, (String, String)> = if pr.base_sha.is_empty() {
        HashMap::new()
    } else {
        files_at(repo, &pr.base_sha)?
    };
    let head_files = files_at(repo, &pr.head_sha)?;
    let mut paths: Vec<String> = base_files
        .keys()
        .chain(head_files.keys())
        .cloned()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    paths.sort();
    let mut out = Vec::new();
    for path in paths {
        let base = base_files.get(&path);
        let head = head_files.get(&path);
        if let (Some((b_oid, _)), Some((h_oid, _))) = (base, head) {
            if b_oid == h_oid {
                continue;
            }
        }
        let base_text = match base {
            Some((oid, _)) => repo
                .objects
                .get(oid)
                .map(|o| String::from_utf8_lossy(&o.data).into_owned())
                .unwrap_or_default(),
            None => String::new(),
        };
        let head_text = match head {
            Some((oid, _)) => repo
                .objects
                .get(oid)
                .map(|o| String::from_utf8_lossy(&o.data).into_owned())
                .unwrap_or_default(),
            None => String::new(),
        };
        let (status, additions, deletions) = match (base, head) {
            (None, Some(_)) => ("added", line_count(&head_text), 0),
            (Some(_), None) => ("removed", 0, line_count(&base_text)),
            (Some(_), Some(_)) => {
                let (a, d) = line_diff(&base_text, &head_text);
                ("modified", a, d)
            }
            (None, None) => unreachable!(),
        };
        out.push(obj(vec![
            ("filename", s(&path)),
            ("status", s(status)),
            ("additions", n(additions as u64)),
            ("deletions", n(deletions as u64)),
            ("changes", n((additions + deletions) as u64)),
        ]));
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Pagination
// ---------------------------------------------------------------------------

/// A page of already-rendered items, with a Link header when more follow.
fn page_response(store: &mut Store, req: &Request, items: Vec<Json>) -> Response {
    let per_page = query_int(&req.query, "per_page", 30).clamp(1, 100) as usize;
    let page_no = query_int(&req.query, "page", 1).max(1) as usize;
    let start = (page_no - 1).saturating_mul(per_page);
    let slice: Vec<Json> = items
        .iter()
        .skip(start)
        .take(per_page)
        .cloned()
        .collect();
    let mut extra: Vec<(String, String)> = Vec::new();
    if start.saturating_add(per_page) < items.len() {
        let host = header(req, "host").unwrap_or("localhost");
        let mut parts: Vec<String> = Vec::new();
        for (k, v) in crate::http::query_pairs(&req.query) {
            if k != "per_page" && k != "page" {
                parts.push(format!("{}={}", form_encode(&k), form_encode(&v)));
            }
        }
        parts.push(format!("per_page={}", per_page));
        parts.push(format!("page={}", page_no + 1));
        let next = format!("http://{}{}?{}", host, req.path, parts.join("&"));
        extra.push(("link".to_string(), format!("<{}>; rel=\"next\"", next)));
    }
    json_response_extra(store, 200, Json::Arr(slice), extra)
}

/// application/x-www-form-urlencoded encoding (space becomes '+').
fn form_encode(input: &str) -> String {
    let mut out = String::new();
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'*' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Base64 (lenient decode like Node's Buffer, standard padded encode)
// ---------------------------------------------------------------------------

fn b64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(triple >> 18) as usize & 63] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(triple >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[triple as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn b64_decode(text: &str) -> Vec<u8> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let clean: Vec<u8> = text.bytes().filter(|b| val(*b).is_some() || *b == b'=').collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= clean.len() {
        let mut quad = [0u32; 4];
        let mut pad = 0;
        for j in 0..4 {
            if clean[i + j] == b'=' {
                pad += 1;
            } else {
                quad[j] = val(clean[i + j]).unwrap_or(0);
            }
        }
        let triple = (quad[0] << 18) | (quad[1] << 12) | (quad[2] << 6) | quad[3];
        out.push((triple >> 16) as u8);
        if pad < 2 {
            out.push((triple >> 8) as u8);
        }
        if pad < 1 {
            out.push(triple as u8);
        }
        i += 4;
        if pad > 0 {
            break;
        }
    }
    // A trailing partial group (Node tolerates missing padding).
    let rest = &clean[i..];
    if rest.len() == 2 && rest.iter().all(|b| val(*b).is_some()) {
        let v = (val(rest[0]).unwrap() << 6) | val(rest[1]).unwrap();
        out.push((v >> 4) as u8);
    } else if rest.len() == 3 && rest.iter().all(|b| val(*b).is_some()) {
        let v = (val(rest[0]).unwrap() << 12) | (val(rest[1]).unwrap() << 6) | val(rest[2]).unwrap();
        out.push((v >> 10) as u8);
        out.push((v >> 2) as u8);
    }
    out
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// Handle one REST request. Returns None when the path is not under /api/v3.
pub fn handle_rest(store: &mut Store, req: &Request) -> Option<Response> {
    let segments = path_segments(req);
    if segments.len() < 2 || segments[0] != "api" || segments[1] != "v3" {
        return None;
    }
    let rest = &segments[2..];
    let method = req.method.as_str();

    // Every REST route needs a bearer token.
    let authorization = header(req, "authorization").unwrap_or("");
    if !authorization.starts_with("Bearer ")
        || authorization["Bearer ".len()..].trim().is_empty()
    {
        return Some(error_response(store, 401, "Requires authentication"));
    }

    let mut body = Json::Obj(vec![]);
    if method != "GET" && method != "HEAD" && !req.body.is_empty() {
        let text = String::from_utf8_lossy(&req.body);
        match Json::parse(&text) {
            Ok(v) => body = v,
            Err(_) => return Some(error_response(store, 400, "Problems parsing JSON")),
        }
    }

    // GET /api/v3/
    if method == "GET" && rest.is_empty() {
        return Some(json_response_extra(
            store,
            200,
            obj(vec![
                ("current_user_url", s("/api/v3/user")),
                ("current_user_repositories_url", s("/api/v3/user/repos")),
                ("repository_url", s("/api/v3/repos/{owner}/{repo}")),
                ("meta_url", s("/api/v3/meta")),
            ]),
            vec![("x-oauth-scopes".to_string(), "repo, read:org".to_string())],
        ));
    }

    // GET /api/v3/meta
    if method == "GET" && rest.len() == 1 && rest[0] == "meta" {
        return Some(json_response(
            store,
            200,
            obj(vec![
                ("verifiable_password_authentication", Json::Bool(false)),
                ("installed_version", s("1.0.0")),
                ("hooks", Json::Arr(vec![])),
                ("git", Json::Arr(vec![])),
                ("packages", Json::Arr(vec![])),
                ("pages", Json::Arr(vec![])),
            ]),
        ));
    }

    // GET /api/v3/user
    if method == "GET" && rest.len() == 1 && rest[0] == "user" {
        return Some(json_response(
            store,
            200,
            obj(vec![
                ("login", s(VIEWER.login)),
                ("id", n(VIEWER.id)),
                ("node_id", s(&global_id("user", VIEWER.login))),
                ("name", s(VIEWER.name)),
                ("email", s(VIEWER.email)),
                ("type", s("User")),
            ]),
        ));
    }

    // GET/POST /api/v3/user/repos
    if rest.len() == 2 && rest[0] == "user" && rest[1] == "repos" {
        if method == "GET" {
            let items: Vec<Json> = store
                .viewer_repos()
                .iter()
                .map(|repo| repo_json(repo))
                .collect();
            return Some(page_response(store, req, items));
        }
        if method == "POST" {
            let name = body_str(&body, "name");
            if name.is_empty() {
                return Some(error_response(
                    store,
                    422,
                    "Validation Failed: name is required",
                ));
            }
            if store.get_repo(VIEWER.login, &name).is_some() {
                return Some(error_response(
                    store,
                    422,
                    "Validation Failed: name already exists on this account",
                ));
            }
            let is_private = body.get("private").and_then(|v| v.as_bool()).unwrap_or(false);
            let description = body_str(&body, "description");
            let rendered = {
                let repo = store.create_repo(VIEWER.login, &name, &description, is_private);
                repo_json(repo)
            };
            return Some(json_response(store, 201, rendered));
        }
    }

    // /api/v3/repos/{owner}/{name}[...]
    if rest.len() >= 3 && rest[0] == "repos" {
        let owner = rest[1].clone();
        let name = rest[2].clone();
        let tail: Vec<String> = rest[3..].to_vec();

        // The repository itself.
        if tail.is_empty() {
            let exists = store.get_repo(&owner, &name).is_some();
            if !exists {
                return Some(error_response(store, 404, "Not Found"));
            }
            if method == "GET" {
                let rendered = {
                    let repo = store.get_repo(&owner, &name).unwrap();
                    repo_json(repo)
                };
                return Some(json_response(store, 200, rendered));
            }
            if method == "PATCH" {
                let rendered = {
                    let repo = store.get_repo(&owner, &name).unwrap();
                    if let Some(desc) = body.get("description").and_then(|v| v.as_str()) {
                        repo.description = desc.to_string();
                    }
                    repo_json(repo)
                };
                return Some(json_response(store, 200, rendered));
            }
            if method == "DELETE" {
                let full = format!("{}/{}", owner, name);
                store.repos.remove(&full);
                return Some(Response {
                    status: 204,
                    headers: vec![
                        (
                            "x-github-request-id".to_string(),
                            store.next_request_id(),
                        ),
                        ("content-length".to_string(), "0".to_string()),
                    ],
                    body: vec![],
                });
            }
            return Some(error_response(store, 404, "Not Found"));
        }

        if store.get_repo(&owner, &name).is_none() {
            return Some(error_response(store, 404, "Not Found"));
        }

        // POST /repos/{o}/{r}/git/refs — create a branch (or tag) at a sha.
        if tail.len() == 2 && tail[0] == "git" && tail[1] == "refs" && method == "POST" {
            let ref_name = body_str(&body, "ref");
            let sha = body_str(&body, "sha");
            let ref_ok = {
                let rest_part = ref_name
                    .strip_prefix("refs/heads/")
                    .or_else(|| ref_name.strip_prefix("refs/tags/"));
                match rest_part {
                    Some(part) => !part.is_empty() && !part.chars().any(|c| c.is_whitespace()),
                    None => false,
                }
            };
            let sha_ok = sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit());
            let sha_known = {
                let repo = store.get_repo(&owner, &name).unwrap();
                repo.objects.contains_key(&sha)
            };
            if !ref_ok || !sha_ok || !sha_known {
                return Some(error_response(
                    store,
                    422,
                    "Validation Failed: ref or sha is invalid",
                ));
            }
            let already = {
                let repo = store.get_repo(&owner, &name).unwrap();
                repo.refs.contains_key(&ref_name)
            };
            if already {
                return Some(error_response(
                    store,
                    422,
                    "Validation Failed: ref already exists",
                ));
            }
            let rendered = {
                let repo = store.get_repo(&owner, &name).unwrap();
                let kind = repo
                    .objects
                    .get(&sha)
                    .map(|o| obj_kind_name(&o.kind))
                    .unwrap_or("commit");
                repo.set_ref(&ref_name, &sha);
                obj(vec![
                    ("ref", s(&ref_name)),
                    (
                        "object",
                        obj(vec![("sha", s(&sha)), ("type", s(kind)), ("url", s(""))]),
                    ),
                ])
            };
            return Some(json_response(store, 201, rendered));
        }

        // GET /repos/{o}/{r}/branches
        if tail.len() == 1 && tail[0] == "branches" && method == "GET" {
            let rendered = {
                let repo = store.get_repo(&owner, &name).unwrap();
                let mut refs: Vec<&String> = repo
                    .refs
                    .keys()
                    .filter(|r| r.starts_with("refs/heads/"))
                    .collect();
                refs.sort();
                Json::Arr(
                    refs.iter()
                        .map(|r| {
                            obj(vec![
                                ("name", s(&r["refs/heads/".len()..])),
                                (
                                    "commit",
                                    obj(vec![(
                                        "sha",
                                        s(repo.refs.get(*r).map(|x| x.as_str()).unwrap_or("")),
                                    )]),
                                ),
                            ])
                        })
                        .collect(),
                )
            };
            return Some(json_response(store, 200, rendered));
        }

        // GET /repos/{o}/{r}/commits
        if tail.len() == 1 && tail[0] == "commits" && method == "GET" {
            let sha_param = query_value(&req.query, "sha");
            let tip = {
                let repo = store.get_repo(&owner, &name).unwrap();
                repo.resolve_ref(sha_param.as_deref())
            };
            let tip = match tip {
                Some(t) => t,
                None => return Some(error_response(store, 404, "Not Found")),
            };
            let rendered = {
                let repo = store.get_repo(&owner, &name).unwrap();
                let list = commits_from(repo, &tip);
                Json::Arr(
                    list.iter()
                        .map(|(sha, message)| {
                            let commit = repo
                                .objects
                                .get(sha)
                                .map(|o| parse_commit(&o.data));
                            let (tree, parents) = match &commit {
                                Some(c) => (c.tree.clone(), c.parents.clone()),
                                None => (String::new(), vec![]),
                            };
                            obj(vec![
                                ("sha", s(sha)),
                                (
                                    "commit",
                                    obj(vec![
                                        ("message", s(message)),
                                        (
                                            "author",
                                            obj(vec![
                                                ("name", s(VIEWER.name)),
                                                ("email", s(VIEWER.email)),
                                                ("date", s(FIXED_ISO)),
                                            ]),
                                        ),
                                        (
                                            "committer",
                                            obj(vec![
                                                ("name", s(VIEWER.name)),
                                                ("email", s(VIEWER.email)),
                                                ("date", s(FIXED_ISO)),
                                            ]),
                                        ),
                                        ("tree", obj(vec![("sha", s(&tree))])),
                                    ]),
                                ),
                                (
                                    "author",
                                    obj(vec![
                                        ("login", s(VIEWER.login)),
                                        ("id", n(VIEWER.id)),
                                        ("type", s("User")),
                                    ]),
                                ),
                                (
                                    "committer",
                                    obj(vec![
                                        ("login", s(VIEWER.login)),
                                        ("id", n(VIEWER.id)),
                                        ("type", s("User")),
                                    ]),
                                ),
                                (
                                    "parents",
                                    Json::Arr(
                                        parents
                                            .iter()
                                            .map(|p| obj(vec![("sha", s(p))]))
                                            .collect(),
                                    ),
                                ),
                            ])
                        })
                        .collect(),
                )
            };
            return Some(json_response(store, 200, rendered));
        }

        // /repos/{o}/{r}/contents[/path...]
        if !tail.is_empty() && tail[0] == "contents" {
            let file_path = tail[1..].join("/");
            let branch_param = query_value(&req.query, "branch")
                .or_else(|| query_value(&req.query, "ref"));
            if method == "GET" {
                let tip = {
                    let repo = store.get_repo(&owner, &name).unwrap();
                    repo.resolve_ref(branch_param.as_deref())
                };
                let tip = match tip {
                    Some(t) => t,
                    None => return Some(error_response(store, 404, "Not Found")),
                };
                let rendered = {
                    let repo = store.get_repo(&owner, &name).unwrap();
                    if let Some(tree_oid) = repo.tree_at(&tip, &file_path) {
                        // A directory: list its entries.
                        let mut entries: Vec<Json> = Vec::new();
                        if let Some(tree_obj) = repo.objects.get(&tree_oid) {
                            for entry in parse_tree(&tree_obj.data) {
                                let is_dir = entry.mode == "40000";
                                let size = if is_dir {
                                    0
                                } else {
                                    repo.objects
                                        .get(&entry.oid)
                                        .map(|o| o.data.len() as u64)
                                        .unwrap_or(0)
                                };
                                let entry_path = if file_path.is_empty() {
                                    entry.name.clone()
                                } else {
                                    format!("{}/{}", file_path, entry.name)
                                };
                                entries.push(obj(vec![
                                    ("type", s(if is_dir { "dir" } else { "file" })),
                                    ("name", s(&entry.name)),
                                    ("path", s(&entry_path)),
                                    ("sha", s(&entry.oid)),
                                    ("size", n(size)),
                                ]));
                            }
                        }
                        entries.sort_by(|a, b| {
                            let na = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
                            let nb = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
                            na.cmp(nb)
                        });
                        Some(Json::Arr(entries))
                    } else if let Some(blob_oid) = repo.blob_at(&tip, &file_path) {
                        let data = repo
                            .objects
                            .get(&blob_oid)
                            .map(|o| o.data.clone())
                            .unwrap_or_default();
                        let short = file_path.rsplit('/').next().unwrap_or("").to_string();
                        Some(obj(vec![
                            ("type", s("file")),
                            ("encoding", s("base64")),
                            ("size", n(data.len() as u64)),
                            ("name", s(&short)),
                            ("path", s(&file_path)),
                            ("content", s(&b64_encode(&data))),
                            ("sha", s(&blob_oid)),
                        ]))
                    } else {
                        None
                    }
                };
                return match rendered {
                    Some(v) => Some(json_response(store, 200, v)),
                    None => Some(error_response(store, 404, "Not Found")),
                };
            }
            if method == "PUT" {
                let message = body_str(&body, "message");
                let content_b64 = body_str(&body, "content");
                let branch_name = body_str_opt(&body, "branch")
                    .unwrap_or_else(|| {
                        let repo = store.get_repo(&owner, &name).unwrap();
                        repo.default_branch.clone()
                    });
                let sha = body_str_opt(&body, "sha");
                if message.is_empty() || content_b64.is_empty() {
                    return Some(error_response(
                        store,
                        422,
                        "Validation Failed: message and content are required",
                    ));
                }
                let content = b64_decode(&content_b64);
                let current_blob = {
                    let repo = store.get_repo(&owner, &name).unwrap();
                    repo.branch_tip(&branch_name)
                        .and_then(|tip| repo.blob_at(&tip, &file_path))
                };
                if let Some(want) = &sha {
                    match &current_blob {
                        Some(cur) if want.to_lowercase() != cur.to_lowercase() => {
                            return Some(error_response(
                                store,
                                409,
                                "Conflict: sha does not match the current file",
                            ));
                        }
                        None => {
                            return Some(error_response(
                                store,
                                409,
                                "Conflict: sha does not match the current file",
                            ));
                        }
                        _ => {}
                    }
                }
                let is_update = current_blob.is_some();
                let rendered = {
                    let repo = store.get_repo(&owner, &name).unwrap();
                    let (commit_sha, blob_sha) =
                        repo.commit_file(&branch_name, &file_path, &content, &message);
                    let short = file_path.rsplit('/').next().unwrap_or("").to_string();
                    obj(vec![
                        (
                            "content",
                            obj(vec![
                                ("name", s(&short)),
                                ("path", s(&file_path)),
                                ("sha", s(&blob_sha)),
                                ("size", n(content.len() as u64)),
                                ("encoding", s("base64")),
                            ]),
                        ),
                        (
                            "commit",
                            obj(vec![("sha", s(&commit_sha)), ("message", s(&message))]),
                        ),
                    ])
                };
                return Some(json_response(store, if is_update { 200 } else { 201 }, rendered));
            }
            if method == "DELETE" {
                let sha = body_str_opt(&body, "sha");
                let message = body_str(&body, "message");
                let branch_name = body_str_opt(&body, "branch")
                    .unwrap_or_else(|| {
                        let repo = store.get_repo(&owner, &name).unwrap();
                        repo.default_branch.clone()
                    });
                let tip_oid = {
                    let repo = store.get_repo(&owner, &name).unwrap();
                    repo.branch_tip(&branch_name)
                };
                let tip_oid = match tip_oid {
                    Some(t) => t,
                    None => return Some(error_response(store, 404, "Not Found")),
                };
                let current_blob = {
                    let repo = store.get_repo(&owner, &name).unwrap();
                    repo.blob_at(&tip_oid, &file_path)
                };
                let current_blob = match current_blob {
                    Some(b) => b,
                    None => return Some(error_response(store, 404, "Not Found")),
                };
                if let Some(want) = &sha {
                    if want.to_lowercase() != current_blob.to_lowercase() {
                        return Some(error_response(
                            store,
                            409,
                            "Conflict: sha does not match the current file",
                        ));
                    }
                }
                let rendered = {
                    let repo = store.get_repo(&owner, &name).unwrap();
                    let commit_sha = repo.delete_file(
                        &branch_name,
                        &file_path,
                        if message.is_empty() {
                            "Delete file"
                        } else {
                            &message
                        },
                    )
                    .unwrap_or_default();
                    obj(vec![(
                        "commit",
                        obj(vec![("sha", s(&commit_sha)), ("message", s(&message))]),
                    )])
                };
                return Some(json_response(store, 200, rendered));
            }
            return Some(error_response(store, 404, "Not Found"));
        }

        // POST /repos/{o}/{r}/labels
        if tail.len() == 1 && tail[0] == "labels" && method == "POST" {
            let label_name = body_str(&body, "name");
            let color = body_str_opt(&body, "color").unwrap_or_else(|| "ededed".to_string());
            if label_name.is_empty() {
                return Some(error_response(
                    store,
                    422,
                    "Validation Failed: name is required",
                ));
            }
            let rendered = {
                let repo = store.get_repo(&owner, &name).unwrap();
                if let Some(existing) = repo.labels.get(&label_name) {
                    obj(vec![
                        ("name", s(&existing.name)),
                        ("color", s(&existing.color)),
                    ])
                } else {
                    let label = Label {
                        name: label_name.clone(),
                        color: color.clone(),
                    };
                    repo.labels.insert(label_name.clone(), label);
                    obj(vec![("name", s(&label_name)), ("color", s(&color))])
                }
            };
            return Some(json_response(store, 201, rendered));
        }

        // /repos/{o}/{r}/issues[/{n}[/comments]]
        if !tail.is_empty() && tail[0] == "issues" {
            if tail.len() == 1 {
                if method == "GET" {
                    let state = query_value(&req.query, "state").unwrap_or_else(|| "open".into());
                    let direction =
                        query_value(&req.query, "direction").unwrap_or_else(|| "desc".into());
                    let items: Vec<Json> = {
                        let repo = store.get_repo(&owner, &name).unwrap();
                        let full = repo.full_name();
                        let mut issues: Vec<&Issue> = repo.issues.values().collect();
                        if state == "open" || state == "closed" {
                            issues.retain(|i| i.state.as_str() == state);
                        }
                        if direction == "asc" {
                            issues.sort_by_key(|i| i.created_seq);
                        } else {
                            issues.sort_by_key(|i| std::cmp::Reverse(i.created_seq));
                        }
                        issues.iter().map(|i| issue_json(&full, i)).collect()
                    };
                    return Some(page_response(store, req, items));
                }
                if method == "POST" {
                    let title = body_str(&body, "title");
                    let body_text = body_str(&body, "body");
                    let label_names: Vec<String> = match body.get("labels") {
                        Some(Json::Arr(items)) => items
                            .iter()
                            .filter_map(|v| v.as_str().map(|x| x.to_string()))
                            .collect(),
                        _ => vec![],
                    };
                    // Validate labels first, before creating anything.
                    let labels: Vec<Label> = {
                        let repo = store.get_repo(&owner, &name).unwrap();
                        let mut out = Vec::new();
                        for label_name in &label_names {
                            match repo.labels.get(label_name) {
                                Some(l) => out.push(l.clone()),
                                None => {
                                    return Some(error_response(
                                        store,
                                        422,
                                        &format!(
                                            "Validation Failed: label {} does not exist",
                                            Json::Str(label_name.clone()).to_string()
                                        ),
                                    ));
                                }
                            }
                        }
                        out
                    };
                    let rendered = {
                        let repo = store.get_repo(&owner, &name).unwrap();
                        let number = repo.next_issue_number;
                        repo.next_issue_number += 1;
                        let issue = Issue {
                            id: number,
                            number,
                            title,
                            body: body_text,
                            state: State::Open,
                            labels,
                            created_seq: repo.next_seq(),
                            comments: vec![],
                            rev: 0,
                            json_cache: None,
                        };
                        repo.issues.insert(number, issue);
                        let full = repo.full_name();
                        let issue = repo.issues.get(&number).unwrap();
                        issue_json(&full, issue)
                    };
                    return Some(json_response(store, 201, rendered));
                }
                return Some(error_response(store, 404, "Not Found"));
            }
            let number: u64 = parse_int_prefix(&tail[1]).unwrap_or(0);
            if tail.len() == 2 {
                let exists = {
                    let repo = store.get_repo(&owner, &name).unwrap();
                    repo.issues.contains_key(&number)
                };
                if !exists {
                    return Some(error_response(store, 404, "Not Found"));
                }
                if method == "GET" {
                    let bytes = {
                        let repo = store.get_repo(&owner, &name).unwrap();
                        let full = repo.full_name();
                        let issue = repo.issues.get_mut(&number).unwrap();
                        issue_json_bytes(&full, issue)
                    };
                    return Some(json_bytes_response(store, 200, bytes, vec![]));
                }
                if method == "PATCH" {
                    let rendered = {
                        let repo = store.get_repo(&owner, &name).unwrap();
                        let full = repo.full_name();
                        let issue = repo.issues.get_mut(&number).unwrap();
                        if let Some(st) = body.get("state").and_then(|v| v.as_str()) {
                            if st == "open" || st == "closed" {
                                issue.state = if st == "closed" { State::Closed } else { State::Open };
                            }
                        }
                        if let Some(t) = body.get("title").and_then(|v| v.as_str()) {
                            issue.title = t.to_string();
                        }
                        if let Some(b) = body.get("body").and_then(|v| v.as_str()) {
                            issue.body = b.to_string();
                        }
                        touch_issue(issue);
                        issue_json(&full, issue)
                    };
                    return Some(json_response(store, 200, rendered));
                }
                return Some(error_response(store, 404, "Not Found"));
            }
            if tail.len() == 3 && tail[2] == "comments" {
                let exists = {
                    let repo = store.get_repo(&owner, &name).unwrap();
                    repo.issues.contains_key(&number)
                };
                if !exists {
                    return Some(error_response(store, 404, "Not Found"));
                }
                if method == "GET" {
                    let rendered = {
                        let repo = store.get_repo(&owner, &name).unwrap();
                        let issue = repo.issues.get(&number).unwrap();
                        Json::Arr(
                            issue
                                .comments
                                .iter()
                                .map(|c| obj(vec![("id", n(c.id)), ("body", s(&c.body))]))
                                .collect(),
                        )
                    };
                    return Some(json_response(store, 200, rendered));
                }
                if method == "POST" {
                    let comment_body = body_str(&body, "body");
                    let rendered = {
                        let repo = store.get_repo(&owner, &name).unwrap();
                        let issue = repo.issues.get_mut(&number).unwrap();
                        let comment = crate::store::Comment {
                            id: repo.next_comment_id,
                            body: comment_body,
                        };
                        repo.next_comment_id += 1;
                        issue.comments.push(comment);
                        touch_issue(issue);
                        let last = issue.comments.last().unwrap();
                        obj(vec![("id", n(last.id)), ("body", s(&last.body))])
                    };
                    return Some(json_response(store, 201, rendered));
                }
                return Some(error_response(store, 404, "Not Found"));
            }
            return Some(error_response(store, 404, "Not Found"));
        }

        // /repos/{o}/{r}/pulls[/{n}[/merge|/files]]
        if !tail.is_empty() && tail[0] == "pulls" {
            if tail.len() == 1 {
                if method == "GET" {
                    let state = query_value(&req.query, "state").unwrap_or_else(|| "open".into());
                    let items: Vec<Json> = {
                        let repo = store.get_repo(&owner, &name).unwrap();
                        let mut pulls: Vec<&PullRequest> = repo.pulls.values().collect();
                        if state == "open" || state == "closed" {
                            pulls.retain(|p| p.state.as_str() == state);
                        }
                        pulls.sort_by_key(|p| std::cmp::Reverse(p.created_seq));
                        pulls.iter().map(|p| pull_json(repo, p)).collect()
                    };
                    return Some(page_response(store, req, items));
                }
                if method == "POST" {
                    let title = body_str(&body, "title");
                    let body_text = body_str(&body, "body");
                    let head_ref = body_str(&body, "head");
                    let base_ref = body_str(&body, "base");
                    let (head_sha, base_sha) = {
                        let repo = store.get_repo(&owner, &name).unwrap();
                        (repo.branch_tip(&head_ref), repo.branch_tip(&base_ref))
                    };
                    let (head_sha, base_sha) = match (head_sha, base_sha) {
                        (Some(h), Some(b)) => (h, b),
                        _ => {
                            return Some(error_response(
                                store,
                                422,
                                "Validation Failed: head or base branch does not exist",
                            ));
                        }
                    };
                    let rendered = {
                        let repo = store.get_repo(&owner, &name).unwrap();
                        let number = repo.next_pull_number;
                        repo.next_pull_number += 1;
                        let pr = PullRequest {
                            id: number,
                            number,
                            title,
                            body: body_text,
                            state: State::Open,
                            head_ref,
                            base_ref,
                            head_sha,
                            base_sha,
                            merged: false,
                            merged_at: None,
                            merge_commit_sha: None,
                            created_seq: repo.next_seq(),
                        };
                        repo.pulls.insert(number, pr);
                        let pr = repo.pulls.get(&number).unwrap();
                        pull_json(repo, pr)
                    };
                    return Some(json_response(store, 201, rendered));
                }
                return Some(error_response(store, 404, "Not Found"));
            }
            let number: u64 = parse_int_prefix(&tail[1]).unwrap_or(0);
            if tail.len() == 2 {
                let exists = {
                    let repo = store.get_repo(&owner, &name).unwrap();
                    repo.pulls.contains_key(&number)
                };
                if !exists {
                    return Some(error_response(store, 404, "Not Found"));
                }
                if method == "GET" {
                    let rendered = {
                        let repo = store.get_repo(&owner, &name).unwrap();
                        let pr = repo.pulls.get(&number).unwrap();
                        pull_json(repo, pr)
                    };
                    return Some(json_response(store, 200, rendered));
                }
                return Some(error_response(store, 404, "Not Found"));
            }
            if tail.len() == 3 && tail[2] == "merge" && method == "PUT" {
                let pr_copy = {
                    let repo = store.get_repo(&owner, &name).unwrap();
                    repo.pulls.get(&number).cloned()
                };
                let pr_copy = match pr_copy {
                    Some(p) => p,
                    None => return Some(error_response(store, 404, "Not Found")),
                };
                let mut pr_copy = pr_copy;
                if pr_copy.state != State::Open {
                    return Some(error_response(store, 405, "Pull request is not open"));
                }
                let commit_sha = {
                    let repo = store.get_repo(&owner, &name).unwrap();
                    squash_merge(repo, &mut pr_copy)
                };
                return match commit_sha {
                    Ok(sha) => {
                        let message = format!(
                            "Merge pull request #{} from {}",
                            pr_copy.number, pr_copy.head_ref
                        );
                        // squash_merge mutated the CLONE (state/merged/merge_commit_sha);
                        // write it back or the stored PR stays open with null merge sha.
                        let repo = store.get_repo(&owner, &name).unwrap();
                        repo.pulls.insert(number, pr_copy);
                        Some(json_response(
                            store,
                            200,
                            obj(vec![
                                ("sha", s(&sha)),
                                ("merged", Json::Bool(true)),
                                ("message", s(&message)),
                            ]),
                        ))
                    }
                    Err(_) => Some(error_response(
                        store,
                        405,
                        "Merge conflict: the pull request cannot be merged cleanly",
                    )),
                };
            }
            if tail.len() == 3 && tail[2] == "files" && method == "GET" {
                let rendered = {
                    let repo = store.get_repo(&owner, &name).unwrap();
                    match repo.pulls.get(&number) {
                        Some(pr) => pull_files(repo, pr),
                        None => return Some(error_response(store, 404, "Not Found")),
                    }
                };
                return match rendered {
                    Some(files) => Some(json_response(store, 200, Json::Arr(files))),
                    None => Some(error_response(store, 500, "Internal Server Error")),
                };
            }
            return Some(error_response(store, 404, "Not Found"));
        }
    }

    // Anything else under /api/v3 is an unknown route.
    Some(error_response(store, 404, "Not Found"))
}

fn obj_kind_name(kind: &ObjKind) -> &'static str {
    match kind {
        ObjKind::Blob => "blob",
        ObjKind::Tree => "tree",
        ObjKind::Commit => "commit",
        ObjKind::Tag => "tag",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Request as HttpRequest;

    fn req(method: &str, path: &str, query: &str, body: &[u8]) -> HttpRequest {
        HttpRequest {
            method: method.to_string(),
            path: path.to_string(),
            query: query.to_string(),
            headers: vec![("authorization".to_string(), "Bearer test-token".to_string())],
            body: body.to_vec(),
        }
    }

    fn body_str_of(resp: &Response) -> String {
        String::from_utf8_lossy(&resp.body).into_owned()
    }

    fn call(store: &mut Store, req: &HttpRequest) -> Response {
        handle_rest(store, req).expect("route should be handled")
    }

    #[test]
    fn auth_required() {
        let mut store = Store::new();
        let no_auth = HttpRequest {
            method: "GET".into(),
            path: "/api/v3/user".into(),
            query: String::new(),
            headers: vec![],
            body: vec![],
        };
        let resp = call(&mut store, &no_auth);
        assert_eq!(resp.status, 401);
        assert!(body_str_of(&resp).contains("Requires authentication"));
        // Empty bearer is also rejected.
        let empty = HttpRequest {
            headers: vec![("authorization".into(), "Bearer   ".into())],
            ..no_auth.clone()
        };
        assert_eq!(call(&mut store, &empty).status, 401);
    }

    #[test]
    fn root_meta_user() {
        let mut store = Store::new();
        let resp = call(&mut store, &req("GET", "/api/v3/", "", &[]));
        assert_eq!(resp.status, 200);
        assert!(body_str_of(&resp).contains("current_user_url"));
        assert!(resp
            .headers
            .iter()
            .any(|(n, v)| n == "x-oauth-scopes" && v == "repo, read:org"));
        let resp = call(&mut store, &req("GET", "/api/v3/meta", "", &[]));
        assert_eq!(resp.status, 200);
        assert!(body_str_of(&resp).contains("verifiable_password_authentication"));
        let resp = call(&mut store, &req("GET", "/api/v3/user", "", &[]));
        assert_eq!(resp.status, 200);
        assert!(body_str_of(&resp).contains("adaiversen"));
        // Unknown route under /api/v3 is a JSON 404.
        let resp = call(&mut store, &req("GET", "/api/v3/nope", "", &[]));
        assert_eq!(resp.status, 404);
        // Outside /api/v3 is not handled.
        assert!(handle_rest(&mut store, &req("GET", "/other", "", &[])).is_none());
    }

    #[test]
    fn repo_create_get_patch_delete() {
        let mut store = Store::new();
        let create = br#"{"name":"demo","description":"d","private":false}"#;
        let resp = call(&mut store, &req("POST", "/api/v3/user/repos", "", create));
        assert_eq!(resp.status, 201);
        let text = body_str_of(&resp);
        assert!(text.contains("\"full_name\":\"adaiversen/demo\""));
        assert!(text.contains("\"open_issues_count\":0"));
        // Duplicate name is 422.
        assert_eq!(
            call(&mut store, &req("POST", "/api/v3/user/repos", "", create)).status,
            422
        );
        // Empty name is 422.
        assert_eq!(
            call(
                &mut store,
                &req("POST", "/api/v3/user/repos", "", br#"{"name":""}"#)
            )
            .status,
            422
        );
        // Malformed JSON is 400.
        assert_eq!(
            call(&mut store, &req("POST", "/api/v3/user/repos", "", b"{oops")).status,
            400
        );
        // GET repo, PATCH description, DELETE.
        let resp = call(&mut store, &req("GET", "/api/v3/repos/adaiversen/demo", "", &[]));
        assert_eq!(resp.status, 200);
        let resp = call(
            &mut store,
            &req(
                "PATCH",
                "/api/v3/repos/adaiversen/demo",
                "",
                br#"{"description":"new"}"#,
            ),
        );
        assert_eq!(resp.status, 200);
        assert!(body_str_of(&resp).contains("\"description\":\"new\""));
        let resp = call(
            &mut store,
            &req("DELETE", "/api/v3/repos/adaiversen/demo", "", &[]),
        );
        assert_eq!(resp.status, 204);
        assert!(resp.body.is_empty());
        assert_eq!(
            call(&mut store, &req("GET", "/api/v3/repos/adaiversen/demo", "", &[])).status,
            404
        );
        // A repo under another owner is 404, never 403.
        assert_eq!(
            call(&mut store, &req("GET", "/api/v3/repos/someone/demo", "", &[])).status,
            404
        );
    }

    #[test]
    fn issues_flow_with_cache() {
        let mut store = Store::new();
        call(
            &mut store,
            &req("POST", "/api/v3/user/repos", "", br#"{"name":"demo"}"#),
        );
        let resp = call(
            &mut store,
            &req(
                "POST",
                "/api/v3/repos/adaiversen/demo/issues",
                "",
                br#"{"title":"Bug","body":"it broke"}"#,
            ),
        );
        assert_eq!(resp.status, 201);
        assert!(body_str_of(&resp).contains("\"number\":1"));
        // Unknown label is 422.
        assert_eq!(
            call(
                &mut store,
                &req(
                    "POST",
                    "/api/v3/repos/adaiversen/demo/issues",
                    "",
                    br#"{"title":"x","labels":["nope"]}"#
                )
            )
            .status,
            422
        );
        // GET single issue twice: second hit serves the cached bytes.
        let first = call(
            &mut store,
            &req("GET", "/api/v3/repos/adaiversen/demo/issues/1", "", &[]),
        );
        let second = call(
            &mut store,
            &req("GET", "/api/v3/repos/adaiversen/demo/issues/1", "", &[]),
        );
        assert_eq!(first.status, 200);
        assert_eq!(first.body, second.body);
        assert!(body_str_of(&first).contains("\"title\":\"Bug\""));
        // Comment create bumps the cache.
        let resp = call(
            &mut store,
            &req(
                "POST",
                "/api/v3/repos/adaiversen/demo/issues/1/comments",
                "",
                br#"{"body":"hi"}"#,
            ),
        );
        assert_eq!(resp.status, 201);
        assert!(body_str_of(&resp).contains("\"id\":1"));
        let after = call(
            &mut store,
            &req("GET", "/api/v3/repos/adaiversen/demo/issues/1", "", &[]),
        );
        assert!(body_str_of(&after).contains("\"comments\":1"));
        // PATCH closes the issue.
        let resp = call(
            &mut store,
            &req(
                "PATCH",
                "/api/v3/repos/adaiversen/demo/issues/1",
                "",
                br#"{"state":"closed"}"#,
            ),
        );
        assert_eq!(resp.status, 200);
        assert!(body_str_of(&resp).contains("\"state\":\"closed\""));
        assert!(body_str_of(&resp).contains("\"closed_at\":\"2025-01-01T00:00:00Z\""));
        // List filters to open only by default.
        let resp = call(
            &mut store,
            &req("GET", "/api/v3/repos/adaiversen/demo/issues", "", &[]),
        );
        assert_eq!(body_str_of(&resp), "[]\n");
        // Missing issue is 404.
        assert_eq!(
            call(
                &mut store,
                &req("GET", "/api/v3/repos/adaiversen/demo/issues/99", "", &[])
            )
            .status,
            404
        );
    }

    #[test]
    fn pagination_link_header() {
        let mut store = Store::new();
        call(
            &mut store,
            &req("POST", "/api/v3/user/repos", "", br#"{"name":"demo"}"#),
        );
        for i in 0..3 {
            call(
                &mut store,
                &req(
                    "POST",
                    "/api/v3/repos/adaiversen/demo/issues",
                    "",
                    format!("{{\"title\":\"t{}\"}}", i).as_bytes(),
                ),
            );
        }
        let resp = call(
            &mut store,
            &req(
                "GET",
                "/api/v3/repos/adaiversen/demo/issues",
                "per_page=2&page=1",
                &[],
            ),
        );
        assert_eq!(resp.status, 200);
        let link = resp
            .headers
            .iter()
            .find(|(n, _)| n == "link")
            .map(|(_, v)| v.clone())
            .unwrap();
        assert_eq!(
            link,
            "<http://localhost/api/v3/repos/adaiversen/demo/issues?per_page=2&page=2>; rel=\"next\""
        );
        // Last page has no Link header.
        let resp = call(
            &mut store,
            &req(
                "GET",
                "/api/v3/repos/adaiversen/demo/issues",
                "per_page=2&page=2",
                &[],
            ),
        );
        assert!(resp.headers.iter().find(|(n, _)| n == "link").is_none());
    }

    #[test]
    fn labels_flow() {
        let mut store = Store::new();
        call(
            &mut store,
            &req("POST", "/api/v3/user/repos", "", br#"{"name":"demo"}"#),
        );
        let resp = call(
            &mut store,
            &req(
                "POST",
                "/api/v3/repos/adaiversen/demo/labels",
                "",
                br#"{"name":"bug","color":"ff0000"}"#,
            ),
        );
        assert_eq!(resp.status, 201);
        assert!(body_str_of(&resp).contains("\"color\":\"ff0000\""));
        // Existing label returns 201 with the stored colour.
        let resp = call(
            &mut store,
            &req(
                "POST",
                "/api/v3/repos/adaiversen/demo/labels",
                "",
                br#"{"name":"bug","color":"00ff00"}"#,
            ),
        );
        assert_eq!(resp.status, 201);
        assert!(body_str_of(&resp).contains("\"color\":\"ff0000\""));
        // Empty name is 422; default colour applies.
        assert_eq!(
            call(
                &mut store,
                &req(
                    "POST",
                    "/api/v3/repos/adaiversen/demo/labels",
                    "",
                    br#"{"name":""}"#
                )
            )
            .status,
            422
        );
        let resp = call(
            &mut store,
            &req(
                "POST",
                "/api/v3/repos/adaiversen/demo/labels",
                "",
                br#"{"name":"enhancement"}"#,
            ),
        );
        assert!(body_str_of(&resp).contains("\"color\":\"ededed\""));
    }

    #[test]
    fn base64_roundtrip() {
        assert_eq!(b64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(b64_encode(b""), "");
        assert_eq!(b64_encode(b"f"), "Zg==");
        assert_eq!(b64_decode("aGVsbG8="), b"hello");
        assert_eq!(b64_decode("aGVsbG8"), b"hello");
        assert_eq!(b64_decode("Zg=="), b"f");
    }

    #[test]
    fn line_diff_counts() {
        assert_eq!(line_diff("a\nb\n", "a\nc\nb\n"), (1, 0));
        assert_eq!(line_diff("a\nb\n", "a\n"), (0, 1));
        assert_eq!(line_diff("same\n", "same\n"), (0, 0));
        assert_eq!(line_diff("", "x\ny\n"), (2, 0));
        assert_eq!(line_count("a\nb\n"), 2);
        assert_eq!(line_count(""), 0);
        assert_eq!(line_count("a"), 1);
    }
}
