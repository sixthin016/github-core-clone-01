// graphql.rs — GraphQL v4 di POST /api/graphql (std only).
//
// Executor kecil yang hanya menutup dokumen-dokumen yang dipakai suite:
// viewer, repository, node, introspeksi __type, dan lima mutasi.
// Field tak dikenal dijawab HTTP 200 dengan array `errors` yang menyebut
// field itu; repository tak dikenal dijawab data.repository null tanpa
// error; token hilang dijawab 401.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::http::{error_response, json_response, Request, Response};
use crate::json::Json;
use crate::object::{flatten_tree, parse_commit, GitObject, ObjKind};
use crate::store::{
    decode_global_id, global_id, merge_would_conflict, squash_merge, Comment, Issue, PullRequest,
    Repo, State, Store, FIXED_ISO, VIEWER,
};

type Vars = HashMap<String, Json>;

// -------------------------------------------------------------------------------------------
// Tokenizer dan parser dokumen
// -------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum TokKind {
    Name,
    Int,
    Float,
    Str,
    Punct,
    Eof,
}

#[derive(Clone, Debug)]
struct Tok {
    kind: TokKind,
    value: String,
}

fn tokenize(src: &str) -> Vec<Tok> {
    let chars: Vec<char> = src.chars().collect();
    let mut toks: Vec<Tok> = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        let ch = chars[i];
        if ch == ' ' || ch == '\t' || ch == '\n' || ch == '\r' || ch == ',' {
            i += 1;
            continue;
        }
        if ch == '#' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if ch == '"' {
            let mut value = String::new();
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    let esc = chars[i + 1];
                    value.push(match esc {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        other => other,
                    });
                    i += 2;
                } else {
                    value.push(chars[i]);
                    i += 1;
                }
            }
            i += 1;
            toks.push(Tok { kind: TokKind::Str, value });
            continue;
        }
        if ch == '-' || ch.is_ascii_digit() {
            let mut value = String::new();
            let mut is_float = false;
            if ch == '-' {
                value.push(ch);
                i += 1;
            }
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                if chars[i] == '.' {
                    is_float = true;
                }
                value.push(chars[i]);
                i += 1;
            }
            toks.push(Tok { kind: if is_float { TokKind::Float } else { TokKind::Int }, value });
            continue;
        }
        if ch.is_ascii_alphabetic() || ch == '_' {
            let mut value = String::new();
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                value.push(chars[i]);
                i += 1;
            }
            toks.push(Tok { kind: TokKind::Name, value });
            continue;
        }
        if ch == '.' && i + 2 < chars.len() && chars[i + 1] == '.' && chars[i + 2] == '.' {
            toks.push(Tok { kind: TokKind::Punct, value: "...".to_string() });
            i += 3;
            continue;
        }
        toks.push(Tok { kind: TokKind::Punct, value: ch.to_string() });
        i += 1;
    }
    toks.push(Tok { kind: TokKind::Eof, value: String::new() });
    toks
}

#[derive(Clone, Debug)]
enum ValueNode {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
    Enum(String),
    Var(String),
    List(Vec<ValueNode>),
    Object(Vec<(String, ValueNode)>),
}

#[derive(Clone, Debug)]
struct FieldNode {
    alias: String,
    name: String,
    args: Vec<(String, ValueNode)>,
    selections: Option<Vec<SelNode>>,
}

#[derive(Clone, Debug)]
enum SelNode {
    Field(FieldNode),
    Fragment { cond: Option<String>, selections: Vec<SelNode> },
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Op {
    Query,
    Mutation,
}

#[derive(Clone, Debug)]
struct Operation {
    op: Op,
    name: Option<String>,
    vars: Vec<(String, Option<ValueNode>)>,
    selections: Vec<SelNode>,
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn new(src: &str) -> Self {
        Parser { toks: tokenize(src), pos: 0 }
    }

    fn peek(&self) -> &Tok {
        &self.toks[self.pos]
    }

    fn next(&mut self) -> Tok {
        let tok = self.toks[self.pos].clone();
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        tok
    }

    fn expect_punct(&mut self, want: &str) -> Result<(), String> {
        let tok = self.next();
        if tok.kind == TokKind::Punct && tok.value == want {
            Ok(())
        } else {
            Err(format!("expected {:?}, got {:?}", want, tok.value))
        }
    }

    fn expect_name(&mut self, want: Option<&str>) -> Result<String, String> {
        let tok = self.next();
        if tok.kind == TokKind::Name && want.map_or(true, |w| w == tok.value) {
            Ok(tok.value)
        } else {
            Err(format!("expected name {}, got {:?}", want.unwrap_or(""), tok.value))
        }
    }

    fn parse_document(&mut self) -> Result<Vec<Operation>, String> {
        let mut ops: Vec<Operation> = Vec::new();
        while self.peek().kind != TokKind::Eof {
            let tok = self.peek().clone();
            if tok.kind == TokKind::Punct && tok.value == "{" {
                ops.push(Operation {
                    op: Op::Query,
                    name: None,
                    vars: Vec::new(),
                    selections: self.parse_selections()?,
                });
                continue;
            }
            if tok.kind == TokKind::Name && (tok.value == "query" || tok.value == "mutation") {
                let op = if self.next().value == "query" { Op::Query } else { Op::Mutation };
                let mut name: Option<String> = None;
                if self.peek().kind == TokKind::Name {
                    name = Some(self.next().value);
                }
                let mut vars: Vec<(String, Option<ValueNode>)> = Vec::new();
                if self.peek().kind == TokKind::Punct && self.peek().value == "(" {
                    self.next();
                    while !(self.peek().kind == TokKind::Punct && self.peek().value == ")") {
                        if self.peek().kind == TokKind::Eof {
                            return Err("unterminated variable definitions".to_string());
                        }
                        self.expect_punct("$")?;
                        let var_name = self.expect_name(None)?;
                        self.expect_punct(":")?;
                        self.parse_type()?;
                        let mut default: Option<ValueNode> = None;
                        if self.peek().kind == TokKind::Punct && self.peek().value == "=" {
                            self.next();
                            default = Some(self.parse_value()?);
                        }
                        vars.push((var_name, default));
                    }
                    self.next();
                }
                ops.push(Operation { op, name, vars, selections: self.parse_selections()? });
                continue;
            }
            if tok.kind == TokKind::Name && tok.value == "fragment" {
                // Definisi fragment bernama tidak dipakai suite; lewati saja.
                self.next();
                self.expect_name(None)?;
                self.expect_name(Some("on"))?;
                self.expect_name(None)?;
                self.parse_selections()?;
                continue;
            }
            return Err(format!("unexpected token {:?}", tok.value));
        }
        Ok(ops)
    }

    fn parse_type(&mut self) -> Result<(), String> {
        if self.peek().kind == TokKind::Punct && self.peek().value == "[" {
            self.next();
            self.parse_type()?;
            self.expect_punct("]")?;
        } else {
            self.expect_name(None)?;
        }
        if self.peek().kind == TokKind::Punct && self.peek().value == "!" {
            self.next();
        }
        Ok(())
    }

    fn parse_selections(&mut self) -> Result<Vec<SelNode>, String> {
        self.expect_punct("{")?;
        let mut sels: Vec<SelNode> = Vec::new();
        while !(self.peek().kind == TokKind::Punct && self.peek().value == "}") {
            if self.peek().kind == TokKind::Eof {
                return Err("unterminated selection set".to_string());
            }
            sels.push(self.parse_selection()?);
        }
        self.next();
        Ok(sels)
    }

    fn parse_selection(&mut self) -> Result<SelNode, String> {
        if self.peek().kind == TokKind::Punct && self.peek().value == "..." {
            self.next();
            let mut cond: Option<String> = None;
            if self.peek().kind == TokKind::Name && self.peek().value == "on" {
                self.next();
                cond = Some(self.expect_name(None)?);
            } else if self.peek().kind == TokKind::Name {
                // Spread fragment bernama: tidak dipakai suite; kosongkan.
                self.next();
                return Ok(SelNode::Fragment { cond: None, selections: Vec::new() });
            }
            return Ok(SelNode::Fragment { cond, selections: self.parse_selections()? });
        }
        let first = self.expect_name(None)?;
        let mut alias = first.clone();
        let mut name = first;
        if self.peek().kind == TokKind::Punct && self.peek().value == ":" {
            self.next();
            name = self.expect_name(None)?;
        } else {
            alias = name.clone();
        }
        let mut args: Vec<(String, ValueNode)> = Vec::new();
        if self.peek().kind == TokKind::Punct && self.peek().value == "(" {
            self.next();
            while !(self.peek().kind == TokKind::Punct && self.peek().value == ")") {
                if self.peek().kind == TokKind::Eof {
                    return Err("unterminated arguments".to_string());
                }
                let arg_name = self.expect_name(None)?;
                self.expect_punct(":")?;
                args.push((arg_name, self.parse_value()?));
            }
            self.next();
        }
        let mut selections: Option<Vec<SelNode>> = None;
        if self.peek().kind == TokKind::Punct && self.peek().value == "{" {
            selections = Some(self.parse_selections()?);
        }
        Ok(SelNode::Field(FieldNode { alias, name, args, selections }))
    }

    fn parse_value(&mut self) -> Result<ValueNode, String> {
        let tok = self.next();
        match tok.kind {
            TokKind::Str => Ok(ValueNode::Str(tok.value)),
            TokKind::Int => Ok(ValueNode::Int(tok.value.parse::<i64>().unwrap_or(0))),
            TokKind::Float => Ok(ValueNode::Float(tok.value.parse::<f64>().unwrap_or(0.0))),
            TokKind::Name => match tok.value.as_str() {
                "true" => Ok(ValueNode::Bool(true)),
                "false" => Ok(ValueNode::Bool(false)),
                "null" => Ok(ValueNode::Null),
                _ => Ok(ValueNode::Enum(tok.value)),
            },
            TokKind::Punct => match tok.value.as_str() {
                "$" => Ok(ValueNode::Var(self.expect_name(None)?)),
                "[" => {
                    let mut values: Vec<ValueNode> = Vec::new();
                    while !(self.peek().kind == TokKind::Punct && self.peek().value == "]") {
                        if self.peek().kind == TokKind::Eof {
                            return Err("unterminated list".to_string());
                        }
                        values.push(self.parse_value()?);
                    }
                    self.next();
                    Ok(ValueNode::List(values))
                }
                "{" => {
                    let mut fields: Vec<(String, ValueNode)> = Vec::new();
                    while !(self.peek().kind == TokKind::Punct && self.peek().value == "}") {
                        if self.peek().kind == TokKind::Eof {
                            return Err("unterminated object".to_string());
                        }
                        let name = self.expect_name(None)?;
                        self.expect_punct(":")?;
                        fields.push((name, self.parse_value()?));
                    }
                    self.next();
                    Ok(ValueNode::Object(fields))
                }
                _ => Err(format!("unexpected punct {:?} in value", tok.value)),
            },
            TokKind::Eof => Err(format!("unexpected token {:?} in value", tok.value)),
        }
    }
}

fn value_from_node(node: &ValueNode, vars: &Vars) -> Json {
    match node {
        ValueNode::Str(s) => Json::Str(s.clone()),
        ValueNode::Int(n) => Json::Num(*n as f64),
        ValueNode::Float(f) => Json::Num(*f),
        ValueNode::Bool(b) => Json::Bool(*b),
        ValueNode::Null => Json::Null,
        ValueNode::Enum(s) => Json::Str(s.clone()),
        ValueNode::Var(name) => vars.get(name).cloned().unwrap_or(Json::Null),
        ValueNode::List(values) => {
            Json::Arr(values.iter().map(|v| value_from_node(v, vars)).collect())
        }
        ValueNode::Object(fields) => Json::Obj(
            fields.iter().map(|(k, v)| (k.clone(), value_from_node(v, vars))).collect(),
        ),
    }
}

fn arg_value(field: &FieldNode, name: &str, vars: &Vars) -> Option<Json> {
    field
        .args
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| value_from_node(v, vars))
}

fn json_int(value: &Json) -> Option<u64> {
    match value {
        Json::Num(n) if n.is_finite() && *n >= 0.0 && n.fract() == 0.0 => Some(*n as u64),
        _ => None,
    }
}

fn first_arg(field: &FieldNode, vars: &Vars, default: usize) -> usize {
    match arg_value(field, "first", vars) {
        Some(Json::Num(n)) if n.is_finite() && n >= 0.0 => n.floor() as usize,
        _ => default,
    }
}

// -------------------------------------------------------------------------------------------
// base64url (untuk cursor koneksi; global id datang dari store)
// -------------------------------------------------------------------------------------------

const B64_ALPH: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn b64url_encode(data: &[u8]) -> String {
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64_ALPH[((n >> 18) & 63) as usize] as char);
        out.push(B64_ALPH[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(B64_ALPH[((n >> 6) & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(B64_ALPH[(n & 63) as usize] as char);
        }
    }
    out
}

fn b64url_val(c: u8) -> Option<u32> {
    match c {
        b'A'..=b'Z' => Some((c - b'A') as u32),
        b'a'..=b'z' => Some((c - b'a' + 26) as u32),
        b'0'..=b'9' => Some((c - b'0' + 52) as u32),
        b'-' => Some(62),
        b'_' => Some(63),
        _ => None,
    }
}

fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let mut vals: Vec<u32> = Vec::new();
        while i < bytes.len() && vals.len() < 4 {
            vals.push(b64url_val(bytes[i])?);
            i += 1;
        }
        if vals.len() < 2 {
            return None;
        }
        let n = (vals[0] << 18)
            | (vals[1] << 12)
            | (vals.get(2).copied().unwrap_or(0) << 6)
            | vals.get(3).copied().unwrap_or(0);
        out.push(((n >> 16) & 0xff) as u8);
        if vals.len() > 2 {
            out.push(((n >> 8) & 0xff) as u8);
        }
        if vals.len() > 3 {
            out.push((n & 0xff) as u8);
        }
    }
    Some(out)
}

fn cursor_for(seq: u64) -> String {
    b64url_encode(format!("cursor:{seq}").as_bytes())
}

fn seq_from_cursor(value: Option<Json>) -> Option<u64> {
    let text = match value {
        Some(Json::Str(s)) if !s.is_empty() => s,
        _ => return None,
    };
    let decoded = b64url_decode(&text)?;
    let decoded = String::from_utf8(decoded).ok()?;
    let rest = decoded.strip_prefix("cursor:")?;
    rest.parse::<u64>().ok()
}

// -------------------------------------------------------------------------------------------
// Daftar field schema (untuk introspeksi __type)
// -------------------------------------------------------------------------------------------

fn schema_fields(name: &str) -> Option<&'static [&'static str]> {
    match name {
        "Query" => Some(&["viewer", "repository", "node"]),
        "Mutation" => {
            Some(&["createIssue", "closeIssue", "createPullRequest", "mergePullRequest", "addComment"])
        }
        "Repository" => Some(&[
            "id", "databaseId", "name", "nameWithOwner", "owner", "description",
            "defaultBranchRef", "viewerPermission", "hasIssuesEnabled", "issue", "issues",
            "pullRequest", "pullRequests", "createdAt", "updatedAt", "url", "isPrivate",
        ]),
        "Issue" => Some(&[
            "id", "number", "title", "body", "state", "author", "createdAt", "updatedAt", "url",
            "repository",
        ]),
        "PullRequest" => Some(&[
            "id", "number", "title", "body", "state", "headRefName", "baseRefName", "headRef",
            "baseRef", "mergeable", "merged", "mergedAt", "files", "author", "createdAt",
            "updatedAt", "url", "repository",
        ]),
        "User" => Some(&["id", "login", "name", "email"]),
        "IssueConnection" => Some(&["nodes", "edges", "pageInfo", "totalCount"]),
        "PullRequestConnection" => Some(&["nodes", "edges", "pageInfo", "totalCount"]),
        "PullRequestFileConnection" => Some(&["nodes", "edges", "pageInfo", "totalCount"]),
        "PageInfo" => Some(&["hasNextPage", "hasPreviousPage", "startCursor", "endCursor"]),
        "Ref" => Some(&["name", "target"]),
        "IssueComment" => Some(&["id", "body", "author", "createdAt"]),
        "IssueCommentEdge" => Some(&["node", "cursor"]),
        "__Type" => Some(&["name", "kind", "description", "fields", "enumValues"]),
        "__Field" => Some(&["name", "description"]),
        "__EnumValue" => Some(&["name", "description"]),
        _ => None,
    }
}

fn schema_enums(name: &str) -> Option<&'static [&'static str]> {
    match name {
        "SearchType" => {
            Some(&["DISCUSSION", "ISSUE", "MILESTONE", "PULL_REQUEST", "REPOSITORY", "USER"])
        }
        "IssueState" => Some(&["OPEN", "CLOSED"]),
        "PullRequestState" => Some(&["OPEN", "CLOSED", "MERGED"]),
        "MergeableState" => Some(&["CONFLICTING", "MERGEABLE", "UNKNOWN"]),
        "RepositoryPermission" => Some(&["ADMIN", "MAINTAIN", "READ", "TRIAGE", "WRITE"]),
        _ => None,
    }
}

// -------------------------------------------------------------------------------------------
// Nilai hasil resolve
// -------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct PageInfo {
    has_next: bool,
    has_prev: bool,
    start: Option<String>,
    end: Option<String>,
}

#[derive(Clone, Debug)]
enum GqlObj {
    Viewer,
    User { login: String },
    Repository { key: String },
    Issue { repo_key: String, number: u64 },
    PullRequest { repo_key: String, number: u64 },
    IssueConnection { repo_key: String, items: Vec<(u64, u64)>, page_info: PageInfo },
    PullRequestConnection { repo_key: String, items: Vec<(u64, u64)>, page_info: PageInfo },
    PullRequestFileConnection { files: Vec<String> },
    PageInfo { info: PageInfo },
    Ref { name: String },
    Type { name: String },
    Comment { id: String, body: String },
    CommentEdge { id: String, body: String },
    CreateIssuePayload { repo_key: String, number: u64 },
    CloseIssuePayload { repo_key: String, number: u64 },
    CreatePullRequestPayload { repo_key: String, number: u64 },
    MergePullRequestPayload { repo_key: String, number: u64 },
    AddCommentPayload { id: String, body: String },
}

#[derive(Clone, Debug)]
enum Value {
    Json(Json),
    Obj(GqlObj),
    List(Vec<Value>),
    Record(Vec<(String, Value)>),
}

fn type_name_of(obj: &GqlObj) -> &'static str {
    match obj {
        GqlObj::Viewer => "User",
        GqlObj::User { .. } => "User",
        GqlObj::Repository { .. } => "Repository",
        GqlObj::Issue { .. } => "Issue",
        GqlObj::PullRequest { .. } => "PullRequest",
        GqlObj::IssueConnection { .. } => "IssueConnection",
        GqlObj::PullRequestConnection { .. } => "PullRequestConnection",
        GqlObj::PullRequestFileConnection { .. } => "PullRequestFileConnection",
        GqlObj::PageInfo { .. } => "PageInfo",
        GqlObj::Ref { .. } => "Ref",
        GqlObj::Type { .. } => "__Type",
        GqlObj::Comment { .. } => "IssueComment",
        GqlObj::CommentEdge { .. } => "IssueCommentEdge",
        GqlObj::CreateIssuePayload { .. } => "CreateIssuePayload",
        GqlObj::CloseIssuePayload { .. } => "CloseIssuePayload",
        GqlObj::CreatePullRequestPayload { .. } => "CreatePullRequestPayload",
        GqlObj::MergePullRequestPayload { .. } => "MergePullRequestPayload",
        GqlObj::AddCommentPayload { .. } => "AddCommentPayload",
    }
}

fn viewer_login() -> String {
    VIEWER.login.to_string()
}

fn obj_get<'a>(value: &'a Json, key: &str) -> Option<&'a Json> {
    match value {
        Json::Obj(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
        _ => None,
    }
}

/// File di sebuah commit, sebagai (path, oid, mode).
fn files_at(repo: &Repo, commit_oid: &str) -> Vec<(String, String, String)> {
    let obj = match repo.objects.get(commit_oid) {
        Some(o) if matches!(o.kind, ObjKind::Commit) => o,
        _ => return Vec::new(),
    };
    let info = parse_commit(&obj.data);
    let lookup = |oid: &str| repo.objects.get(oid).cloned();
    flatten_tree(&info.tree, &lookup)
}

/// Path yang berubah antara base dan head sebuah PR (hanya path yang dipakai
/// koneksi files GraphQL; additions/deletions tidak ditanyakan di sini).
fn changed_paths(repo: &Repo, pr: &PullRequest) -> Vec<String> {
    let base: BTreeMap<String, String> = if pr.base_sha.is_empty() {
        BTreeMap::new()
    } else {
        files_at(repo, &pr.base_sha).into_iter().map(|(p, o, _)| (p, o)).collect()
    };
    let head: BTreeMap<String, String> =
        files_at(repo, &pr.head_sha).into_iter().map(|(p, o, _)| (p, o)).collect();
    let mut paths: BTreeSet<String> = BTreeSet::new();
    paths.extend(base.keys().cloned());
    paths.extend(head.keys().cloned());
    paths.into_iter().filter(|p| base.get(p) != head.get(p)).collect()
}

/// Koneksi issues sebuah repo, mengikuti states, orderBy, first dan after.
fn issues_connection(repo: &Repo, field: &FieldNode, vars: &Vars) -> GqlObj {
    let mut items: Vec<&Issue> = repo.issues.values().collect();
    if let Some(Json::Arr(states)) = arg_value(field, "states", vars) {
        if !states.is_empty() {
            let wanted: Vec<String> = states
                .iter()
                .filter_map(|s| match s {
                    Json::Str(x) => Some(x.to_lowercase()),
                    _ => None,
                })
                .collect();
            items.retain(|issue| wanted.iter().any(|w| w == issue.state.as_str()));
        }
    }
    let mut asc = false;
    if let Some(Json::Obj(order)) = arg_value(field, "orderBy", vars) {
        if let Some(Json::Str(d)) = order.iter().find(|(k, _)| k == "direction").map(|(_, v)| v) {
            asc = d == "ASC";
        }
    }
    items.sort_by(|a, b| {
        if asc {
            a.created_seq.cmp(&b.created_seq)
        } else {
            b.created_seq.cmp(&a.created_seq)
        }
    });
    let after_seq = seq_from_cursor(arg_value(field, "after", vars));
    if let Some(seq) = after_seq {
        items.retain(|issue| {
            if asc {
                issue.created_seq > seq
            } else {
                issue.created_seq < seq
            }
        });
    }
    let limit = first_arg(field, vars, items.len());
    let has_next = items.len() > limit;
    let page: Vec<&Issue> = items.into_iter().take(limit).collect();
    let page_info = PageInfo {
        has_next,
        has_prev: after_seq.is_some(),
        start: page.first().map(|i| cursor_for(i.created_seq)),
        end: page.last().map(|i| cursor_for(i.created_seq)),
    };
    GqlObj::IssueConnection {
        repo_key: repo.full_name().to_string(),
        items: page.iter().map(|i| (i.number, i.created_seq)).collect(),
        page_info,
    }
}

/// Koneksi PR sebuah repo: tanpa states berarti hanya yang open.
fn pulls_connection(repo: &Repo, field: &FieldNode, vars: &Vars) -> GqlObj {
    let mut items: Vec<&PullRequest> = repo.pulls.values().collect();
    match arg_value(field, "states", vars) {
        Some(Json::Arr(states)) if !states.is_empty() => {
            let wanted: Vec<String> = states
                .iter()
                .filter_map(|s| match s {
                    Json::Str(x) => Some(x.to_lowercase()),
                    _ => None,
                })
                .collect();
            items.retain(|pr| wanted.iter().any(|w| w == pr.state.as_str()));
        }
        _ => items.retain(|pr| pr.state == State::Open),
    }
    items.sort_by(|a, b| b.created_seq.cmp(&a.created_seq));
    let limit = first_arg(field, vars, items.len());
    let has_next = items.len() > limit;
    let page: Vec<&PullRequest> = items.into_iter().take(limit).collect();
    let page_info = PageInfo {
        has_next,
        has_prev: false,
        start: page.first().map(|p| cursor_for(p.created_seq)),
        end: page.last().map(|p| cursor_for(p.created_seq)),
    };
    GqlObj::PullRequestConnection {
        repo_key: repo.full_name().to_string(),
        items: page.iter().map(|p| (p.number, p.created_seq)).collect(),
        page_info,
    }
}

/// Pecah kunci global "owner/name#N" menjadi (repo, nomor).
fn split_numbered_key(key: &str) -> Option<(String, u64)> {
    let idx = key.rfind('#')?;
    let number = key[idx + 1..].parse::<u64>().ok()?;
    Some((key[..idx].to_string(), number))
}

fn node_object(store: &Store, id: &str) -> Option<GqlObj> {
    let (kind, key) = decode_global_id(id)?;
    match kind.as_str() {
        "repo" => {
            if store.repos.contains_key(&key) {
                Some(GqlObj::Repository { key })
            } else {
                None
            }
        }
        "issue" => {
            let (repo_key, number) = split_numbered_key(&key)?;
            let repo = store.repos.get(&repo_key)?;
            if repo.issues.contains_key(&number) {
                Some(GqlObj::Issue { repo_key, number })
            } else {
                None
            }
        }
        "pull" => {
            let (repo_key, number) = split_numbered_key(&key)?;
            let repo = store.repos.get(&repo_key)?;
            if repo.pulls.contains_key(&number) {
                Some(GqlObj::PullRequest { repo_key, number })
            } else {
                None
            }
        }
        _ => None,
    }
}

// -------------------------------------------------------------------------------------------
// Executor
// -------------------------------------------------------------------------------------------

struct Exec<'a> {
    store: &'a mut Store,
    errors: Vec<String>,
}

impl<'a> Exec<'a> {
    fn resolve_field(
        &mut self,
        obj: &GqlObj,
        field: &FieldNode,
        vars: &Vars,
    ) -> Option<Value> {
        match obj {
            GqlObj::Viewer => match field.name.as_str() {
                "login" => Some(Value::Json(Json::Str(viewer_login()))),
                "id" => Some(Value::Json(Json::Str(global_id("user", &viewer_login())))),
                "name" => Some(Value::Json(Json::Str(VIEWER.name.to_string()))),
                "email" => Some(Value::Json(Json::Str(VIEWER.email.to_string()))),
                _ => None,
            },
            GqlObj::User { login } => match field.name.as_str() {
                "login" => Some(Value::Json(Json::Str(login.clone()))),
                "id" => Some(Value::Json(Json::Str(global_id("user", login)))),
                "name" => Some(Value::Json(Json::Str(
                    if login == &viewer_login() { VIEWER.name.to_string() } else { login.clone() },
                ))),
                "email" => Some(if login == &viewer_login() {
                    Value::Json(Json::Str(VIEWER.email.to_string()))
                } else {
                    Value::Json(Json::Null)
                }),
                _ => None,
            },
            GqlObj::Repository { key } => {
                let repo = self.store.repos.get(key)?;
                match field.name.as_str() {
                    "id" => Some(Value::Json(Json::Str(repo.global_id()))),
                    "databaseId" => Some(Value::Json(Json::Num(repo.database_id as f64))),
                    "name" => Some(Value::Json(Json::Str(repo.name.to_string()))),
                    "nameWithOwner" => Some(Value::Json(Json::Str(repo.full_name().to_string()))),
                    "description" => Some(Value::Json(Json::Str(repo.description.to_string()))),
                    "isPrivate" => Some(Value::Json(Json::Bool(repo.is_private))),
                    "createdAt" | "updatedAt" => {
                        Some(Value::Json(Json::Str(FIXED_ISO.to_string())))
                    }
                    "url" => Some(Value::Json(Json::Str(format!(
                        "https://example.invalid/{}",
                        repo.full_name()
                    )))),
                    "owner" => Some(Value::Obj(GqlObj::User { login: repo.owner.to_string() })),
                    "defaultBranchRef" => {
                        if repo.default_tip().is_some() {
                            Some(Value::Obj(GqlObj::Ref { name: repo.default_branch.to_string() }))
                        } else {
                            Some(Value::Json(Json::Null))
                        }
                    }
                    "viewerPermission" => Some(Value::Json(Json::Str("ADMIN".to_string()))),
                    "hasIssuesEnabled" => Some(Value::Json(Json::Bool(true))),
                    "issue" => {
                        let number = match arg_value(field, "number", vars) {
                            Some(v) => match json_int(&v) {
                                Some(n) => n,
                                None => return Some(Value::Json(Json::Null)),
                            },
                            None => return Some(Value::Json(Json::Null)),
                        };
                        if repo.issues.contains_key(&number) {
                            Some(Value::Obj(GqlObj::Issue {
                                repo_key: key.clone(),
                                number,
                            }))
                        } else {
                            Some(Value::Json(Json::Null))
                        }
                    }
                    "issues" => Some(Value::Obj(issues_connection(repo, field, vars))),
                    "pullRequest" => {
                        let number = match arg_value(field, "number", vars) {
                            Some(v) => match json_int(&v) {
                                Some(n) => n,
                                None => return Some(Value::Json(Json::Null)),
                            },
                            None => return Some(Value::Json(Json::Null)),
                        };
                        if repo.pulls.contains_key(&number) {
                            Some(Value::Obj(GqlObj::PullRequest {
                                repo_key: key.clone(),
                                number,
                            }))
                        } else {
                            Some(Value::Json(Json::Null))
                        }
                    }
                    "pullRequests" => Some(Value::Obj(pulls_connection(repo, field, vars))),
                    _ => None,
                }
            }
            GqlObj::Issue { repo_key, number } => {
                let repo = self.store.repos.get(repo_key)?;
                let issue = repo.issues.get(number)?;
                match field.name.as_str() {
                    "id" => Some(Value::Json(Json::Str(global_id(
                        "issue",
                        &format!("{repo_key}#{number}"),
                    )))),
                    "number" => Some(Value::Json(Json::Num(issue.number as f64))),
                    "title" => Some(Value::Json(Json::Str(issue.title.to_string()))),
                    "body" => Some(Value::Json(Json::Str(issue.body.to_string()))),
                    "state" => Some(Value::Json(Json::Str(
                        if issue.state == State::Open { "OPEN".to_string() } else { "CLOSED".to_string() },
                    ))),
                    "author" => Some(Value::Obj(GqlObj::User { login: viewer_login() })),
                    "createdAt" | "updatedAt" => {
                        Some(Value::Json(Json::Str(FIXED_ISO.to_string())))
                    }
                    "url" => Some(Value::Json(Json::Str(format!(
                        "https://example.invalid/{repo_key}/issues/{number}"
                    )))),
                    "repository" => {
                        Some(Value::Obj(GqlObj::Repository { key: repo_key.clone() }))
                    }
                    _ => None,
                }
            }
            GqlObj::PullRequest { repo_key, number } => {
                let repo = self.store.repos.get(repo_key)?;
                let pr = repo.pulls.get(number)?;
                match field.name.as_str() {
                    "id" => Some(Value::Json(Json::Str(global_id(
                        "pull",
                        &format!("{repo_key}#{number}"),
                    )))),
                    "number" => Some(Value::Json(Json::Num(pr.number as f64))),
                    "title" => Some(Value::Json(Json::Str(pr.title.to_string()))),
                    "body" => Some(Value::Json(Json::Str(pr.body.to_string()))),
                    "state" => Some(Value::Json(Json::Str(
                        if pr.state == State::Open { "OPEN".to_string() } else { "CLOSED".to_string() },
                    ))),
                    "headRefName" => Some(Value::Json(Json::Str(pr.head_ref.to_string()))),
                    "baseRefName" => Some(Value::Json(Json::Str(pr.base_ref.to_string()))),
                    "headRef" => Some(Value::Obj(GqlObj::Ref { name: pr.head_ref.to_string() })),
                    "baseRef" => Some(Value::Obj(GqlObj::Ref { name: pr.base_ref.to_string() })),
                    "mergeable" => Some(Value::Json(Json::Str(
                        if merge_would_conflict(repo, pr) {
                            "CONFLICTING".to_string()
                        } else {
                            "MERGEABLE".to_string()
                        },
                    ))),
                    "merged" => Some(Value::Json(Json::Bool(pr.merged))),
                    "mergedAt" => Some(match &pr.merged_at {
                        Some(t) => Value::Json(Json::Str(t.to_string())),
                        None => Value::Json(Json::Null),
                    }),
                    "author" => Some(Value::Obj(GqlObj::User { login: viewer_login() })),
                    "createdAt" | "updatedAt" => {
                        Some(Value::Json(Json::Str(FIXED_ISO.to_string())))
                    }
                    "url" => Some(Value::Json(Json::Str(format!(
                        "https://example.invalid/{repo_key}/pull/{number}"
                    )))),
                    "repository" => {
                        Some(Value::Obj(GqlObj::Repository { key: repo_key.clone() }))
                    }
                    "files" => {
                        let limit = first_arg(field, vars, 100);
                        let files: Vec<String> =
                            changed_paths(repo, pr).into_iter().take(limit).collect();
                        Some(Value::Obj(GqlObj::PullRequestFileConnection { files }))
                    }
                    _ => None,
                }
            }
            GqlObj::IssueConnection { repo_key, items, page_info } => match field.name.as_str() {
                "nodes" => Some(Value::List(
                    items
                        .iter()
                        .map(|(n, _)| {
                            Value::Obj(GqlObj::Issue { repo_key: repo_key.clone(), number: *n })
                        })
                        .collect(),
                )),
                "edges" => Some(Value::List(
                    items
                        .iter()
                        .map(|(n, seq)| {
                            Value::Record(vec![
                                (
                                    "node".to_string(),
                                    Value::Obj(GqlObj::Issue {
                                        repo_key: repo_key.clone(),
                                        number: *n,
                                    }),
                                ),
                                ("cursor".to_string(), Value::Json(Json::Str(cursor_for(*seq)))),
                            ])
                        })
                        .collect(),
                )),
                "pageInfo" => Some(Value::Obj(GqlObj::PageInfo { info: page_info.clone() })),
                "totalCount" => Some(Value::Json(Json::Num(items.len() as f64))),
                _ => None,
            },
            GqlObj::PullRequestConnection { repo_key, items, page_info } => {
                match field.name.as_str() {
                    "nodes" => Some(Value::List(
                        items
                            .iter()
                            .map(|(n, _)| {
                                Value::Obj(GqlObj::PullRequest {
                                    repo_key: repo_key.clone(),
                                    number: *n,
                                })
                            })
                            .collect(),
                    )),
                    "edges" => Some(Value::List(
                        items
                            .iter()
                            .map(|(n, seq)| {
                                Value::Record(vec![
                                    (
                                        "node".to_string(),
                                        Value::Obj(GqlObj::PullRequest {
                                            repo_key: repo_key.clone(),
                                            number: *n,
                                        }),
                                    ),
                                    (
                                        "cursor".to_string(),
                                        Value::Json(Json::Str(cursor_for(*seq))),
                                    ),
                                ])
                            })
                            .collect(),
                    )),
                    "pageInfo" => Some(Value::Obj(GqlObj::PageInfo { info: page_info.clone() })),
                    "totalCount" => Some(Value::Json(Json::Num(items.len() as f64))),
                    _ => None,
                }
            }
            GqlObj::PullRequestFileConnection { files } => match field.name.as_str() {
                "nodes" => Some(Value::List(
                    files
                        .iter()
                        .map(|p| {
                            Value::Record(vec![(
                                "path".to_string(),
                                Value::Json(Json::Str(p.clone())),
                            )])
                        })
                        .collect(),
                )),
                "edges" => Some(Value::List(
                    files
                        .iter()
                        .map(|p| {
                            Value::Record(vec![
                                (
                                    "node".to_string(),
                                    Value::Record(vec![(
                                        "path".to_string(),
                                        Value::Json(Json::Str(p.clone())),
                                    )]),
                                ),
                                ("cursor".to_string(), Value::Json(Json::Str(cursor_for(0)))),
                            ])
                        })
                        .collect(),
                )),
                "pageInfo" => Some(Value::Obj(GqlObj::PageInfo {
                    info: PageInfo { has_next: false, has_prev: false, start: None, end: None },
                })),
                "totalCount" => Some(Value::Json(Json::Num(files.len() as f64))),
                _ => None,
            },
            GqlObj::PageInfo { info } => match field.name.as_str() {
                "hasNextPage" => Some(Value::Json(Json::Bool(info.has_next))),
                "hasPreviousPage" => Some(Value::Json(Json::Bool(info.has_prev))),
                "startCursor" => Some(match &info.start {
                    Some(c) => Value::Json(Json::Str(c.clone())),
                    None => Value::Json(Json::Null),
                }),
                "endCursor" => Some(match &info.end {
                    Some(c) => Value::Json(Json::Str(c.clone())),
                    None => Value::Json(Json::Null),
                }),
                _ => None,
            },
            GqlObj::Ref { name } => match field.name.as_str() {
                "name" => Some(Value::Json(Json::Str(name.clone()))),
                "target" => Some(Value::Json(Json::Null)),
                _ => None,
            },
            GqlObj::Type { name } => match field.name.as_str() {
                "name" => Some(Value::Json(Json::Str(name.clone()))),
                "kind" => Some(Value::Json(Json::Str(
                    if schema_enums(name).is_some() { "ENUM".to_string() } else { "OBJECT".to_string() },
                ))),
                "description" => Some(Value::Json(Json::Null)),
                "fields" => match schema_fields(name) {
                    Some(fields) => Some(Value::List(
                        fields
                            .iter()
                            .map(|f| {
                                Value::Record(vec![
                                    ("name".to_string(), Value::Json(Json::Str(f.to_string()))),
                                    ("description".to_string(), Value::Json(Json::Null)),
                                ])
                            })
                            .collect(),
                    )),
                    None => Some(Value::Json(Json::Null)),
                },
                "enumValues" => match schema_enums(name) {
                    Some(values) => Some(Value::List(
                        values
                            .iter()
                            .map(|v| {
                                Value::Record(vec![
                                    ("name".to_string(), Value::Json(Json::Str(v.to_string()))),
                                    ("description".to_string(), Value::Json(Json::Null)),
                                ])
                            })
                            .collect(),
                    )),
                    None => Some(Value::Json(Json::Null)),
                },
                _ => None,
            },
            GqlObj::Comment { id, body } => match field.name.as_str() {
                "id" => Some(Value::Json(Json::Str(id.clone()))),
                "body" => Some(Value::Json(Json::Str(body.clone()))),
                "author" => Some(Value::Obj(GqlObj::User { login: viewer_login() })),
                "createdAt" => Some(Value::Json(Json::Str(FIXED_ISO.to_string()))),
                _ => None,
            },
            GqlObj::CommentEdge { id, body } => match field.name.as_str() {
                "node" => Some(Value::Obj(GqlObj::Comment { id: id.clone(), body: body.clone() })),
                "cursor" => Some(Value::Json(Json::Str(cursor_for(0)))),
                _ => None,
            },
            GqlObj::CreateIssuePayload { repo_key, number }
            | GqlObj::CloseIssuePayload { repo_key, number } => match field.name.as_str() {
                "issue" => Some(Value::Obj(GqlObj::Issue { repo_key: repo_key.clone(), number: *number })),
                _ => None,
            },
            GqlObj::CreatePullRequestPayload { repo_key, number }
            | GqlObj::MergePullRequestPayload { repo_key, number } => {
                match field.name.as_str() {
                    "pullRequest" => Some(Value::Obj(GqlObj::PullRequest {
                        repo_key: repo_key.clone(),
                        number: *number,
                    })),
                    _ => None,
                }
            }
            GqlObj::AddCommentPayload { id, body } => match field.name.as_str() {
                "commentEdge" => {
                    Some(Value::Obj(GqlObj::CommentEdge { id: id.clone(), body: body.clone() }))
                }
                _ => None,
            },
        }
    }

    fn resolve_value(
        &mut self,
        value: Value,
        selections: Option<&[SelNode]>,
        vars: &Vars,
    ) -> Json {
        match value {
            Value::Json(j) => j,
            Value::List(items) => Json::Arr(
                items.into_iter().map(|v| self.resolve_value(v, selections, vars)).collect(),
            ),
            Value::Record(entries) => {
                let Some(sels) = selections else {
                    return Json::Obj(
                        entries
                            .iter()
                            .map(|(k, v)| {
                                (
                                    k.clone(),
                                    match v {
                                        Value::Json(j) => j.clone(),
                                        _ => Json::Null,
                                    },
                                )
                            })
                            .collect(),
                    );
                };
                let mut out: Vec<(String, Json)> = Vec::new();
                for sel in sels {
                    let SelNode::Field(f) = sel else { continue };
                    let found = entries.iter().find(|(k, _)| *k == f.name);
                    let j = match found {
                        Some((_, v)) => {
                            self.resolve_value(v.clone(), f.selections.as_deref(), vars)
                        }
                        None => Json::Null,
                    };
                    out.push((f.alias.clone(), j));
                }
                Json::Obj(out)
            }
            Value::Obj(obj) => {
                let Some(sels) = selections else {
                    return Json::Null;
                };
                self.execute_selections(&obj, sels, vars)
            }
        }
    }

    fn execute_selections(
        &mut self,
        obj: &GqlObj,
        selections: &[SelNode],
        vars: &Vars,
    ) -> Json {
        let mut out: Vec<(String, Json)> = Vec::new();
        let type_name = type_name_of(obj);
        for sel in selections {
            match sel {
                SelNode::Fragment { cond, selections: inner } => {
                    if let Some(c) = cond {
                        if c != type_name {
                            continue;
                        }
                    }
                    if let Json::Obj(fields) = self.execute_selections(obj, inner, vars) {
                        out.extend(fields);
                    }
                }
                SelNode::Field(f) => {
                    if f.name == "__typename" {
                        out.push((f.alias.clone(), Json::Str(type_name.to_string())));
                        continue;
                    }
                    match self.resolve_field(obj, f, vars) {
                        Some(value) => {
                            let j = self.resolve_value(value, f.selections.as_deref(), vars);
                            out.push((f.alias.clone(), j));
                        }
                        None => {
                            self.errors.push(format!(
                                "Cannot query field \"{}\" on type \"{}\".",
                                f.name, type_name
                            ));
                            out.push((f.alias.clone(), Json::Null));
                        }
                    }
                }
            }
        }
        Json::Obj(out)
    }

    fn execute_root_field(
        &mut self,
        operation: &Operation,
        field: &FieldNode,
        vars: &Vars,
    ) -> Option<(String, Json)> {
        let alias = field.alias.clone();
        if operation.op == Op::Query {
            match field.name.as_str() {
                "viewer" => {
                    let value =
                        self.resolve_value(Value::Obj(GqlObj::Viewer), field.selections.as_deref(), vars);
                    return Some((alias, value));
                }
                "repository" => {
                    let owner = arg_value(field, "owner", vars);
                    let name = arg_value(field, "name", vars);
                    let (owner, name) = match (owner, name) {
                        (Some(Json::Str(o)), Some(Json::Str(n))) => (o, n),
                        _ => return Some((alias, Json::Null)),
                    };
                    let key = format!("{owner}/{name}");
                    if self.store.repos.contains_key(&key) {
                        let value = self.resolve_value(
                            Value::Obj(GqlObj::Repository { key }),
                            field.selections.as_deref(),
                            vars,
                        );
                        return Some((alias, value));
                    }
                    return Some((alias, Json::Null));
                }
                "node" => {
                    let id = arg_value(field, "id", vars);
                    let obj = match id {
                        Some(Json::Str(s)) => node_object(self.store, &s),
                        _ => None,
                    };
                    let value = match obj {
                        Some(o) => {
                            self.resolve_value(Value::Obj(o), field.selections.as_deref(), vars)
                        }
                        None => Json::Null,
                    };
                    return Some((alias, value));
                }
                "__type" => {
                    let name = arg_value(field, "name", vars);
                    let name = match name {
                        Some(Json::Str(s)) => s,
                        _ => String::new(),
                    };
                    let known =
                        schema_fields(&name).is_some() || schema_enums(&name).is_some();
                    if !known {
                        return Some((alias, Json::Null));
                    }
                    let value = self.resolve_value(
                        Value::Obj(GqlObj::Type { name }),
                        field.selections.as_deref(),
                        vars,
                    );
                    return Some((alias, value));
                }
                _ => return None,
            }
        }

        // Mutasi. `input` dibaca sebagai objek JSON atau objek kosong.
        let input = arg_value(field, "input", vars).unwrap_or(Json::Null);
        let input_str = |name: &str| -> Option<String> {
            match obj_get(&input, name) {
                Some(Json::Str(s)) => Some(s.clone()),
                _ => None,
            }
        };
        match field.name.as_str() {
            "createIssue" => {
                let repo_key = match input_str("repositoryId") {
                    Some(id) => match decode_global_id(&id) {
                        Some((kind, key)) if kind == "repo" => key,
                        _ => {
                            self.errors.push("createIssue: repository not found".to_string());
                            return Some((alias, Json::Null));
                        }
                    },
                    None => {
                        self.errors.push("createIssue: repository not found".to_string());
                        return Some((alias, Json::Null));
                    }
                };
                let number = {
                    let repo = match self.store.repos.get_mut(&repo_key) {
                        Some(r) => r,
                        None => {
                            self.errors.push("createIssue: repository not found".to_string());
                            return Some((alias, Json::Null));
                        }
                    };
                    let number = repo.next_issue_number;
                    repo.next_issue_number += 1;
                    let issue = Issue {
                        id: number,
                        number,
                        title: input_str("title").unwrap_or_default(),
                        body: input_str("body").unwrap_or_default(),
                        state: State::Open,
                        labels: Vec::new(),
                        created_seq: repo.next_seq(),
                        comments: Vec::new(),
                        rev: 0,
                        json_cache: None,
                    };
                    repo.issues.insert(number, issue);
                    number
                };
                let value = self.resolve_value(
                    Value::Obj(GqlObj::CreateIssuePayload { repo_key, number }),
                    field.selections.as_deref(),
                    vars,
                );
                Some((alias, value))
            }
            "closeIssue" => {
                let (repo_key, number) = match input_str("issueId") {
                    Some(id) => match decode_global_id(&id) {
                        Some((kind, key)) if kind == "issue" => match split_numbered_key(&key) {
                            Some(pair) => pair,
                            None => {
                                self.errors.push("closeIssue: issue not found".to_string());
                                return Some((alias, Json::Null));
                            }
                        },
                        _ => {
                            self.errors.push("closeIssue: issue not found".to_string());
                            return Some((alias, Json::Null));
                        }
                    },
                    None => {
                        self.errors.push("closeIssue: issue not found".to_string());
                        return Some((alias, Json::Null));
                    }
                };
                let found = match self.store.repos.get_mut(&repo_key) {
                    Some(r) => match r.issues.get_mut(&number) {
                        Some(issue) => {
                            issue.state = State::Closed;
                            issue.rev += 1;
                            issue.json_cache = None;
                            true
                        }
                        None => false,
                    },
                    None => false,
                };
                if !found {
                    self.errors.push("closeIssue: issue not found".to_string());
                    return Some((alias, Json::Null));
                }
                let value = self.resolve_value(
                    Value::Obj(GqlObj::CloseIssuePayload { repo_key, number }),
                    field.selections.as_deref(),
                    vars,
                );
                Some((alias, value))
            }
            "createPullRequest" => {
                let repo_key = match input_str("repositoryId") {
                    Some(id) => match decode_global_id(&id) {
                        Some((kind, key)) if kind == "repo" => key,
                        _ => {
                            self.errors
                                .push("createPullRequest: repository or branch not found".to_string());
                            return Some((alias, Json::Null));
                        }
                    },
                    None => {
                        self.errors
                            .push("createPullRequest: repository or branch not found".to_string());
                        return Some((alias, Json::Null));
                    }
                };
                let head_ref = input_str("headRefName").unwrap_or_default();
                let base_ref = input_str("baseRefName").unwrap_or_default();
                let number = {
                    let repo = match self.store.repos.get_mut(&repo_key) {
                        Some(r) => r,
                        None => {
                            self.errors
                                .push("createPullRequest: repository or branch not found".to_string());
                            return Some((alias, Json::Null));
                        }
                    };
                    let head_sha = repo.branch_tip(&head_ref).map(|s| s.to_string());
                    let base_sha = repo.branch_tip(&base_ref).map(|s| s.to_string());
                    let (head_sha, base_sha) = match (head_sha, base_sha) {
                        (Some(h), Some(b)) => (h, b),
                        _ => {
                            self.errors
                                .push("createPullRequest: repository or branch not found".to_string());
                            return Some((alias, Json::Null));
                        }
                    };
                    let number = repo.next_pull_number;
                    repo.next_pull_number += 1;
                    let pr = PullRequest {
                        id: number,
                        number,
                        title: input_str("title").unwrap_or_default(),
                        body: input_str("body").unwrap_or_default(),
                        state: State::Open,
                        head_ref: head_ref.clone(),
                        base_ref: base_ref.clone(),
                        head_sha,
                        base_sha,
                        merged: false,
                        merged_at: None,
                        merge_commit_sha: None,
                        created_seq: repo.next_seq(),
                    };
                    repo.pulls.insert(number, pr);
                    number
                };
                let value = self.resolve_value(
                    Value::Obj(GqlObj::CreatePullRequestPayload { repo_key, number }),
                    field.selections.as_deref(),
                    vars,
                );
                Some((alias, value))
            }
            "mergePullRequest" => {
                let (repo_key, number) = match input_str("pullRequestId") {
                    Some(id) => match decode_global_id(&id) {
                        Some((kind, key)) if kind == "pull" => match split_numbered_key(&key) {
                            Some(pair) => pair,
                            None => {
                                self.errors.push("mergePullRequest: pull request not found".to_string());
                                return Some((alias, Json::Null));
                            }
                        },
                        _ => {
                            self.errors.push("mergePullRequest: pull request not found".to_string());
                            return Some((alias, Json::Null));
                        }
                    },
                    None => {
                        self.errors.push("mergePullRequest: pull request not found".to_string());
                        return Some((alias, Json::Null));
                    }
                };
                // Keluarkan PR dari map selama merge agar repo dan PR bisa
                // dipinjam mutabel bersamaan; kembalikan apa pun hasilnya.
                let mut pr = match self.store.repos.get_mut(&repo_key) {
                    Some(repo) => match repo.pulls.remove(&number) {
                        Some(p) => p,
                        None => {
                            self.errors.push("mergePullRequest: pull request not found".to_string());
                            return Some((alias, Json::Null));
                        }
                    },
                    None => {
                        self.errors.push("mergePullRequest: pull request not found".to_string());
                        return Some((alias, Json::Null));
                    }
                };
                if pr.state != State::Open {
                    let num = pr.number;
                    if let Some(repo) = self.store.repos.get_mut(&repo_key) {
                        repo.pulls.insert(num, pr);
                    }
                    self.errors.push("mergePullRequest: pull request is not open".to_string());
                    return Some((alias, Json::Null));
                }
                let merged = match self.store.repos.get_mut(&repo_key) {
                    Some(repo) => squash_merge(repo, &mut pr),
                    None => Err(vec!["repository not found".to_string()]),
                };
                let num = pr.number;
                if let Some(repo) = self.store.repos.get_mut(&repo_key) {
                    repo.pulls.insert(num, pr);
                }
                if merged.is_err() {
                    self.errors.push("mergePullRequest: merge conflict".to_string());
                    return Some((alias, Json::Null));
                }
                let value = self.resolve_value(
                    Value::Obj(GqlObj::MergePullRequestPayload { repo_key, number }),
                    field.selections.as_deref(),
                    vars,
                );
                Some((alias, value))
            }
            "addComment" => {
                let (repo_key, number) = match input_str("subjectId") {
                    Some(id) => match decode_global_id(&id) {
                        Some((kind, key)) if kind == "issue" => match split_numbered_key(&key) {
                            Some(pair) => pair,
                            None => {
                                self.errors.push("addComment: subject not found".to_string());
                                return Some((alias, Json::Null));
                            }
                        },
                        _ => {
                            self.errors.push("addComment: subject not found".to_string());
                            return Some((alias, Json::Null));
                        }
                    },
                    None => {
                        self.errors.push("addComment: subject not found".to_string());
                        return Some((alias, Json::Null));
                    }
                };
                let body = input_str("body").unwrap_or_default();
                let (comment_id, comment_body) = {
                    let repo = match self.store.repos.get_mut(&repo_key) {
                        Some(r) => r,
                        None => {
                            self.errors.push("addComment: subject not found".to_string());
                            return Some((alias, Json::Null));
                        }
                    };
                    let comment_id = repo.next_comment_id;
                    repo.next_comment_id += 1;
                    let issue = match repo.issues.get_mut(&number) {
                        Some(i) => i,
                        None => {
                            self.errors.push("addComment: subject not found".to_string());
                            return Some((alias, Json::Null));
                        }
                    };
                    issue.comments.push(Comment { id: comment_id, body: body.clone() });
                    issue.rev += 1;
                    issue.json_cache = None;
                    let gid = global_id(
                        "comment",
                        &format!("{}#{}#{}", repo.full_name(), number, comment_id),
                    );
                    (gid, body.clone())
                };
                let value = self.resolve_value(
                    Value::Obj(GqlObj::AddCommentPayload { id: comment_id, body: comment_body }),
                    field.selections.as_deref(),
                    vars,
                );
                Some((alias, value))
            }
            _ => None,
        }
    }

    fn execute_operation(
        &mut self,
        operation: &Operation,
        vars: &Vars,
    ) -> Json {
        let mut data: Vec<(String, Json)> = Vec::new();
        let root_type = if operation.op == Op::Query { "Query" } else { "Mutation" };
        for selection in &operation.selections {
            let SelNode::Field(field) = selection else { continue };
            match self.execute_root_field(operation, field, vars) {
                Some((alias, value)) => data.push((alias, value)),
                None => {
                    self.errors.push(format!(
                        "Cannot query field \"{}\" on type \"{}\".",
                        field.name, root_type
                    ));
                    data.push((field.alias.clone(), Json::Null));
                }
            }
        }
        let mut out: Vec<(String, Json)> = vec![("data".to_string(), Json::Obj(data))];
        if !self.errors.is_empty() {
            out.push((
                "errors".to_string(),
                Json::Arr(
                    self.errors
                        .iter()
                        .map(|m| {
                            Json::Obj(vec![("message".to_string(), Json::Str(m.clone()))])
                        })
                        .collect(),
                ),
            ));
        }
        Json::Obj(out)
    }
}

fn execute_one(store: &mut Store, operation: &Operation, vars: Vars) -> Json {
    let mut exec = Exec { store, errors: Vec::new() };
    exec.execute_operation(operation, &vars)
}

// -------------------------------------------------------------------------------------------
// Handler HTTP
// -------------------------------------------------------------------------------------------

fn err_result(message: &str) -> Json {
    Json::Obj(vec![(
        "errors".to_string(),
        Json::Arr(vec![Json::Obj(vec![("message".to_string(), Json::Str(message.to_string()))])]),
    )])
}

fn header<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    req.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// Tangani satu request GraphQL. None berarti method bukan POST (app lanjut
/// ke rute lain / 404).
pub fn handle_graphql(store: &mut Store, req: &Request) -> Option<Response> {
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
            return Some(json_response(store, 400, err_result("Request body is not valid JSON")));
        }
    };
    let is_batch = matches!(body, Json::Arr(_));
    let items: Vec<Json> = match body {
        Json::Arr(items) => items,
        other => vec![other],
    };
    let mut results: Vec<Json> = Vec::new();
    for item in &items {
        let query = obj_get(item, "query");
        let variables = obj_get(item, "variables");
        let query = match query {
            Some(Json::Str(q)) if !q.trim().is_empty() => q.clone(),
            _ => {
                results.push(err_result("Must provide query string."));
                continue;
            }
        };
        let operations = match Parser::new(&query).parse_document() {
            Ok(ops) => ops,
            Err(msg) => {
                results.push(err_result(&msg));
                continue;
            }
        };
        let Some(operation) = operations.into_iter().next() else {
            results.push(err_result("Must provide an operation."));
            continue;
        };
        let mut vars: Vars = HashMap::new();
        for (name, default) in &operation.vars {
            let provided = match variables {
                Some(Json::Obj(fields)) => {
                    fields.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
                }
                _ => None,
            };
            if let Some(v) = provided {
                vars.insert(name.clone(), v);
            } else if let Some(d) = default {
                vars.insert(name.clone(), value_from_node(d, &HashMap::new()));
            }
        }
        results.push(execute_one(store, &operation, vars));
    }
    if is_batch {
        Some(json_response(store, 200, Json::Arr(results)))
    } else {
        let first = results.into_iter().next().unwrap_or_else(|| err_result("Must provide an operation."));
        Some(json_response(store, 200, first))
    }
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
            path: "/api/graphql".to_string(),
            query: String::new(),
            headers: vec![("authorization".to_string(), "Bearer test".to_string())],
            body: body.as_bytes().to_vec(),
        }
    }

    fn body_json(resp: &Response) -> Json {
        let text = String::from_utf8_lossy(&resp.body).to_string();
        Json::parse(&text).expect("response is JSON")
    }

    #[test]
    fn parse_viewer_document() {
        let ops = Parser::new("query ViewerLogin {\n  viewer {\n    login\n  }\n}")
            .parse_document()
            .expect("parses");
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].op, Op::Query);
        assert_eq!(ops[0].name.as_deref(), Some("ViewerLogin"));
        assert_eq!(ops[0].selections.len(), 1);
    }

    #[test]
    fn parse_mutation_with_input_object() {
        let ops = Parser::new(
            "mutation IssueCreate($input: CreateIssueInput!) { createIssue(input: $input) { issue { number id } } }",
        )
        .parse_document()
        .expect("parses");
        assert_eq!(ops[0].op, Op::Mutation);
        assert_eq!(ops[0].vars.len(), 1);
        assert_eq!(ops[0].vars[0].0, "input");
    }

    #[test]
    fn parse_inline_fragment_and_alias() {
        let ops = Parser::new(
            "query NodeRepository($id: ID!) { node(id: $id) { id ... on Repository { name nameWithOwner } } }",
        )
        .parse_document()
        .expect("parses");
        assert_eq!(ops[0].selections.len(), 1);
        let ops2 = Parser::new(
            "query Issue_fields { Issue: __type(name: \"Issue\") { fields(includeDeprecated: true) { name } } }",
        )
        .parse_document()
        .expect("parses alias");
        assert_eq!(ops2[0].selections.len(), 1);
        if let SelNode::Field(f) = &ops2[0].selections[0] {
            assert_eq!(f.alias, "Issue");
            assert_eq!(f.name, "__type");
        } else {
            panic!("expected field");
        }
    }

    #[test]
    fn viewer_query_end_to_end() {
        let mut store = Store::new();
        store.create_repo("adaiversen", "demo", "demo repo", false);
        let req = post_req(r#"{"query":"query ViewerLogin { viewer { login } }"}"#);
        let resp = handle_graphql(&mut store, &req).expect("handled");
        assert_eq!(resp.status, 200);
        let text = String::from_utf8_lossy(&resp.body).to_string();
        assert!(text.contains("adaiversen"), "body: {text}");
        assert!(text.contains("\"login\""), "body: {text}");
    }

    #[test]
    fn repository_and_introspection_end_to_end() {
        let mut store = Store::new();
        store.create_repo("adaiversen", "demo", "demo repo", false);
        let req = post_req(
            r#"{"query":"query RepositoryFields($owner: String!, $name: String!) { repository(owner: $owner, name: $name) { id databaseId name nameWithOwner owner { login } viewerPermission hasIssuesEnabled } }","variables":{"owner":"adaiversen","name":"demo"}}"#,
        );
        let resp = handle_graphql(&mut store, &req).expect("handled");
        assert_eq!(resp.status, 200);
        let text = String::from_utf8_lossy(&resp.body).to_string();
        assert!(text.contains("\"name\":\"demo\""), "body: {text}");
        assert!(text.contains("\"viewerPermission\":\"ADMIN\""), "body: {text}");
        let req2 = post_req(
            r#"{"query":"query SearchType_enumValues { SearchType: __type(name: \"SearchType\") { enumValues(includeDeprecated: true) { name } } }"}"#,
        );
        let resp2 = handle_graphql(&mut store, &req2).expect("handled");
        let text2 = String::from_utf8_lossy(&resp2.body).to_string();
        assert!(text2.contains("PULL_REQUEST"), "body: {text2}");
    }

    #[test]
    fn invalid_field_reports_error_with_200() {
        let mut store = Store::new();
        let req = post_req(
            r#"{"query":"query ViewerInvalidField { viewer { login noSuchFieldOnViewer } }"}"#,
        );
        let resp = handle_graphql(&mut store, &req).expect("handled");
        assert_eq!(resp.status, 200);
        let text = String::from_utf8_lossy(&resp.body).to_string();
        assert!(text.contains("noSuchFieldOnViewer"), "body: {text}");
        assert!(text.contains("errors"), "body: {text}");
        assert!(text.contains("\"login\":\"adaiversen\""), "body: {text}");
    }

    #[test]
    fn create_issue_mutation_end_to_end() {
        let mut store = Store::new();
        let repo = store.create_repo("adaiversen", "demo", "demo repo", false);
        let repo_id = repo.global_id();
        let body = format!(
            "{{\"query\":\"mutation IssueCreate($input: CreateIssueInput!) {{ createIssue(input: $input) {{ issue {{ number id }} }} }}\",\"variables\":{{\"input\":{{\"repositoryId\":\"{repo_id}\",\"title\":\"hello\",\"body\":\"world\"}}}}}}"
        );
        let req = post_req(&body);
        let resp = handle_graphql(&mut store, &req).expect("handled");
        assert_eq!(resp.status, 200);
        let text = String::from_utf8_lossy(&resp.body).to_string();
        assert!(text.contains("\"number\":1"), "body: {text}");
        // Baca balik lewat query issues.
        let req2 = post_req(
            r#"{"query":"query OpenIssues($owner: String!, $name: String!, $first: Int!) { repository(owner: $owner, name: $name) { issues(first: $first, states: [OPEN], orderBy: {field: CREATED_AT, direction: DESC}) { nodes { number title state id } } } }","variables":{"owner":"adaiversen","name":"demo","first":10}}"#,
        );
        let resp2 = handle_graphql(&mut store, &req2).expect("handled");
        let text2 = String::from_utf8_lossy(&resp2.body).to_string();
        assert!(text2.contains("\"title\":\"hello\""), "body: {text2}");
        assert!(text2.contains("\"state\":\"OPEN\""), "body: {text2}");
        let _ = body_json(&resp2);
    }

    #[test]
    fn missing_token_is_401() {
        let mut store = Store::new();
        let req = Request {
            method: "POST".to_string(),
            path: "/api/graphql".to_string(),
            query: String::new(),
            headers: vec![],
            body: b"{}".to_vec(),
        };
        let resp = handle_graphql(&mut store, &req).expect("handled");
        assert_eq!(resp.status, 401);
    }

    #[test]
    fn batch_returns_array_in_order() {
        let mut store = Store::new();
        let req = post_req(
            r#"[{"query":"{ viewer { login } }"},{"query":"{ viewer { noSuchFieldOnViewer } }"}]"#,
        );
        let resp = handle_graphql(&mut store, &req).expect("handled");
        assert_eq!(resp.status, 200);
        let text = String::from_utf8_lossy(&resp.body).to_string();
        assert!(text.starts_with('['), "body: {text}");
        assert!(text.contains("adaiversen"), "body: {text}");
        assert!(text.contains("errors"), "body: {text}");
    }
}
