// app.rs — routing forge dan lifecycle proses (std only).
//
//   GET  /healthz         probe boot env: 200 {"ok":true}
//   /{owner}/{repo}.git/* git smart HTTP
//   POST /api/graphql     GraphQL v4
//   POST /mcp            Model Context Protocol
//   /api/v3/*            GitHub REST API v3
//
// Selain itu JSON 404. run() mengikat port ephemeral, mendaftarkannya ke
// env lewat POST /register, lalu polling GET /done sampai env berkata
// selesai dan keluar 0.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::githttp::handle_git;
use crate::graphql::handle_graphql;
use crate::http::{error_response, json_response, path_segments, Request, Response};
use crate::json::Json;
use crate::mcp::handle_mcp;
use crate::rest::handle_rest;
use crate::store::Store;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const RETRY_INITIAL_MS: u64 = 250;
const RETRY_MAX_MS: u64 = 2_000;

/// Satu request masuk -> satu response keluar.
pub fn route(store: &mut Store, req: Request) -> Response {
    let segments = path_segments(&req);
    let path = req.path.clone();
    if req.method == "GET" && path == "/healthz" {
        return json_response(
            store,
            200,
            Json::Obj(vec![("ok".to_string(), Json::Bool(true))]),
        );
    }
    if segments.len() >= 2 && segments[1].ends_with(".git") {
        if let Some(resp) = handle_git(store, &req, &segments) {
            return resp;
        }
    }
    if path == "/mcp" {
        if let Some(resp) = handle_mcp(store, &req) {
            return resp;
        }
    }
    if path == "/api/graphql" {
        if let Some(resp) = handle_graphql(store, &req) {
            return resp;
        }
    }
    if segments.len() >= 2 && segments[0] == "api" && segments[1] == "v3" {
        if let Some(resp) = handle_rest(store, &req) {
            return resp;
        }
    }
    error_response(store, 404, "Not Found")
}

// -------------------------------------------------------------------------------------------
// Klien HTTP minimal untuk percakapan dengan env (register / done)
// -------------------------------------------------------------------------------------------

fn retry_delay_ms(attempt: usize) -> u64 {
    let shift = attempt.min(8) as u32;
    (RETRY_INITIAL_MS.saturating_mul(1u64 << shift)).min(RETRY_MAX_MS)
}

/// Pecah URL env menjadi (host, port, prefix path tanpa trailing slash).
fn parse_env_url(url: &str) -> Option<(String, u16, String)> {
    let after_scheme = match url.find("://") {
        Some(i) => &url[i + 3..],
        None => url,
    };
    let (authority, prefix) = match after_scheme.find('/') {
        Some(i) => (&after_scheme[..i], after_scheme[i..].trim_end_matches('/').to_string()),
        None => (after_scheme, String::new()),
    };
    if authority.is_empty() {
        return None;
    }
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let end = rest.find(']')?;
        let host = &rest[..end];
        let tail = &rest[end + 1..];
        let port = match tail.strip_prefix(':') {
            Some(p) => p.parse::<u16>().ok()?,
            None => 80,
        };
        (host.to_string(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) if p.parse::<u16>().is_ok() => (h.to_string(), p.parse::<u16>().ok()?),
            _ => (authority.to_string(), 80),
        }
    };
    if host.is_empty() {
        return None;
    }
    Some((host, port, prefix))
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Baca dari stream sampai `buf` berisi setidaknya `need` byte.
fn fill_to(
    stream: &mut TcpStream,
    buf: &mut Vec<u8>,
    need: usize,
) -> Result<(), String> {
    let mut tmp = [0u8; 4096];
    while buf.len() < need {
        match stream.read(&mut tmp) {
            Ok(0) => return Err("connection closed".to_string()),
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(())
}

/// Satu request HTTP/1.1 ke env; jawabannya (status, body).
fn http_request(
    env_url: &str,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> Result<(u16, Vec<u8>), String> {
    let (host, port, prefix) = parse_env_url(env_url)
        .ok_or_else(|| format!("bad MAKEFUN_ENV_URL: {env_url}"))?;
    let mut last_err = String::new();
    let mut stream: Option<TcpStream> = None;
    for addr in format!("{host}:{port}")
        .to_socket_addrs()
        .map_err(|e| e.to_string())?
    {
        match TcpStream::connect_timeout(&addr, REQUEST_TIMEOUT) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => last_err = e.to_string(),
        }
    }
    let mut stream = stream.ok_or(last_err)?;
    stream
        .set_read_timeout(Some(REQUEST_TIMEOUT))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(REQUEST_TIMEOUT))
        .map_err(|e| e.to_string())?;
    let full_path = format!("{prefix}{path}");
    let mut req = format!(
        "{method} {full_path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n"
    );
    if let Some(b) = body {
        req.push_str("Content-Type: application/json\r\n");
        req.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    req.push_str("\r\n");
    if let Some(b) = body {
        req.push_str(b);
    }
    stream.write_all(req.as_bytes()).map_err(|e| e.to_string())?;

    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end = loop {
        match stream.read(&mut tmp) {
            Ok(0) => break None,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                    break Some(pos + 4);
                }
                if buf.len() > 65536 {
                    return Err("response headers too large".to_string());
                }
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    let header_end = match header_end {
        Some(p) => p,
        None => return Err("connection closed before headers".to_string()),
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| format!("bad status line: {status_line}"))?;
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            let v = v.trim();
            if k == "content-length" {
                content_length = v.parse::<usize>().ok();
            } else if k == "transfer-encoding" && v.eq_ignore_ascii_case("chunked") {
                chunked = true;
            }
        }
    }
    if chunked {
        let mut body_bytes: Vec<u8> = Vec::new();
        let mut base = header_end;
        loop {
            // Baca satu baris ukuran chunk.
            let line_end = loop {
                if let Some(p) = find_subslice(&buf[base..], b"\r\n") {
                    break p;
                }
                let need = buf.len() + 1;
                fill_to(&mut stream, &mut buf, need)?;
            };
            let size_line =
                String::from_utf8_lossy(&buf[base..base + line_end]).to_string();
            let size_str = size_line.split(';').next().unwrap_or("").trim();
            let size = usize::from_str_radix(size_str, 16)
                .map_err(|_| format!("bad chunk size: {size_str}"))?;
            // Buang baris ukuran + data chunk + CRLF penutup.
            let need = base + line_end + 2 + size + 2;
            fill_to(&mut stream, &mut buf, need)?;
            if size == 0 {
                break;
            }
            body_bytes.extend_from_slice(&buf[base + line_end + 2..base + line_end + 2 + size]);
            buf.drain(..need);
            base = 0;
        }
        return Ok((status, body_bytes));
    }
    if let Some(len) = content_length {
        fill_to(&mut stream, &mut buf, header_end + len)?;
        return Ok((status, buf[header_end..header_end + len].to_vec()));
    }
    // Tanpa panjang: baca sampai tutup.
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok((status, buf[header_end..].to_vec()))
}

/// POST port ke env sampai diterima. 200/409 selesai; 400/405 fatal.
fn register(env_url: &str, port: u16) -> Result<(), String> {
    let mut attempt = 0usize;
    loop {
        let body = format!("{{\"port\":{port}}}");
        match http_request(env_url, "POST", "/register", Some(&body)) {
            Ok((200, _)) | Ok((409, _)) => return Ok(()),
            Ok((status @ (400 | 405), resp_body)) => {
                return Err(format!(
                    "register answered HTTP {status}: {}",
                    String::from_utf8_lossy(&resp_body)
                ));
            }
            Ok(_) => {}
            Err(_) => {}
        }
        std::thread::sleep(Duration::from_millis(retry_delay_ms(attempt)));
        attempt += 1;
    }
}

/// Poll GET /done sampai env menjawab {"done":true}.
fn wait_for_done(env_url: &str) {
    let mut attempt = 0usize;
    loop {
        if let Ok((status, body)) = http_request(env_url, "GET", "/done", None) {
            if (200..300).contains(&status) {
                if let Ok(Json::Obj(fields)) = Json::parse(&String::from_utf8_lossy(&body)) {
                    if fields
                        .iter()
                        .any(|(k, v)| k == "done" && matches!(v, Json::Bool(true)))
                    {
                        return;
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(retry_delay_ms(attempt)));
        attempt += 1;
    }
}

/// Lifecycle proses: ikat port, daftarkan, layani, tunggu done, keluar 0.
pub fn run() {
    let env_url = std::env::var("MAKEFUN_ENV_URL").unwrap_or_default();
    let env_url = env_url.trim_end_matches('/').to_string();
    if env_url.is_empty() {
        eprintln!("[entry] MAKEFUN_ENV_URL is not set: there is no env to register with");
        std::process::exit(1);
    }
    let listener = match TcpListener::bind("0.0.0.0:0") {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[entry] listen failed: {e}");
            std::process::exit(1);
        }
    };
    let port = match listener.local_addr() {
        Ok(addr) => addr.port(),
        Err(e) => {
            eprintln!("[entry] {e}");
            std::process::exit(1);
        }
    };
    println!("[entry] listening on port {port}");
    let store = std::sync::Arc::new(std::sync::Mutex::new(Store::new()));
    std::thread::spawn(move || {
        crate::http::serve(listener, store, route);
    });
    if let Err(e) = register(&env_url, port) {
        eprintln!("[entry] {e}");
        std::process::exit(1);
    }
    println!("[entry] registered; waiting for the env to say done");
    wait_for_done(&env_url);
    println!("[entry] done");
}

// -------------------------------------------------------------------------------------------
// Test
// -------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_env_url_plain() {
        assert_eq!(
            parse_env_url("http://store:8080"),
            Some(("store".to_string(), 8080, String::new()))
        );
        assert_eq!(
            parse_env_url("http://127.0.0.1:9000/base/"),
            Some(("127.0.0.1".to_string(), 9000, "/base".to_string()))
        );
        assert_eq!(
            parse_env_url("http://localhost"),
            Some(("localhost".to_string(), 80, String::new()))
        );
        assert_eq!(parse_env_url(""), None);
    }

    #[test]
    fn retry_delay_caps_at_2s() {
        assert_eq!(retry_delay_ms(0), 250);
        assert_eq!(retry_delay_ms(1), 500);
        assert_eq!(retry_delay_ms(2), 1000);
        assert_eq!(retry_delay_ms(3), 2000);
        assert_eq!(retry_delay_ms(20), 2000);
    }

    #[test]
    fn route_healthz_and_404() {
        let mut store = Store::new();
        let req = Request {
            method: "GET".to_string(),
            path: "/healthz".to_string(),
            query: String::new(),
            headers: vec![],
            body: vec![],
        };
        let resp = route(&mut store, req);
        assert_eq!(resp.status, 200);
        assert!(String::from_utf8_lossy(&resp.body).contains("\"ok\":true"));
        let req2 = Request {
            method: "GET".to_string(),
            path: "/nope".to_string(),
            query: String::new(),
            headers: vec![],
            body: vec![],
        };
        let resp2 = route(&mut store, req2);
        assert_eq!(resp2.status, 404);
        assert!(String::from_utf8_lossy(&resp2.body).contains("Not Found"));
    }

    #[test]
    fn route_dispatches_to_graphql_and_mcp() {
        let mut store = Store::new();
        let req = Request {
            method: "POST".to_string(),
            path: "/api/graphql".to_string(),
            query: String::new(),
            headers: vec![("authorization".to_string(), "Bearer t".to_string())],
            body: br#"{"query":"{ viewer { login } }"}"#.to_vec(),
        };
        let resp = route(&mut store, req);
        assert_eq!(resp.status, 200);
        assert!(String::from_utf8_lossy(&resp.body).contains("adaiversen"));
        let req2 = Request {
            method: "POST".to_string(),
            path: "/mcp".to_string(),
            query: String::new(),
            headers: vec![("authorization".to_string(), "Bearer t".to_string())],
            body: br#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#.to_vec(),
        };
        let resp2 = route(&mut store, req2);
        assert_eq!(resp2.status, 200);
        assert!(String::from_utf8_lossy(&resp2.body).contains("create_repo"));
    }
}
