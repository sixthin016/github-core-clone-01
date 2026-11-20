// mcp.rs — Model Context Protocol di POST /mcp (Streamable HTTP,
// JSON-RPC 2.0). Enam tool dengan nama seperti github-mcp-server, semuanya
// berbagi satu store dengan REST dan GraphQL. Hasil tool adalah satu blok
// teks berisi objek sebagai JSON. Tool tak dikenal dijawab JSON-RPC -32602;
// argumen wajib yang hilang dijawab hasil tool dengan isError:true; body
// bukan JSON dijawab HTTP 400 dengan -32700; tanpa Authorization dijawab 401.
//
// Bentuk JSON repo/issue/pull di sini ditulis ulang dari bentuk wire REST
// (kontrak SPEC tidak memuat helper JSON rest.rs, jadi modul ini tidak
// bergantung pada signature yang belum dikontrak).

use std::collections::HashMap;

use crate::http::{error_response, json_response, Request, Response};
use crate::json::Json;
use crate::store::{
    global_id, merge_would_conflict, squash_merge, Issue, PullRequest, Repo, State, Store,
    FIXED_ISO, VIEWER,
};

const SERVER_NAME: &str = "makefun-git-forge";
const SERVER_VERSION: &str = "1.0.0";
const PROTOCOL_VERSION: &str = "2026-07-28";

struct ToolDef {
    name: &'static str,
    desc: &'static str,
    props: &'static [(&'static str, &'static str, &'static str)],
    required: &'static [&'static str],
}

const TOOLS: &[ToolDef] = &[
    ToolDef {
        name: "create_repo",
        desc: "Create a repository owned by the authenticated user.",
        props: &[
            ("name", "string", "The repository name."),
            ("description", "string", "A short description of the repository."),
            ("private", "boolean", "Whether the repository is private."),
        ],
        required: &["name"],
    },
    ToolDef {
        name: "get_repo",
        desc: "Get a repository by owner and name.",
        props: &[
            ("owner", "string", "The repository owner."),
            ("repo", "string", "The repository name."),
        ],
        required: &["owner", "repo"],
    },
    ToolDef {
        name: "create_issue",
        desc: "Create an issue in a repository.",
        props: &[
            ("owner", "string", "The repository owner."),
            ("repo", "string", "The repository name."),
            ("title", "string", "The issue title."),
            ("body", "string", "The issue body."),
        ],
        required: &["owner", "repo", "title"],
    },
    ToolDef {
        name: "list_issues",
        desc: "List the issues of a repository.",
        props: &[
            ("owner", "string", "The repository owner."),
            ("repo", "string", "The repository name."),
            ("state", "string", "Filter by state: open, closed or all."),
        ],
        required: &["owner", "repo"],
    },
    ToolDef {
        name: "create_pull_request",
        desc: "Create a pull request from a head branch into a base branch.",
        props: &[
            ("owner", "string", "The repository owner."),
            ("repo", "string", "The repository name."),
            ("title", "string", "The pull request title."),
            ("body", "string", "The pull request body."),
            ("head", "string", "The head branch name."),
            ("base", "string", "The base branch name."),
        ],
        required: &["owner", "repo", "title", "head", "base"],
    },
    ToolDef {
        name: "merge_pull_request",
        desc: "Squash-merge a pull request into its base branch.",
        props: &[
            ("owner", "string", "The repository owner."),
            ("repo", "string", "The repository name."),
            ("pull_number", "number", "The pull request number."),
        ],
        required: &["owner", "repo", "pull_number"],
    },
];

fn tools_json() -> Json {
    Json::Arr(
        TOOLS
            .iter()
            .map(|t| {
                let props: Vec<(String, Json)> = t
                    .props
                    .iter()
                    .map(|(name, ty, desc)| {
                        (
                            name.to_string(),
                            Json::Obj(vec![
                                ("type".to_string(), Json::Str(ty.to_string())),
                                ("description".to_string(), Json::Str(desc.to_string())),
                            ]),
                        )
                    })
                    .collect();
                Json::Obj(vec![
                    ("name".to_string(), Json::Str(t.name.to_string())),
                    ("description".to_string(), Json::Str(t.desc.to_string())),
                    (
                        "inputSchema".to_string(),
                        Json::Obj(vec![
                            ("type".to_string(), Json::Str("object".to_string())),
                            ("properties".to_string(), Json::Obj(props)),
                            (
                                "required".to_string(),
                                Json::Arr(
                                    t.required
                                        .iter()
                                        .map(|r| Json::Str(r.to_string()))
                                        .collect(),
                                ),
                            ),
                        ]),
                    ),
                ])
            })
            .collect(),
    )
}

fn obj_get<'a>(value: &'a Json, key: &str) -> Option<&'a Json> {
    match value {
        Json::Obj(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
        _ => None,
    }
}

fn ok_result(value: Json) -> Json {
    Json::Obj(vec![(
        "content".to_string(),
        Json::Arr(vec![Json::Obj(vec![
            ("type".to_string(), Json::Str("text".to_string())),
            ("text".to_string(), Json::Str(format!("{}\n", value.to_string()))),
        ])]),
    )])
}

fn error_result(message: &str) -> Json {
    Json::Obj(vec![
        (
            "content".to_string(),
            Json::Arr(vec![Json::Obj(vec![
                ("type".to_string(), Json::Str("text".to_string())),
                ("text".to_string(), Json::Str(format!("{message}\n"))),
            ])]),
        ),
        ("isError".to_string(), Json::Bool(true)),
    ])
}

fn viewer_login() -> String {
    VIEWER.login.to_string()
}

fn user_json(login: &str) -> Json {
    Json::Obj(vec![
        ("login".to_string(), Json::Str(login.to_string())),
        ("id".to_string(), Json::Num(VIEWER.id as f64)),
        ("type".to_string(), Json::Str("User".to_string())),
    ])
}

fn repo_json(repo: &Repo) -> Json {
    let open = repo.issues.values().filter(|i| i.state == State::Open).count();
    let full = repo.full_name().to_string();
    Json::Obj(vec![
        ("id".to_string(), Json::Num(repo.database_id as f64)),
        ("node_id".to_string(), Json::Str(repo.global_id())),
        ("name".to_string(), Json::Str(repo.name.to_string())),
        ("full_name".to_string(), Json::Str(full.clone())),
        ("private".to_string(), Json::Bool(repo.is_private)),
        ("owner".to_string(), user_json(&repo.owner.to_string())),
        ("description".to_string(), Json::Str(repo.description.to_string())),
        ("default_branch".to_string(), Json::Str(repo.default_branch.to_string())),
        ("created_at".to_string(), Json::Str(FIXED_ISO.to_string())),
        ("updated_at".to_string(), Json::Str(FIXED_ISO.to_string())),
        ("pushed_at".to_string(), Json::Str(FIXED_ISO.to_string())),
        ("html_url".to_string(), Json::Str(format!("https://example.invalid/{full}"))),
        ("url".to_string(), Json::Str(format!("/api/v3/repos/{full}"))),
        ("clone_url".to_string(), Json::Str(format!("https://example.invalid/{full}.git"))),
        ("git_url".to_string(), Json::Str(format!("git://example.invalid/{full}.git"))),
        ("ssh_url".to_string(), Json::Str(format!("git@example.invalid:{full}.git"))),
        ("fork".to_string(), Json::Bool(false)),
        ("archived".to_string(), Json::Bool(false)),
        ("disabled".to_string(), Json::Bool(false)),
        (
            "visibility".to_string(),
            Json::Str(if repo.is_private { "private".to_string() } else { "public".to_string() }),
        ),
        ("has_issues".to_string(), Json::Bool(true)),
        ("has_projects".to_string(), Json::Bool(true)),
        ("has_wiki".to_string(), Json::Bool(true)),
        ("has_pages".to_string(), Json::Bool(false)),
        ("has_downloads".to_string(), Json::Bool(true)),
        ("license".to_string(), Json::Null),
        ("topics".to_string(), Json::Arr(Vec::new())),
        ("open_issues_count".to_string(), Json::Num(open as f64)),
    ])
}

fn issue_json(repo: &Repo, issue: &Issue) -> Json {
    let full = repo.full_name().to_string();
    Json::Obj(vec![
        ("id".to_string(), Json::Num(issue.id as f64)),
        (
            "node_id".to_string(),
            Json::Str(global_id("issue", &format!("{full}#{}", issue.number))),
        ),
        ("number".to_string(), Json::Num(issue.number as f64)),
        ("title".to_string(), Json::Str(issue.title.to_string())),
        ("body".to_string(), Json::Str(issue.body.to_string())),
        ("state".to_string(), Json::Str(issue.state.to_string())),
        (
            "labels".to_string(),
            Json::Arr(
                issue
                    .labels
                    .iter()
                    .map(|l| {
                        Json::Obj(vec![
                            ("name".to_string(), Json::Str(l.name.to_string())),
                            ("color".to_string(), Json::Str(l.color.to_string())),
                        ])
                    })
                    .collect(),
            ),
        ),
        ("user".to_string(), user_json(&viewer_login())),
        ("created_at".to_string(), Json::Str(FIXED_ISO.to_string())),
        ("updated_at".to_string(), Json::Str(FIXED_ISO.to_string())),
        (
            "closed_at".to_string(),
            if issue.state == State::Closed {
                Json::Str(FIXED_ISO.to_string())
            } else {
                Json::Null
            },
        ),
        ("comments".to_string(), Json::Num(issue.comments.len() as f64)),
        (
            "html_url".to_string(),
            Json::Str(format!("https://example.invalid/{full}/issues/{}", issue.number)),
        ),
        (
            "url".to_string(),
            Json::Str(format!("/api/v3/repos/{full}/issues/{}", issue.number)),
        ),
        ("repository_url".to_string(), Json::Str(format!("/api/v3/repos/{full}"))),
    ])
}

fn pull_json(repo: &Repo, pr: &PullRequest) -> Json {
    let full = repo.full_name().to_string();
    Json::Obj(vec![
        ("id".to_string(), Json::Num(pr.id as f64)),
        (
            "node_id".to_string(),
            Json::Str(global_id("pull", &format!("{full}#{}", pr.number))),
        ),
        ("number".to_string(), Json::Num(pr.number as f64)),
        ("title".to_string(), Json::Str(pr.title.to_string())),
        ("body".to_string(), Json::Str(pr.body.to_string())),
        ("state".to_string(), Json::Str(pr.state.to_string())),
        (
            "head".to_string(),
            Json::Obj(vec![
                ("ref".to_string(), Json::Str(pr.head_ref.to_string())),
                ("sha".to_string(), Json::Str(pr.head_sha.to_string())),
                ("repo".to_string(), repo_json(repo)),
            ]),
        ),
        (
            "base".to_string(),
            Json::Obj(vec![
                ("ref".to_string(), Json::Str(pr.base_ref.to_string())),
                ("sha".to_string(), Json::Str(pr.base_sha.to_string())),
                ("repo".to_string(), repo_json(repo)),
            ]),
        ),
        ("merged".to_string(), Json::Bool(pr.merged)),
        (
            "mergeable".to_string(),
            if pr.state == State::Open {
                Json::Bool(!merge_would_conflict(repo, pr))
            } else {
                Json::Null
            },
        ),
        (
            "merged_at".to_string(),
            match &pr.merged_at {
                Some(t) => Json::Str(t.to_string()),
                None => Json::Null,
            },
        ),
        (
            "merge_commit_sha".to_string(),
            match &pr.merge_commit_sha {
                Some(s) => Json::Str(s.to_string()),
                None => Json::Null,
            },
        ),
        ("user".to_string(), user_json(&viewer_login())),
        ("created_at".to_string(), Json::Str(FIXED_ISO.to_string())),
        ("updated_at".to_string(), Json::Str(FIXED_ISO.to_string())),
        (
            "closed_at".to_string(),
            if pr.state == State::Closed {
                Json::Str(FIXED_ISO.to_string())
            } else {
                Json::Null
            },
        ),
        (
            "html_url".to_string(),
            Json::Str(format!("https://example.invalid/{full}/pull/{}", pr.number)),
        ),
        (
            "url".to_string(),
            Json::Str(format!("/api/v3/repos/{full}/pulls/{}", pr.number)),
        ),
    ])
}

fn string_arg(args: &HashMap<String, Json>, name: &str) -> Option<String> {
    match args.get(name) {
        Some(Json::Str(s)) => Some(s.clone()),
        _ => None,
    }
}

fn number_arg(args: &HashMap<String, Json>, name: &str) -> Option<f64> {
    match args.get(name) {
        Some(Json::Num(n)) if n.is_finite() => Some(*n),
        _ => None,
    }
}

/// Jalankan satu tool. Err membawa (kode JSON-RPC, pesan) untuk tool tak
/// dikenal; semua kegagalan lain dijawab sebagai hasil tool isError:true.
fn call_tool(
    store: &mut Store,
    name: &str,
    args: &HashMap<String, Json>,
) -> Result<Json, (i64, String)> {
    let tool = TOOLS
        .iter()
        .find(|t| t.name == name)
        .ok_or_else(|| (-32602i64, format!("Unknown tool: {name}")))?;
    for required in tool.required {
        match args.get(*required) {
            None | Some(Json::Null) => {
                return Ok(error_result(&format!("Missing required argument: {required}")));
            }
            Some(Json::Str(s)) if s.is_empty() => {
                return Ok(error_result(&format!("Missing required argument: {required}")));
            }
            _ => {}
        }
    }
    match name {
        "create_repo" => {
            let viewer = viewer_login();
            let repo_name = string_arg(args, "name").unwrap_or_default();
            if store.get_repo(&viewer, &repo_name).is_some() {
                return Ok(error_result(&format!("Repository {viewer}/{repo_name} already exists")));
            }
            let description = string_arg(args, "description").unwrap_or_default();
            let private = matches!(args.get("private"), Some(Json::Bool(true)));
            let repo = store.create_repo(&viewer, &repo_name, &description, private);
            Ok(ok_result(repo_json(repo)))
        }
        "get_repo" => {
            let owner = string_arg(args, "owner").unwrap_or_default();
            let repo_name = string_arg(args, "repo").unwrap_or_default();
            let repo = match store.get_repo(&owner, &repo_name) {
                Some(r) => r,
                None => return Ok(error_result("Repository not found")),
            };
            Ok(ok_result(repo_json(repo)))
        }
        "create_issue" => {
            let owner = string_arg(args, "owner").unwrap_or_default();
            let repo_name = string_arg(args, "repo").unwrap_or_default();
            let title = string_arg(args, "title").unwrap_or_default();
            let body = string_arg(args, "body").unwrap_or_default();
            let repo = match store.get_repo(&owner, &repo_name) {
                Some(r) => r,
                None => return Ok(error_result("Repository not found")),
            };
            let number = repo.next_issue_number;
            repo.next_issue_number += 1;
            let issue = Issue {
                id: number,
                number,
                title,
                body,
                state: State::Open,
                labels: Vec::new(),
                created_seq: repo.next_seq(),
                comments: Vec::new(),
                rev: 0,
                json_cache: None,
            };
            let rendered = issue_json(repo, &issue);
            repo.issues.insert(number, issue);
            Ok(ok_result(rendered))
        }
        "list_issues" => {
            let owner = string_arg(args, "owner").unwrap_or_default();
            let repo_name = string_arg(args, "repo").unwrap_or_default();
            let repo = match store.get_repo(&owner, &repo_name) {
                Some(r) => r,
                None => return Ok(error_result("Repository not found")),
            };
            let state = string_arg(args, "state").unwrap_or_else(|| "open".to_string());
            let mut issues: Vec<&Issue> = repo.issues.values().collect();
            if state == "open" || state == "closed" {
                issues.retain(|i| i.state.as_str() == state);
            }
            issues.sort_by(|a, b| b.created_seq.cmp(&a.created_seq));
            let rendered: Vec<Json> =
                issues.iter().map(|i| issue_json(repo, i)).collect();
            Ok(ok_result(Json::Arr(rendered)))
        }
        "create_pull_request" => {
            let owner = string_arg(args, "owner").unwrap_or_default();
            let repo_name = string_arg(args, "repo").unwrap_or_default();
            let title = string_arg(args, "title").unwrap_or_default();
            let body = string_arg(args, "body").unwrap_or_default();
            let head_ref = string_arg(args, "head").unwrap_or_default();
            let base_ref = string_arg(args, "base").unwrap_or_default();
            let repo = match store.get_repo(&owner, &repo_name) {
                Some(r) => r,
                None => return Ok(error_result("Repository not found")),
            };
            let head_sha = repo.branch_tip(&head_ref).map(|s| s.to_string());
            let base_sha = repo.branch_tip(&base_ref).map(|s| s.to_string());
            let (head_sha, base_sha) = match (head_sha, base_sha) {
                (Some(h), Some(b)) => (h, b),
                _ => return Ok(error_result("Head or base branch not found")),
            };
            let number = repo.next_pull_number;
            repo.next_pull_number += 1;
            let pr = PullRequest {
                id: number,
                number,
                title,
                body,
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
            let rendered = pull_json(repo, &pr);
            repo.pulls.insert(number, pr);
            Ok(ok_result(rendered))
        }
        "merge_pull_request" => {
            let owner = string_arg(args, "owner").unwrap_or_default();
            let repo_name = string_arg(args, "repo").unwrap_or_default();
            let number = number_arg(args, "pull_number").unwrap_or(-1.0) as u64;
            let key = format!("{owner}/{repo_name}");
            let mut pr = match store.repos.get_mut(&key) {
                Some(repo) => match repo.pulls.remove(&number) {
                    Some(p) => p,
                    None => return Ok(error_result("Pull request not found")),
                },
                None => return Ok(error_result("Pull request not found")),
            };
            if pr.state != State::Open {
                let num = pr.number;
                if let Some(repo) = store.repos.get_mut(&key) {
                    repo.pulls.insert(num, pr);
                }
                return Ok(error_result("Pull request is not open"));
            }
            let merged = match store.repos.get_mut(&key) {
                Some(repo) => squash_merge(repo, &mut pr),
                None => Err(vec!["repository not found".to_string()]),
            };
            let num = pr.number;
            if let Some(repo) = store.repos.get_mut(&key) {
                repo.pulls.insert(num, pr);
            }
            match merged {
                Ok(sha) => Ok(ok_result(Json::Obj(vec![
                    ("merged".to_string(), Json::Bool(true)),
                    ("sha".to_string(), Json::Str(sha)),
                    ("number".to_string(), Json::Num(num as f64)),
                ]))),
                Err(_) => {
                    Ok(error_result("Merge conflict: the pull request cannot be merged cleanly"))
                }
            }
        }
        _ => Ok(error_result(&format!("Unknown tool: {name}"))),
    }
}

fn rpc_result(store: &mut Store, id: Json, result: Json) -> Response {
    json_response(
        store,
        200,
        Json::Obj(vec![
            ("jsonrpc".to_string(), Json::Str("2.0".to_string())),
            ("id".to_string(), id),
            ("result".to_string(), result),
        ]),
    )
}

fn rpc_error(store: &mut Store, id: Json, code: i64, message: &str) -> Response {
    json_response(
        store,
        200,
        Json::Obj(vec![
            ("jsonrpc".to_string(), Json::Str("2.0".to_string())),
            ("id".to_string(), id),
            (
                "error".to_string(),
                Json::Obj(vec![
                    ("code".to_string(), Json::Num(code as f64)),
                    ("message".to_string(), Json::Str(message.to_string())),
                ]),
            ),
        ]),
    )
}

fn header<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    req.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn looks_like_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[0].is_ascii_digit()
        && b[1].is_ascii_digit()
        && b[2].is_ascii_digit()
        && b[3].is_ascii_digit()
        && b[4] == b'-'
        && b[5].is_ascii_digit()
        && b[6].is_ascii_digit()
        && b[7] == b'-'
        && b[8].is_ascii_digit()
        && b[9].is_ascii_digit()
}

/// Tangani satu request MCP. None berarti method bukan POST.
pub fn handle_mcp(store: &mut Store, req: &Request) -> Option<Response> {
    if req.method != "POST" {
        return None;
    }
    let auth = header(req, "authorization").unwrap_or("");
    if !auth.starts_with("Bearer ") || auth["Bearer ".len()..].trim().is_empty() {
        return Some(error_response(store, 401, "Requires authentication"));
    }
    let text = String::from_utf8_lossy(&req.body);
    let body = match Json::parse(&text) {
        Ok(v) => v,
        Err(_) => {
            return Some(json_response(
                store,
                400,
                Json::Obj(vec![
                    ("jsonrpc".to_string(), Json::Str("2.0".to_string())),
                    ("id".to_string(), Json::Null),
                    (
                        "error".to_string(),
                        Json::Obj(vec![
                            ("code".to_string(), Json::Num(-32700.0)),
                            ("message".to_string(), Json::Str("Parse error".to_string())),
                        ]),
                    ),
                ]),
            ));
        }
    };
    let Json::Obj(_) = body else {
        return Some(json_response(
            store,
            400,
            Json::Obj(vec![
                ("jsonrpc".to_string(), Json::Str("2.0".to_string())),
                ("id".to_string(), Json::Null),
                (
                    "error".to_string(),
                    Json::Obj(vec![
                        ("code".to_string(), Json::Num(-32600.0)),
                        ("message".to_string(), Json::Str("Invalid Request".to_string())),
                    ]),
                ),
            ]),
        ));
    };
    let id_json = obj_get(&body, "id").cloned().unwrap_or(Json::Null);
    let method = match obj_get(&body, "method") {
        Some(Json::Str(s)) => s.clone(),
        _ => String::new(),
    };
    let params: HashMap<String, Json> = match obj_get(&body, "params") {
        Some(Json::Obj(fields)) => fields.iter().cloned().collect(),
        _ => HashMap::new(),
    };

    // Notifikasi (tanpa id) tidak dijawab dengan body.
    if obj_get(&body, "id").is_none() {
        return Some(Response {
            status: 202,
            headers: vec![("x-github-request-id".to_string(), store.next_request_id())],
            body: Vec::new(),
        });
    }

    if method == "server/discover" {
        return Some(rpc_result(
            store,
            id_json,
            Json::Obj(vec![
                ("protocolVersion".to_string(), Json::Str(PROTOCOL_VERSION.to_string())),
                (
                    "serverInfo".to_string(),
                    Json::Obj(vec![
                        ("name".to_string(), Json::Str(SERVER_NAME.to_string())),
                        ("version".to_string(), Json::Str(SERVER_VERSION.to_string())),
                    ]),
                ),
                (
                    "capabilities".to_string(),
                    Json::Obj(vec![("tools".to_string(), Json::Obj(Vec::new()))]),
                ),
                (
                    "supportedVersions".to_string(),
                    Json::Arr(vec![Json::Str(PROTOCOL_VERSION.to_string())]),
                ),
            ]),
        ));
    }
    if method == "initialize" {
        let offered = match params.get("protocolVersion") {
            Some(Json::Str(s)) => s.clone(),
            _ => PROTOCOL_VERSION.to_string(),
        };
        let negotiated =
            if looks_like_date(&offered) { offered } else { PROTOCOL_VERSION.to_string() };
        return Some(rpc_result(
            store,
            id_json,
            Json::Obj(vec![
                ("protocolVersion".to_string(), Json::Str(negotiated)),
                (
                    "serverInfo".to_string(),
                    Json::Obj(vec![
                        ("name".to_string(), Json::Str(SERVER_NAME.to_string())),
                        ("version".to_string(), Json::Str(SERVER_VERSION.to_string())),
                    ]),
                ),
                (
                    "capabilities".to_string(),
                    Json::Obj(vec![("tools".to_string(), Json::Obj(Vec::new()))]),
                ),
            ]),
        ));
    }
    if method == "tools/list" {
        return Some(rpc_result(
            store,
            id_json,
            Json::Obj(vec![("tools".to_string(), tools_json())]),
        ));
    }
    if method == "tools/call" {
        let tool_name = match params.get("name") {
            Some(Json::Str(s)) => s.clone(),
            _ => String::new(),
        };
        let args: HashMap<String, Json> = match params.get("arguments") {
            Some(Json::Obj(fields)) => fields.iter().cloned().collect(),
            _ => HashMap::new(),
        };
        return Some(match call_tool(store, &tool_name, &args) {
            Ok(result) => rpc_result(store, id_json, result),
            Err((code, message)) => rpc_error(store, id_json, code, &message),
        });
    }
    Some(rpc_error(store, id_json, -32601, &format!("Method not found: {method}")))
}

// -------------------------------------------------------------------------------------------
// Test
// -------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Request;

    fn post_req(body: &str) -> Request {
        Request {
            method: "POST".to_string(),
            path: "/mcp".to_string(),
            query: String::new(),
            headers: vec![("authorization".to_string(), "Bearer test".to_string())],
            body: body.as_bytes().to_vec(),
        }
    }

    fn text(resp: &Response) -> String {
        String::from_utf8_lossy(&resp.body).to_string()
    }

    #[test]
    fn tools_list_has_six_tools() {
        let mut store = Store::new();
        let req = post_req(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#);
        let resp = handle_mcp(&mut store, &req).expect("handled");
        assert_eq!(resp.status, 200);
        let body = text(&resp);
        for name in [
            "create_repo",
            "get_repo",
            "create_issue",
            "list_issues",
            "create_pull_request",
            "merge_pull_request",
        ] {
            assert!(body.contains(name), "missing {name}: {body}");
        }
    }

    #[test]
    fn initialize_negotiates_offered_version() {
        let mut store = Store::new();
        let req = post_req(
            r#"{"jsonrpc":"2.0","id":"init-1","method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
        );
        let resp = handle_mcp(&mut store, &req).expect("handled");
        let body = text(&resp);
        assert!(body.contains("\"protocolVersion\":\"2025-11-25\""), "body: {body}");
        assert!(body.contains("makefun-git-forge"), "body: {body}");
    }

    #[test]
    fn create_repo_then_get_repo_end_to_end() {
        let mut store = Store::new();
        let req = post_req(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"create_repo","arguments":{"name":"demo","description":"d","private":false}}}"#,
        );
        let resp = handle_mcp(&mut store, &req).expect("handled");
        let body = text(&resp);
        assert!(body.contains("\\\"full_name\\\":\\\"adaiversen/demo\\\""), "body: {body}");
        assert!(!body.contains("isError"), "body: {body}");
        let req2 = post_req(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"get_repo","arguments":{"owner":"adaiversen","repo":"demo"}}}"#,
        );
        let resp2 = handle_mcp(&mut store, &req2).expect("handled");
        let body2 = text(&resp2);
        assert!(body2.contains("\\\"name\\\":\\\"demo\\\""), "body: {body2}");
    }

    #[test]
    fn unknown_tool_is_minus_32602() {
        let mut store = Store::new();
        let req = post_req(
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"no_such_tool","arguments":{}}}"#,
        );
        let resp = handle_mcp(&mut store, &req).expect("handled");
        let body = text(&resp);
        assert!(body.contains("-32602"), "body: {body}");
    }

    #[test]
    fn missing_args_answer_is_error() {
        let mut store = Store::new();
        let req = post_req(
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"create_issue","arguments":{}}}"#,
        );
        let resp = handle_mcp(&mut store, &req).expect("handled");
        let body = text(&resp);
        assert!(body.contains("\"isError\":true"), "body: {body}");
        assert!(body.contains("Missing required argument"), "body: {body}");
    }

    #[test]
    fn notification_gets_202_empty() {
        let mut store = Store::new();
        let req = post_req(
            r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#,
        );
        let resp = handle_mcp(&mut store, &req).expect("handled");
        assert_eq!(resp.status, 202);
        assert!(resp.body.is_empty());
    }

    #[test]
    fn missing_token_is_401_and_bad_json_is_400() {
        let mut store = Store::new();
        let req = Request {
            method: "POST".to_string(),
            path: "/mcp".to_string(),
            query: String::new(),
            headers: vec![],
            body: b"{}".to_vec(),
        };
        let resp = handle_mcp(&mut store, &req).expect("handled");
        assert_eq!(resp.status, 401);
        let req2 = post_req("{not json");
        let resp2 = handle_mcp(&mut store, &req2).expect("handled");
        assert_eq!(resp2.status, 400);
        assert!(text(&resp2).contains("-32700"), "body: {}", text(&resp2));
    }
}
