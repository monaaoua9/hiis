mod common;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use hiis::json::Json;
use hiis::server::{Server, ServerConfig};
use hiis::Engine;

struct Reply {
    status: u16,
    json: Json,
}

fn start(cfg: ServerConfig) -> (TmpDir, Arc<Engine>, Server) {
    let dir = TmpDir::new();
    let engine = Arc::new(Engine::open(&dir.0, small_cfg()).unwrap());
    let cfg = ServerConfig { addr: "127.0.0.1:0".into(), ..cfg };
    let server = Server::start(Arc::clone(&engine), cfg).unwrap();
    (dir, engine, server)
}

fn read_reply(s: &mut TcpStream) -> Reply {
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        s.read_exact(&mut b).unwrap();
        head.push(b[0]);
    }
    let head = String::from_utf8(head).unwrap();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    let len: usize = head
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap())
        })
        .unwrap();
    let mut body = vec![0u8; len];
    s.read_exact(&mut body).unwrap();
    Reply { status, json: Json::parse(std::str::from_utf8(&body).unwrap()).unwrap() }
}

fn send(s: &mut TcpStream, method: &str, target: &str, token: Option<&str>, body: &str) -> Reply {
    let auth = token.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
    write!(s, "{method} {target} HTTP/1.1\r\nHost: x\r\n{auth}Content-Length: {}\r\n\r\n{body}", body.len())
        .unwrap();
    read_reply(s)
}

fn conn(server: &Server) -> TcpStream {
    let s = TcpStream::connect(server.local_addr()).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s
}

fn call(server: &Server, method: &str, target: &str, body: &str) -> Reply {
    send(&mut conn(server), method, target, None, body)
}

fn data(r: &Reply) -> &Json {
    r.json.get("data").unwrap_or_else(|| panic!("no data in {}", r.json))
}

fn num(j: &Json) -> f64 {
    match j {
        Json::Num(n) => *n,
        other => panic!("not a number: {other}"),
    }
}

fn error_code(r: &Reply) -> &str {
    r.json.get("error").unwrap().get("code").unwrap().as_str().unwrap()
}

#[test]
fn add_search_get_delete() {
    let (_d, _e, srv) = start(ServerConfig::default());

    let body = r#"{"documents": ["the quick brown fox", "a lazy dog", "fox and dog"]}"#;
    let r = call(&srv, "POST", "/api/v1/documents?commit=true", body);
    assert_eq!(r.status, 201);
    assert_eq!(data(&r).get("ids").unwrap().to_string(), "[0,1,2]");

    let r = call(&srv, "GET", "/api/v1/search?q=fox&highlight=1", "");
    assert_eq!(r.status, 200);
    let hits = data(&r).as_array().unwrap();
    assert_eq!(hits.len(), 2);
    let snippet = hits[0].get("snippet").unwrap().as_str().unwrap();
    assert!(snippet.contains("<em>fox</em>"), "{snippet}");
    assert!(r.json.get("meta").unwrap().get("took_ms").is_some());

    let r = call(&srv, "GET", "/api/v1/documents/1", "");
    assert_eq!(data(&r).get("text").unwrap().as_str(), Some("a lazy dog"));

    let r = call(&srv, "DELETE", "/api/v1/documents/1", "");
    assert_eq!(r.status, 200);
    assert_eq!(call(&srv, "GET", "/api/v1/documents/1", "").status, 404);
    assert_eq!(call(&srv, "DELETE", "/api/v1/documents/1", "").status, 404);
    srv.shutdown().unwrap();
}

#[test]
fn single_text_form_and_unicode() {
    let (_d, _e, srv) = start(ServerConfig::default());
    let body = r#"{"text": "المعلمون في المدرسة \"quoted\" é"}"#;
    let r = call(&srv, "POST", "/api/v1/documents?commit=true", body);
    assert_eq!(r.status, 201);
    assert_eq!(num(data(&r).get("id").unwrap()), 0.0);

    let r = call(&srv, "GET", "/api/v1/search?q=%D9%85%D8%B9%D9%84%D9%85", "");
    assert_eq!(data(&r).as_array().unwrap().len(), 1);
    let r = call(&srv, "GET", "/api/v1/documents/0", "");
    assert_eq!(data(&r).get("text").unwrap().as_str(), Some("المعلمون في المدرسة \"quoted\" é"));
}

#[test]
fn pagination_walks_every_hit_once() {
    let (_d, _e, srv) = start(ServerConfig::default());
    let docs: Vec<String> = (0..25).map(|i| format!("\"shared word number{i}\"")).collect();
    let body = format!("{{\"documents\": [{}]}}", docs.join(","));
    call(&srv, "POST", "/api/v1/documents?commit=true", &body);

    let mut seen = Vec::new();
    let mut cursor = String::new();
    for _ in 0..10 {
        let r = call(&srv, "GET", &format!("/api/v1/search?q=shared&limit=10&cursor={cursor}"), "");
        for h in data(&r).as_array().unwrap() {
            seen.push(num(h.get("id").unwrap()) as u64);
        }
        match r.json.get("meta").unwrap().get("next_cursor").unwrap() {
            Json::Str(c) => cursor = c.clone(),
            _ => break,
        }
    }
    seen.sort_unstable();
    assert_eq!(seen, (0..25).collect::<Vec<u64>>());

    let r = call(&srv, "GET", "/api/v1/search?q=shared&limit=3&count=1", "");
    assert_eq!(data(&r).as_array().unwrap().len(), 3);
    assert_eq!(num(r.json.get("meta").unwrap().get("total").unwrap()), 25.0);
}

#[test]
fn similar_explain_suggest_stats() {
    let (_d, _e, srv) = start(ServerConfig::default());
    let body = r#"{"documents": ["rust ownership and borrowing", "rust borrowing rules", "sourdough bread flour", "compression of posting lists"]}"#;
    call(&srv, "POST", "/api/v1/documents?commit=true", body);

    let r = call(&srv, "GET", "/api/v1/documents/0/similar?limit=1", "");
    assert_eq!(num(data(&r).as_array().unwrap()[0].get("id").unwrap()), 1.0);

    let r = call(&srv, "GET", "/api/v1/documents/0/explain?q=rust+bread", "");
    assert_eq!(data(&r).get("matched"), Some(&Json::Bool(true)));
    assert!(data(&r).get("explanation").unwrap().as_str().unwrap().contains("rust"));

    let r = call(&srv, "GET", "/api/v1/suggest?prefix=compre", "");
    assert_eq!(data(&r).as_array().unwrap()[0].get("text").unwrap().as_str(), Some("compression"));

    let r = call(&srv, "GET", "/api/v1/stats", "");
    assert_eq!(num(data(&r).get("live_docs").unwrap()), 4.0);

    let r = call(&srv, "GET", "/api/v1/search?q=ownrship", "");
    assert!(data(&r).as_array().unwrap().is_empty());
    assert_eq!(r.json.get("meta").unwrap().get("did_you_mean").unwrap().as_str(), Some("ownership"));
}

#[test]
fn errors_have_stable_codes() {
    let (_d, _e, srv) = start(ServerConfig { max_body: 200, ..Default::default() });
    let r = call(&srv, "POST", "/api/v1/documents", "not json");
    assert_eq!((r.status, error_code(&r)), (400, "INVALID_REQUEST"));
    assert_eq!(call(&srv, "POST", "/api/v1/documents", r#"{"nothing": 1}"#).status, 400);
    assert_eq!(call(&srv, "POST", "/api/v1/documents", r#"{"documents": [1]}"#).status, 400);
    let r = call(&srv, "GET", "/api/v1/search", "");
    assert_eq!((r.status, error_code(&r)), (400, "INVALID_REQUEST"));
    assert_eq!(call(&srv, "GET", "/api/v1/search?q=x&limit=0", "").status, 400);
    assert_eq!(call(&srv, "GET", "/api/v1/search?q=x&cursor=garbage", "").status, 400);
    assert_eq!(call(&srv, "GET", "/api/v1/documents/abc", "").status, 400);
    let r = call(&srv, "GET", "/api/v1/nothing", "");
    assert_eq!((r.status, error_code(&r)), (404, "NOT_FOUND"));
    let r = call(&srv, "PUT", "/api/v1/search?q=x", "");
    assert_eq!((r.status, error_code(&r)), (405, "METHOD_NOT_ALLOWED"));
    let r = call(&srv, "POST", "/api/v1/documents", &format!("{{\"text\": \"{}\"}}", "x".repeat(500)));
    assert_eq!((r.status, error_code(&r)), (413, "BODY_TOO_LARGE"));
}

#[test]
fn bearer_token_is_enforced() {
    let (_d, _e, srv) = start(ServerConfig { token: Some("s3cret".into()), ..Default::default() });
    for (tok, want) in [(None, 401), (Some("s3cre"), 401), (Some("s3cretX"), 401), (Some("s3cret"), 200)] {
        let mut c = conn(&srv);
        assert_eq!(send(&mut c, "GET", "/api/v1/stats", tok, "").status, want, "{tok:?}");
    }
    // health check is for load balancers, no secrets in it
    assert_eq!(call(&srv, "GET", "/health", "").status, 200);
}

#[test]
fn keep_alive_serves_many_requests_on_one_connection() {
    let (_d, _e, srv) = start(ServerConfig::default());
    let mut c = conn(&srv);
    for i in 0..20 {
        let r = send(&mut c, "POST", "/api/v1/documents", None, &format!("{{\"text\": \"doc {i}\"}}"));
        assert_eq!(r.status, 201);
    }
    assert_eq!(send(&mut c, "GET", "/health", None, "").status, 200);

    // chunked bodies are refused and the connection closed; the next one is fine
    let mut raw = conn(&srv);
    raw.write_all(b"GET / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap();
    assert_eq!(read_reply(&mut raw).status, 501);
    assert_eq!(call(&srv, "GET", "/health", "").status, 200);
}

#[test]
fn background_commit_makes_documents_searchable() {
    let cfg = ServerConfig { commit_interval: Some(Duration::from_millis(50)), ..Default::default() };
    let (_d, _e, srv) = start(cfg);
    call(&srv, "POST", "/api/v1/documents", r#"{"text": "eventually visible"}"#);

    let mut found = false;
    for _ in 0..100 {
        let r = call(&srv, "GET", "/api/v1/search?q=eventually", "");
        if !data(&r).as_array().unwrap().is_empty() {
            found = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(found, "never became searchable");
}

#[test]
fn concurrent_clients() {
    let (_d, e, srv) = start(ServerConfig::default());
    std::thread::scope(|sc| {
        for t in 0..6 {
            let srv = &srv;
            sc.spawn(move || {
                let mut c = conn(srv);
                for i in 0..15 {
                    let body = format!("{{\"text\": \"worker{t} item{i} common\"}}");
                    assert_eq!(
                        send(&mut c, "POST", "/api/v1/documents?commit=true", None, &body).status,
                        201
                    );
                    assert_eq!(send(&mut c, "GET", "/api/v1/search?q=common", None, "").status, 200);
                }
            });
        }
    });
    srv.shutdown().unwrap();
    assert_eq!(e.stats().live_docs, 90);
}

#[test]
fn malformed_requests_dont_hurt() {
    let (_d, _e, srv) = start(ServerConfig::default());
    let junk: [&[u8]; 4] =
        [b"\r\n\r\n", b"GARBAGE\r\n\r\n", b"GET / HTTP/9.9\r\n\r\n", b"GET /x HTTP/1.1\r\nbadheader\r\n\r\n"];
    for j in junk {
        let mut c = conn(&srv);
        c.write_all(j).unwrap();
        let mut sink = Vec::new();
        let _ = c.read_to_end(&mut sink);
    }
    // oversized header block
    let mut c = conn(&srv);
    let big = format!("GET / HTTP/1.1\r\nX: {}\r\n\r\n", "a".repeat(40_000));
    let _ = c.write_all(big.as_bytes());
    // the server may reset the connection before we read the 431 (unread request data), either is fine
    let mut buf = Vec::new();
    let _ = c.read_to_end(&mut buf);
    assert!(buf.is_empty() || String::from_utf8_lossy(&buf).contains("431"));
    assert_eq!(call(&srv, "GET", "/health", "").status, 200);
}

#[test]
fn fielded_documents_over_http() {
    let (_d, _e, srv) = start(ServerConfig::default());
    let body = r#"{"fields": {"title": "Rust in Action", "body": "ownership and borrowing"}}"#;
    let r = call(&srv, "POST", "/api/v1/documents?commit=true", body);
    assert_eq!(r.status, 201);
    call(
        &srv,
        "POST",
        "/api/v1/documents?commit=true",
        r#"{"fields": {"title": "Baking", "body": "rust on the oven door"}}"#,
    );

    let r = call(&srv, "GET", "/api/v1/search?q=title%3Arust", "");
    let hits = data(&r).as_array().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(num(hits[0].get("id").unwrap()), 0.0);

    let r = call(&srv, "GET", "/api/v1/documents/0", "");
    assert_eq!(data(&r).get("fields").unwrap().get("title").unwrap().as_str(), Some("Rust in Action"));
    assert_eq!(data(&r).get("text").unwrap().as_str(), Some("Rust in Action\nownership and borrowing"));

    assert_eq!(call(&srv, "POST", "/api/v1/documents", r#"{"fields": {"Bad Name": "x"}}"#).status, 400);
    assert_eq!(call(&srv, "POST", "/api/v1/documents", r#"{"fields": {"title": 5}}"#).status, 400);
    assert_eq!(call(&srv, "POST", "/api/v1/documents", r#"{"fields": []}"#).status, 400);
}

#[test]
fn idle_keep_alive_connections_dont_starve_others() {
    let cfg =
        ServerConfig { max_connections: 64, idle_timeout: Duration::from_secs(30), ..Default::default() };
    let (_d, _e, srv) = start(cfg);
    // more idle keep-alive clients than workers, each having made one request
    let mut idle: Vec<TcpStream> = (0..6).map(|_| conn(&srv)).collect();
    for c in &mut idle {
        assert_eq!(send(c, "GET", "/health", None, "").status, 200);
    }
    let t = std::time::Instant::now();
    let mut fresh = conn(&srv);
    fresh.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    assert_eq!(send(&mut fresh, "GET", "/health", None, "").status, 200);
    assert!(t.elapsed() < Duration::from_secs(2), "took {:?}", t.elapsed());
}

#[test]
fn large_document_and_many_clients() {
    let (_d, e, srv) = start(ServerConfig { max_connections: 64, ..Default::default() });
    let big = "word ".repeat(400_000);
    let r = call(&srv, "POST", "/api/v1/documents?commit=true", &format!("{{\"text\": \"{big}\"}}"));
    assert_eq!(r.status, 201);
    std::thread::scope(|sc| {
        for _ in 0..16 {
            sc.spawn(|| {
                for _ in 0..10 {
                    assert_eq!(call(&srv, "GET", "/api/v1/search?q=word", "").status, 200);
                }
            });
        }
    });
    assert_eq!(e.stats().live_docs, 1);
}

#[test]
fn connections_over_the_cap_get_a_503() {
    let (_d, _e, srv) = start(ServerConfig { max_connections: 3, ..Default::default() });
    let mut held: Vec<TcpStream> = (0..3).map(|_| conn(&srv)).collect();
    for c in &mut held {
        assert_eq!(send(c, "GET", "/health", None, "").status, 200);
    }
    let mut extra = conn(&srv);
    let r = read_reply(&mut extra);
    assert_eq!((r.status, error_code(&r)), (503, "OVERLOADED"));
    // a slot frees up when a client leaves
    held.pop();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(call(&srv, "GET", "/health", "").status, 200);
}
