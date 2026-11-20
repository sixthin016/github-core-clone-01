// http.rs — HTTP/1.1 transport over std TcpListener, one thread per
// connection, plus the JSON reply helpers every surface shares.
//
// Wire behaviour follows the TS raw server: full request (headers + body)
// is read before the handler runs; Content-Length and chunked bodies are
// parsed; `Expect: 100-continue` is answered as soon as the head is in;
// keep-alive by default on HTTP/1.1; `Connection: close` ends the socket
// after the reply. Pipelining is not needed: requests on one socket are
// answered in order, one at a time.
//
// Every JSON reply is `application/json; charset=utf-8` and carries a
// non-empty `X-GitHub-Request-Id` taken from the store counter. Error
// bodies are always JSON with a string `message`.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::json::Json;
use crate::store::Store;

pub const JSON_TYPE: &str = "application/json; charset=utf-8";
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    /// Raw path, no query string, no fragment, absolute-form stripped.
    pub path: String,
    /// Raw query string, no leading '?'.
    pub query: String,
    /// Header names stored lower-cased; duplicate names joined with ", ".
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Case-insensitive header lookup (names are stored lower-cased).
pub fn header<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    let want = name.to_ascii_lowercase();
    req.headers
        .iter()
        .find(|(n, _)| *n == want)
        .map(|(_, v)| v.as_str())
}

/// Path segments, percent-decoded, without empty leading/trailing parts.
pub fn path_segments(req: &Request) -> Vec<String> {
    req.path
        .split('/')
        .filter(|part| !part.is_empty())
        .map(percent_decode)
        .collect()
}

/// Percent-decode a path segment (`+` stays a plus here).
pub fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Query pairs in order, form-decoded (`+` becomes a space).
pub fn query_pairs(query: &str) -> Vec<(String, String)> {
    if query.is_empty() {
        return Vec::new();
    }
    query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => (form_decode(k), form_decode(v)),
            None => (form_decode(pair), String::new()),
        })
        .collect()
}

fn form_decode(input: &str) -> String {
    percent_decode(&input.replace('+', " "))
}

/// First value of a query parameter, if present.
pub fn query_value(query: &str, name: &str) -> Option<String> {
    query_pairs(query)
        .into_iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v)
}

/// Positive integer query value, or `fallback` when absent or invalid.
pub fn query_int(query: &str, name: &str, fallback: i64) -> i64 {
    match query_value(query, name) {
        None => fallback,
        Some(raw) => {
            let text = raw.trim();
            let (neg, digits) = match text.strip_prefix('-') {
                Some(rest) => (true, rest),
                None => (false, text.strip_prefix('+').unwrap_or(text)),
            };
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return fallback;
            }
            // Leading digits only, like parseInt; the all-digits check above
            // already guarantees the whole token is numeric.
            match digits.parse::<i64>() {
                Ok(v) => {
                    let v = if neg { -v } else { v };
                    if v > 0 {
                        v
                    } else {
                        fallback
                    }
                }
                Err(_) => fallback,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Reply helpers
// ---------------------------------------------------------------------------

pub fn json_response(store: &mut Store, status: u16, value: Json) -> Response {
    json_response_extra(store, status, value, Vec::new())
}

pub fn json_response_extra(
    store: &mut Store,
    status: u16,
    value: Json,
    extra: Vec<(String, String)>,
) -> Response {
    let mut text = value.to_string();
    text.push('\n');
    json_bytes_response(store, status, text.into_bytes(), extra)
}

/// Send already-serialised JSON bytes (hot path: a cached issue body).
pub fn json_bytes_response(
    store: &mut Store,
    status: u16,
    body: Vec<u8>,
    extra: Vec<(String, String)>,
) -> Response {
    let mut headers = vec![
        ("content-type".to_string(), JSON_TYPE.to_string()),
        ("content-length".to_string(), body.len().to_string()),
        (
            "x-github-request-id".to_string(),
            store.next_request_id(),
        ),
    ];
    headers.extend(extra);
    Response {
        status,
        headers,
        body,
    }
}

/// Error body: JSON `{"message": ...}`.
pub fn error_response(store: &mut Store, status: u16, msg: &str) -> Response {
    json_response(
        store,
        status,
        Json::Obj(vec![("message".to_string(), Json::Str(msg.to_string()))]),
    )
}

// ---------------------------------------------------------------------------
// Request parsing
// ---------------------------------------------------------------------------

/// A parsed request head: request line + headers.
#[derive(Debug)]
pub struct ParsedHead {
    pub method: String,
    /// Raw request target (absolute-form already stripped).
    pub target: String,
    pub version: String,
    pub headers: Vec<(String, String)>,
    /// Byte length of the head including the final CRLFCRLF.
    pub head_len: usize,
}

/// Parse the request line + header block from `buf`. Returns None when the
/// head is incomplete (no CRLFCRLF yet).
pub fn parse_head(buf: &[u8]) -> Option<ParsedHead> {
    let end = find_subslice(buf, b"\r\n\r\n")?;
    let line_end = find_subslice(buf, b"\r\n")?;
    let first_space = buf[..line_end].iter().position(|&b| b == b' ')?;
    let second_space = buf[first_space + 1..line_end]
        .iter()
        .position(|&b| b == b' ')
        .map(|p| p + first_space + 1)?;
    let method = String::from_utf8_lossy(&buf[..first_space]).into_owned();
    let raw_target = String::from_utf8_lossy(&buf[first_space + 1..second_space]).into_owned();
    let version = String::from_utf8_lossy(&buf[second_space + 1..line_end]).into_owned();
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut pos = line_end + 2;
    while pos < end {
        // One header line: find its CRLF and its first ':' in a single scan.
        let mut colon: Option<usize> = None;
        let mut eol: Option<usize> = None;
        let mut j = pos;
        while j <= end {
            let c = buf[j];
            if c == b':' && colon.is_none() {
                colon = Some(j);
            } else if c == b'\r' && buf.get(j + 1) == Some(&b'\n') {
                eol = Some(j);
                break;
            }
            j += 1;
        }
        let eol = eol?;
        if let Some(colon) = colon {
            let mut name_start = pos;
            let mut name_end = colon;
            while name_start < name_end
                && (buf[name_start] == b' ' || buf[name_start] == b'\t')
            {
                name_start += 1;
            }
            while name_end > name_start
                && (buf[name_end - 1] == b' ' || buf[name_end - 1] == b'\t')
            {
                name_end -= 1;
            }
            if name_start < name_end {
                let name = String::from_utf8_lossy(&buf[name_start..name_end])
                    .to_ascii_lowercase();
                let value = String::from_utf8_lossy(&buf[colon + 1..eol])
                    .trim()
                    .to_string();
                match headers.iter_mut().find(|(n, _)| *n == name) {
                    Some((_, prev)) => {
                        prev.push_str(", ");
                        prev.push_str(&value);
                    }
                    None => headers.push((name, value)),
                }
            }
        }
        pos = eol + 2;
    }
    let target = strip_absolute_form(&raw_target);
    Some(ParsedHead {
        method,
        target,
        version,
        headers,
        head_len: end + 4,
    })
}

fn strip_absolute_form(target: &str) -> String {
    if target.starts_with("http://") || target.starts_with("https://") {
        let after_scheme = target.find("://").map(|i| i + 3).unwrap_or(0);
        match target[after_scheme..].find('/') {
            Some(rel) => target[after_scheme + rel..].to_string(),
            None => "/".to_string(),
        }
    } else {
        target.to_string()
    }
}

/// Split a request target into (path, raw query). Fragment is dropped.
pub fn split_target(target: &str) -> (String, String) {
    let no_frag = match target.find('#') {
        Some(i) => &target[..i],
        None => target,
    };
    match no_frag.find('?') {
        Some(i) => (no_frag[..i].to_string(), no_frag[i + 1..].to_string()),
        None => (no_frag.to_string(), String::new()),
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

/// Parse a chunked body starting at `buf[0]`. Returns the body plus the
/// bytes consumed, or None when more data is needed.
pub fn parse_chunked(buf: &[u8]) -> Option<(Vec<u8>, usize)> {
    let mut parts: Vec<u8> = Vec::new();
    let mut pos = 0usize;
    loop {
        let line_end = find_subslice(&buf[pos..], b"\r\n").map(|i| i + pos)?;
        let size_text = std::str::from_utf8(&buf[pos..line_end])
            .ok()?
            .split(';')
            .next()
            .unwrap_or("")
            .trim();
        let size = usize::from_str_radix(size_text, 16).ok()?;
        let chunk_start = line_end + 2;
        if size == 0 {
            // Terminal chunk: trailers end at the next empty line.
            let trailer_end = find_subslice(&buf[chunk_start..], b"\r\n").map(|i| i + chunk_start)?;
            return Some((parts, trailer_end + 2));
        }
        if buf.len() < chunk_start + size + 2 {
            return None;
        }
        parts.extend_from_slice(&buf[chunk_start..chunk_start + size]);
        pos = chunk_start + size + 2;
    }
}

enum Framing {
    Length(usize),
    Chunked,
    None,
}

fn framing_of(head: &ParsedHead) -> Option<Framing> {
    let te = head
        .headers
        .iter()
        .find(|(n, _)| n == "transfer-encoding")
        .map(|(_, v)| v.to_ascii_lowercase())
        .unwrap_or_default();
    if te.contains("chunked") {
        return Some(Framing::Chunked);
    }
    if let Some((_, cl)) = head.headers.iter().find(|(n, _)| n == "content-length") {
        let length = parse_content_length(cl)?;
        if length > MAX_BODY_BYTES {
            return None;
        }
        return Some(Framing::Length(length));
    }
    Some(Framing::None)
}

/// Lenient leading-digits parse, like Number.parseInt.
fn parse_content_length(text: &str) -> Option<usize> {
    let text = text.trim();
    let digits: String = text.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse::<usize>().ok()
}

// ---------------------------------------------------------------------------
// Connection handling
// ---------------------------------------------------------------------------

struct Conn {
    stream: TcpStream,
    buf: Vec<u8>,
    pos: usize,
}

impl Conn {
    /// Read more bytes into the buffer. Ok(false) means clean EOF.
    fn fill(&mut self) -> std::io::Result<bool> {
        if self.pos > 0 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        let mut tmp = [0u8; 8192];
        match self.stream.read(&mut tmp) {
            Ok(0) => Ok(false),
            Ok(n) => {
                self.buf.extend_from_slice(&tmp[..n]);
                Ok(true)
            }
            Err(e) => Err(e),
        }
    }

    fn available(&self) -> &[u8] {
        &self.buf[self.pos..]
    }

    fn consume(&mut self, n: usize) {
        self.pos += n;
    }

    /// Read one complete request. Ok(None) on clean EOF before any byte.
    /// Err(()) on malformed framing or I/O error: the socket is closed.
    fn read_request(&mut self) -> Result<Option<(Request, bool)>, ()> {
        // Head.
        let head = loop {
            if self.available().is_empty() && !self.fill().map_err(|_| ())? {
                return Ok(None);
            }
            if self.available().len() > MAX_HEADER_BYTES
                && find_subslice(self.available(), b"\r\n\r\n").is_none()
            {
                return Err(());
            }
            match parse_head(self.available()) {
                Some(h) => break h,
                None => {
                    if !self.fill().map_err(|_| ())? {
                        return Ok(None);
                    }
                }
            }
        };
        let framing = framing_of(&head).ok_or(())?;
        let expect_continue = head
            .headers
            .iter()
            .find(|(n, _)| n == "expect")
            .map(|(_, v)| v.eq_ignore_ascii_case("100-continue"))
            .unwrap_or(false);
        if expect_continue {
            if self
                .stream
                .write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
                .is_err()
            {
                return Err(());
            }
        }
        let (path, query) = split_target(&head.target);
        // Body.
        let body: Vec<u8> = match framing {
            Framing::None => Vec::new(),
            Framing::Length(len) => {
                while self.available().len() < head.head_len + len {
                    if !self.fill().map_err(|_| ())? {
                        return Err(());
                    }
                }
                let start = head.head_len;
                let body = self.available()[start..start + len].to_vec();
                self.consume(head.head_len + len);
                body
            }
            Framing::Chunked => loop {
                let rest_start = head.head_len;
                match parse_chunked(&self.available()[rest_start..]) {
                    Some((body, consumed)) => {
                        self.consume(rest_start + consumed);
                        break body;
                    }
                    None => {
                        if self.available().len() > MAX_BODY_BYTES + MAX_HEADER_BYTES {
                            return Err(());
                        }
                        if !self.fill().map_err(|_| ())? {
                            return Err(());
                        }
                    }
                }
            },
        };
        if matches!(framing, Framing::None) {
            self.consume(head.head_len);
        }
        let connection = head
            .headers
            .iter()
            .find(|(n, _)| n == "connection")
            .map(|(_, v)| v.to_ascii_lowercase())
            .unwrap_or_default();
        let keep_alive = if connection.contains("close") {
            false
        } else if connection.contains("keep-alive") {
            true
        } else {
            head.version == "HTTP/1.1"
        };
        Ok(Some((
            Request {
                method: head.method,
                path,
                query,
                headers: head.headers,
                body,
            },
            keep_alive,
        )))
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Content Too Large",
        422 => "Unprocessable Entity",
        500 => "Internal Server Error",
        _ => "",
    }
}

fn has_header(headers: &[(String, String)], name: &str) -> bool {
    headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(name))
}

/// Serialise one response onto the socket.
fn write_response(stream: &mut TcpStream, resp: &Response, keep_alive: bool) -> std::io::Result<()> {
    let no_body = resp.status == 204 || resp.status == 304;
    let body: &[u8] = if no_body { &[] } else { &resp.body };
    let mut head = format!("HTTP/1.1 {} {}\r\n", resp.status, reason(resp.status));
    for (name, value) in &resp.headers {
        head.push_str(&format!("{}: {}\r\n", name, value));
    }
    if !has_header(&resp.headers, "content-length") {
        head.push_str(&format!("content-length: {}\r\n", body.len()));
    }
    if !has_header(&resp.headers, "connection") {
        head.push_str(&format!(
            "connection: {}\r\n",
            if keep_alive { "keep-alive" } else { "close" }
        ));
    }
    if !has_header(&resp.headers, "date") {
        head.push_str(&format!("date: {}\r\n", date_header()));
    }
    head.push_str("\r\n");
    let mut out = head.into_bytes();
    out.extend_from_slice(body);
    stream.write_all(&out)
}

fn handle_conn(stream: TcpStream, store: Arc<Mutex<Store>>, handler: fn(&mut Store, Request) -> Response) {
    let _ = stream.set_nodelay(true);
    let mut conn = Conn {
        stream,
        buf: Vec::new(),
        pos: 0,
    };
    loop {
        let (req, keep_alive) = match conn.read_request() {
            Ok(Some(pair)) => pair,
            _ => break,
        };
        let resp = {
            let mut guard = store.lock().unwrap_or_else(|e| e.into_inner());
            handler(&mut guard, req)
        };
        if write_response(&mut conn.stream, &resp, keep_alive).is_err() {
            break;
        }
        if !keep_alive {
            break;
        }
    }
}

/// Accept loop: one thread per connection. The store mutex is locked once
/// per request, around the handler call only.
pub fn serve(
    listener: TcpListener,
    store: Arc<Mutex<Store>>,
    handler: fn(&mut Store, Request) -> Response,
) {
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let store = Arc::clone(&store);
                std::thread::spawn(move || handle_conn(s, store, handler));
            }
            Err(_) => continue,
        }
    }
}

// ---------------------------------------------------------------------------
// Date header (cached at most once per second, like the TS server)
// ---------------------------------------------------------------------------

static DATE_CACHE: std::sync::OnceLock<Mutex<(u64, String)>> = std::sync::OnceLock::new();

fn date_header() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let cache = DATE_CACHE.get_or_init(|| Mutex::new((0, String::new())));
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    if now - guard.0 >= 1 || guard.1.is_empty() {
        guard.1 = format_http_date(now);
        guard.0 = now;
    }
    guard.1.clone()
}

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

fn format_http_date(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    // 1970-01-01 was a Thursday; weekday 0 = Sunday.
    let weekday = WEEKDAYS[((days + 4).rem_euclid(7)) as usize];
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        weekday,
        day,
        MONTHS[(month - 1) as usize],
        year,
        tod / 3600,
        (tod / 60) % 60,
        tod % 60
    )
}

/// Days since 1970-01-01 -> (year, month, day), Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_parses() {
        let raw = b"GET /api/v3/user?x=1 HTTP/1.1\r\nHost: example.invalid\r\nAuthorization: Bearer abc\r\nX-Custom:  a  b \r\n\r\n";
        let head = parse_head(raw).unwrap();
        assert_eq!(head.method, "GET");
        assert_eq!(head.target, "/api/v3/user?x=1");
        assert_eq!(head.version, "HTTP/1.1");
        assert_eq!(head.head_len, raw.len());
        assert_eq!(
            head.headers
                .iter()
                .find(|(n, _)| n == "authorization")
                .map(|(_, v)| v.as_str()),
            Some("Bearer abc")
        );
        assert_eq!(
            head.headers
                .iter()
                .find(|(n, _)| n == "x-custom")
                .map(|(_, v)| v.as_str()),
            Some("a  b")
        );
    }

    #[test]
    fn head_duplicate_headers_join() {
        let raw = b"GET / HTTP/1.1\r\nX-A: 1\r\nx-a: 2\r\n\r\n";
        let head = parse_head(raw).unwrap();
        assert_eq!(head.headers.len(), 1);
        assert_eq!(head.headers[0], ("x-a".to_string(), "1, 2".to_string()));
    }

    #[test]
    fn head_incomplete_is_none() {
        assert!(parse_head(b"GET / HTTP/1.1\r\nHost: x\r\n").is_none());
        assert!(parse_head(b"").is_none());
    }

    #[test]
    fn absolute_form_stripped() {
        let raw = b"GET http://example.invalid/api/v3/user HTTP/1.1\r\nHost: example.invalid\r\n\r\n";
        let head = parse_head(raw).unwrap();
        assert_eq!(head.target, "/api/v3/user");
    }

    #[test]
    fn chunked_parses() {
        let raw = b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        let (body, consumed) = parse_chunked(raw).unwrap();
        assert_eq!(body, b"Wikipedia");
        assert_eq!(consumed, raw.len());
        // Chunk extensions and trailers: like the TS parser, consumption
        // stops at the end of the first trailer line (the final empty
        // line's CRLF is left for the next read).
        let raw2 = b"3;ext=1\r\nabc\r\n0\r\nTrailer: x\r\n\r\n";
        let (body2, consumed2) = parse_chunked(raw2).unwrap();
        assert_eq!(body2, b"abc");
        assert_eq!(consumed2, raw2.len() - 2);
        // Incomplete body needs more data.
        assert!(parse_chunked(b"5\r\nab").is_none());
    }

    #[test]
    fn target_split_and_segments() {
        let (path, query) = split_target("/api/v3/repos/a/b/issues?page=2&per_page=5#frag");
        assert_eq!(path, "/api/v3/repos/a/b/issues");
        assert_eq!(query, "page=2&per_page=5");
        let req = Request {
            method: "GET".into(),
            path: "/api/v3/repos/adaiversen/my%20repo/issues".into(),
            query: "state=open".into(),
            headers: vec![],
            body: vec![],
        };
        assert_eq!(
            path_segments(&req),
            vec!["api", "v3", "repos", "adaiversen", "my repo", "issues"]
        );
        assert_eq!(query_value(&req.query, "state"), Some("open".to_string()));
        assert_eq!(query_value(&req.query, "page"), None);
    }

    #[test]
    fn query_int_rules() {
        assert_eq!(query_int("per_page=5", "per_page", 30), 5);
        assert_eq!(query_int("per_page=0", "per_page", 30), 30);
        assert_eq!(query_int("per_page=-3", "per_page", 30), 30);
        assert_eq!(query_int("per_page=abc", "per_page", 30), 30);
        assert_eq!(query_int("", "per_page", 30), 30);
        assert_eq!(query_int("per_page=250", "per_page", 30), 250);
    }

    #[test]
    fn form_decode_plus() {
        assert_eq!(query_value("q=hello+world&x=a%26b", "q"), Some("hello world".into()));
        assert_eq!(query_value("q=hello+world&x=a%26b", "x"), Some("a&b".into()));
    }

    #[test]
    fn http_date_format() {
        // 2025-01-01T00:00:00Z = Wednesday.
        assert_eq!(format_http_date(1_735_689_600), "Wed, 01 Jan 2025 00:00:00 GMT");
        // 2026-11-20T10:00:00Z = Friday.
        assert_eq!(format_http_date(1_795_168_800), "Fri, 20 Nov 2026 10:00:00 GMT");
    }

    #[test]
    fn serve_live_smoke() {
        use std::net::TcpListener;
        use std::sync::Arc;

        fn echo_handler(store: &mut Store, req: Request) -> Response {
            let body_len = req.body.len() as i64;
            json_response(
                store,
                200,
                Json::Obj(vec![
                    ("method".to_string(), Json::Str(req.method)),
                    ("path".to_string(), Json::Str(req.path)),
                    ("body_len".to_string(), Json::Num(body_len as f64)),
                ]),
            )
        }

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let store = Arc::new(Mutex::new(Store::new()));
        std::thread::spawn(move || serve(listener, store, echo_handler));
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();

        // Read one response head + body (by content-length). `buf` persists
        // across calls: one read() may already carry the next response
        // (e.g. 100-continue followed at once by the 200), so bytes past
        // the end of this response must be kept, not dropped.
        fn read_response(stream: &mut std::net::TcpStream, buf: &mut Vec<u8>) -> (String, Vec<u8>) {
            let mut tmp = [0u8; 4096];
            let head_end;
            loop {
                if let Some(i) = find_subslice(buf, b"\r\n\r\n") {
                    head_end = i + 4;
                    break;
                }
                let n = stream.read(&mut tmp).unwrap();
                assert!(n > 0, "socket closed");
                buf.extend_from_slice(&tmp[..n]);
            }
            let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
            let len: usize = head
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                .and_then(|l| l.split(':').nth(1))
                .map(|v| v.trim().parse().unwrap_or(0))
                .unwrap_or(0);
            while buf.len() < head_end + len {
                let n = stream.read(&mut tmp).unwrap();
                assert!(n > 0, "socket closed");
                buf.extend_from_slice(&tmp[..n]);
            }
            let body = buf[head_end..head_end + len].to_vec();
            buf.drain(..head_end + len);
            (head, body)
        }

        let mut rbuf: Vec<u8> = Vec::new();

        // Request 1: plain GET, keep-alive.
        stream
            .write_all(b"GET /api/v3/user HTTP/1.1\r\nHost: x\r\n\r\n")
            .unwrap();
        let (head, body) = read_response(&mut stream, &mut rbuf);
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{}", head);
        assert!(head.to_ascii_lowercase().contains("connection: keep-alive"));
        assert!(head.to_ascii_lowercase().contains("x-github-request-id: req-1"));
        assert!(head.to_ascii_lowercase().contains("date: "));
        assert_eq!(String::from_utf8_lossy(&body), "{\"method\":\"GET\",\"path\":\"/api/v3/user\",\"body_len\":0}\n");

        // Request 2 on the same socket: chunked POST with Expect 100-continue.
        stream
            .write_all(
                b"POST /api/v3/user/repos HTTP/1.1\r\nHost: x\r\nExpect: 100-continue\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
            )
            .unwrap();
        let (head100, _) = read_response(&mut stream, &mut rbuf);
        assert!(
            head100.starts_with("HTTP/1.1 100 Continue\r\n"),
            "expected 100-continue first, got {}",
            head100
        );
        let (head2, body2) = read_response(&mut stream, &mut rbuf);
        assert!(head2.starts_with("HTTP/1.1 200 OK\r\n"), "{}", head2);
        assert_eq!(
            String::from_utf8_lossy(&body2),
            "{\"method\":\"POST\",\"path\":\"/api/v3/user/repos\",\"body_len\":5}\n"
        );

        // Request 3: Connection: close ends the socket after the reply.
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .unwrap();
        let (head3, _) = read_response(&mut stream, &mut rbuf);
        assert!(head3.to_ascii_lowercase().contains("connection: close"));
        let mut tmp = [0u8; 16];
        assert_eq!(stream.read(&mut tmp).unwrap(), 0, "socket should be closed");
    }

    #[test]
    fn framing_detection() {
        let mut head = parse_head(b"POST /x HTTP/1.1\r\nContent-Length: 3\r\n\r\n").unwrap();
        assert!(matches!(framing_of(&head), Some(Framing::Length(3))));
        head = parse_head(b"POST /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap();
        assert!(matches!(framing_of(&head), Some(Framing::Chunked)));
        head = parse_head(b"GET /x HTTP/1.1\r\n\r\n").unwrap();
        assert!(matches!(framing_of(&head), Some(Framing::None)));
        head = parse_head(b"POST /x HTTP/1.1\r\nContent-Length: nope\r\n\r\n").unwrap();
        assert!(framing_of(&head).is_none());
    }
}
