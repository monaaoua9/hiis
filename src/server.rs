//! A small HTTP/1.1 + JSON front end, std only.
//!
//! ```text
//! POST   /api/v1/documents               {"text": "..."} or {"documents": ["..", ".."]}
//! GET    /api/v1/documents/{id}
//! DELETE /api/v1/documents/{id}
//! GET    /api/v1/documents/{id}/similar?limit=5
//! GET    /api/v1/documents/{id}/explain?q=...
//! GET    /api/v1/search?q=...&limit=10&cursor=...&highlight=1&count=1
//! GET    /api/v1/suggest?prefix=...&limit=10
//! POST   /api/v1/commit
//! GET    /api/v1/stats
//! GET    /health
//! ```
//!
//! Success is `{"data": ..., "meta": ...}`, failure `{"error": {"code", "message"}}` with
//! a stable code. Connections are keep-alive and served by a fixed pool of threads; a
//! background committer makes new documents searchable within `commit_interval`
//! without the client having to call commit.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::engine::Engine;
use crate::error::{Error, Result};
use crate::highlight::HighlightOpts;
use crate::json::Json;
use crate::search::page::Cursor;

const MAX_HEADER: usize = 16 << 10;
const MAX_LIMIT: usize = 1000;

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub addr: String,
    pub threads: usize,
    /// requests must carry `Authorization: Bearer <token>`; /health stays open
    pub token: Option<String>,
    pub max_body: usize,
    /// None disables the background commit (clients then call /commit themselves)
    pub commit_interval: Option<Duration>,
    pub idle_timeout: Duration,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            addr: "127.0.0.1:8080".into(),
            threads: 8,
            token: None,
            max_body: 16 << 20,
            commit_interval: Some(Duration::from_secs(1)),
            idle_timeout: Duration::from_secs(10),
        }
    }
}

struct State {
    engine: Arc<Engine>,
    cfg: ServerConfig,
    stop: AtomicBool,
    dirty: AtomicBool,
}

pub struct Server {
    addr: SocketAddr,
    state: Arc<State>,
    threads: Vec<JoinHandle<()>>,
}

impl Server {
    pub fn start(engine: Arc<Engine>, cfg: ServerConfig) -> Result<Server> {
        let listener = TcpListener::bind(&cfg.addr)?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let workers = cfg.threads.max(1);
        let commit_every = cfg.commit_interval;
        let state =
            Arc::new(State { engine, cfg, stop: AtomicBool::new(false), dirty: AtomicBool::new(false) });

        let (tx, rx) = mpsc::channel::<TcpStream>();
        let rx = Arc::new(Mutex::new(rx));
        let mut threads = Vec::new();
        for i in 0..workers {
            let (st, rx) = (Arc::clone(&state), Arc::clone(&rx));
            threads
                .push(thread::Builder::new().name(format!("hiis-http-{i}")).spawn(move || worker(st, rx))?);
        }
        let st = Arc::clone(&state);
        threads.push(
            thread::Builder::new().name("hiis-accept".into()).spawn(move || accept_loop(st, listener, tx))?,
        );
        if let Some(every) = commit_every {
            let st = Arc::clone(&state);
            threads
                .push(thread::Builder::new().name("hiis-commit".into()).spawn(move || committer(st, every))?);
        }
        Ok(Server { addr, state, threads })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stop accepting, finish in-flight requests, commit what's pending.
    pub fn shutdown(mut self) -> Result<()> {
        self.stop_threads();
        self.state.engine.commit()
    }

    /// Block until the process is killed (the CLI uses this).
    pub fn wait(mut self) {
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }

    fn stop_threads(&mut self) {
        self.state.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop_threads();
    }
}

fn accept_loop(st: Arc<State>, listener: TcpListener, tx: Sender<TcpStream>) {
    while !st.stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((s, _)) => {
                let _ = s.set_nonblocking(false);
                if tx.send(s).is_err() {
                    return;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(2)),
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn committer(st: Arc<State>, every: Duration) {
    let mut last = Instant::now();
    while !st.stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(10));
        if last.elapsed() >= every {
            last = Instant::now();
            if st.dirty.swap(false, Ordering::AcqRel) {
                let _ = st.engine.commit();
            }
        }
    }
}

fn worker(st: Arc<State>, rx: Arc<Mutex<Receiver<TcpStream>>>) {
    loop {
        // the lock is only held while waiting for a connection, not while serving it
        let next = rx.lock().unwrap().recv_timeout(Duration::from_millis(50));
        match next {
            Ok(stream) => {
                let _ = serve_connection(&st, stream);
            }
            Err(RecvTimeoutError::Timeout) => {
                if st.stop.load(Ordering::Relaxed) {
                    return;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

struct Request {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    close: bool,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    fn param(&self, name: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

struct Response {
    status: u16,
    body: String,
}

fn ok(data: Json, meta: Json) -> Response {
    Response { status: 200, body: Json::obj([("data", data), ("meta", meta)]).to_string() }
}

fn fail(status: u16, code: &str, message: &str) -> Response {
    let e = Json::obj([("code", code.into()), ("message", message.into())]);
    Response { status, body: Json::obj([("error", e)]).to_string() }
}

enum ReadErr {
    Closed,
    Bad(Response),
}

fn serve_connection(st: &State, stream: TcpStream) -> io::Result<()> {
    stream.set_read_timeout(Some(st.cfg.idle_timeout))?;
    stream.set_nodelay(true)?;
    let mut rd = BufReader::new(stream.try_clone()?);
    let mut wr = stream;

    while !st.stop.load(Ordering::Relaxed) {
        let (resp, close) = match read_request(&mut rd, st.cfg.max_body) {
            Ok(req) => {
                let close = req.close;
                (route(st, &req), close)
            }
            Err(ReadErr::Closed) => return Ok(()),
            Err(ReadErr::Bad(r)) => (r, true),
        };
        write_response(&mut wr, &resp, close)?;
        if close {
            return Ok(());
        }
    }
    Ok(())
}

fn bad(status: u16, code: &str, msg: &str) -> ReadErr {
    ReadErr::Bad(fail(status, code, msg))
}

fn read_line(rd: &mut impl BufRead, budget: &mut usize) -> std::result::Result<Option<String>, ReadErr> {
    let mut line = String::new();
    // +1 so a line that exactly fills the budget is still told apart from one that overruns it
    let n = rd.by_ref().take(*budget as u64 + 1).read_line(&mut line).map_err(|_| ReadErr::Closed)?;
    if n == 0 {
        return Ok(None);
    }
    if n > *budget {
        return Err(bad(431, "HEADERS_TOO_LARGE", "request headers are too large"));
    }
    *budget -= n;
    Ok(Some(line.trim_end_matches(['\r', '\n']).to_string()))
}

fn read_request(rd: &mut BufReader<TcpStream>, max_body: usize) -> std::result::Result<Request, ReadErr> {
    let mut budget = MAX_HEADER;
    let Some(first) = read_line(rd, &mut budget)? else { return Err(ReadErr::Closed) };
    let mut parts = first.split_whitespace();
    let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(bad(400, "BAD_REQUEST", "malformed request line"));
    };
    if !version.starts_with("HTTP/1.") {
        return Err(bad(505, "BAD_VERSION", "only HTTP/1.x is supported"));
    }

    let mut headers = Vec::new();
    loop {
        let Some(line) = read_line(rd, &mut budget)? else { return Err(ReadErr::Closed) };
        if line.is_empty() {
            break;
        }
        let Some((k, v)) = line.split_once(':') else {
            return Err(bad(400, "BAD_REQUEST", "malformed header"));
        };
        headers.push((k.trim().to_string(), v.trim().to_string()));
    }

    let find =
        |name: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str());
    if find("transfer-encoding").is_some() {
        return Err(bad(501, "NOT_SUPPORTED", "chunked bodies are not supported, send Content-Length"));
    }
    let len = match find("content-length") {
        None => 0,
        Some(v) => v.parse::<usize>().map_err(|_| bad(400, "BAD_REQUEST", "bad Content-Length"))?,
    };
    if len > max_body {
        return Err(bad(413, "BODY_TOO_LARGE", "request body is too large"));
    }
    let mut body = vec![0u8; len];
    rd.read_exact(&mut body).map_err(|_| ReadErr::Closed)?;

    let http10 = version == "HTTP/1.0";
    let conn = find("connection").map(str::to_ascii_lowercase);
    let close = match conn.as_deref() {
        Some("close") => true,
        Some("keep-alive") => false,
        _ => http10,
    };

    let (path, qs) = target.split_once('?').unwrap_or((target, ""));
    Ok(Request {
        method: method.to_ascii_uppercase(),
        path: percent_decode(path, false),
        query: parse_query(qs),
        headers,
        body,
        close,
    })
}

fn write_response(w: &mut impl Write, r: &Response, close: bool) -> io::Result<()> {
    let reason = match r.status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        501 => "Not Implemented",
        505 => "HTTP Version Not Supported",
        _ => "Internal Server Error",
    };
    write!(
        w,
        "HTTP/1.1 {} {reason}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: {}\r\n\r\n",
        r.status,
        r.body.len(),
        if close { "close" } else { "keep-alive" }
    )?;
    w.write_all(r.body.as_bytes())?;
    w.flush()
}

fn parse_query(qs: &str) -> Vec<(String, String)> {
    qs.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(k, true), percent_decode(v, true))
        })
        .collect()
}

fn percent_decode(s: &str, plus_is_space: bool) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' if plus_is_space => out.push(b' '),
            b'%' if i + 2 < b.len() && b[i + 1].is_ascii_hexdigit() && b[i + 2].is_ascii_hexdigit() => {
                let h = std::str::from_utf8(&b[i + 1..i + 3]).unwrap();
                out.push(u8::from_str_radix(h, 16).unwrap());
                i += 2;
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn authorized(st: &State, req: &Request) -> bool {
    let Some(want) = &st.cfg.token else { return true };
    let got = req.header("authorization").and_then(|h| h.strip_prefix("Bearer ")).unwrap_or("");
    // compare every byte either way, so the time doesn't say how much of the token matched
    got.len() == want.len() && got.bytes().zip(want.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

fn route(st: &State, req: &Request) -> Response {
    if req.path == "/health" {
        return if req.method == "GET" {
            ok("ok".into(), Json::Null)
        } else {
            fail(405, "METHOD_NOT_ALLOWED", "GET only")
        };
    }
    if !authorized(st, req) {
        return fail(401, "UNAUTHORIZED", "missing or wrong bearer token");
    }
    let Some(rest) = req.path.strip_prefix("/api/v1/") else {
        return fail(404, "NOT_FOUND", "no such route");
    };
    let segs: Vec<&str> = rest.trim_end_matches('/').split('/').collect();

    let result = match (req.method.as_str(), segs.as_slice()) {
        ("POST", ["documents"]) => add_documents(st, req),
        ("GET", ["documents", id]) => get_document(st, id),
        ("DELETE", ["documents", id]) => delete_document(st, id),
        ("GET", ["documents", id, "similar"]) => similar(st, req, id),
        ("GET", ["documents", id, "explain"]) => explain(st, req, id),
        ("GET", ["search"]) => search(st, req),
        ("GET", ["suggest"]) => suggest(st, req),
        ("POST", ["commit"]) => commit(st),
        ("GET", ["stats"]) => stats(st),
        (
            _,
            ["documents"]
            | ["documents", _]
            | ["documents", _, _]
            | ["search"]
            | ["suggest"]
            | ["commit"]
            | ["stats"],
        ) => Err(fail(405, "METHOD_NOT_ALLOWED", "method not allowed here")),
        _ => Err(fail(404, "NOT_FOUND", "no such route")),
    };
    match result {
        Ok(r) | Err(r) => r,
    }
}

type Reply = std::result::Result<Response, Response>;

fn invalid(msg: &str) -> Response {
    fail(400, "INVALID_REQUEST", msg)
}

fn internal(e: Error) -> Response {
    match e {
        Error::Invalid(m) => invalid(&m),
        other => fail(500, "INTERNAL", &other.to_string()),
    }
}

fn doc_id(s: &str) -> std::result::Result<u64, Response> {
    s.parse().map_err(|_| invalid("document id must be a number"))
}

fn limit(req: &Request, default: usize) -> std::result::Result<usize, Response> {
    match req.param("limit") {
        None => Ok(default),
        Some(v) => match v.parse::<usize>() {
            Ok(n) if (1..=MAX_LIMIT).contains(&n) => Ok(n),
            _ => Err(invalid(&format!("limit must be between 1 and {MAX_LIMIT}"))),
        },
    }
}

fn body_json(req: &Request) -> std::result::Result<Json, Response> {
    let text = std::str::from_utf8(&req.body).map_err(|_| invalid("body is not valid utf-8"))?;
    Json::parse(text).map_err(|e| invalid(&format!("body is not valid JSON: {e}")))
}

fn add_documents(st: &State, req: &Request) -> Reply {
    let body = body_json(req)?;
    let texts: Vec<&str> = match (body.get("text"), body.get("documents")) {
        (Some(t), None) => vec![t.as_str().ok_or_else(|| invalid("\"text\" must be a string"))?],
        (None, Some(Json::Arr(items))) => items
            .iter()
            .map(|j| j.as_str().ok_or_else(|| invalid("\"documents\" must be an array of strings")))
            .collect::<std::result::Result<_, _>>()?,
        _ => return Err(invalid("send either {\"text\": \"...\"} or {\"documents\": [\"...\"]}")),
    };
    if texts.is_empty() {
        return Err(invalid("no documents"));
    }

    let ids = st.engine.add_documents(&texts).map_err(internal)?;
    st.dirty.store(true, Ordering::Release);
    if req.param("commit") == Some("true") {
        st.engine.commit().map_err(internal)?;
    }
    let data = match body.get("text") {
        Some(_) => Json::obj([("id", ids[0].into())]),
        None => Json::obj([("ids", ids.into())]),
    };
    let mut r = ok(data, Json::Null);
    r.status = 201;
    Ok(r)
}

fn get_document(st: &State, id: &str) -> Reply {
    let id = doc_id(id)?;
    match st.engine.get(id) {
        Some(text) => Ok(ok(Json::obj([("id", id.into()), ("text", text.into())]), Json::Null)),
        None => Err(fail(404, "NOT_FOUND", "no such document")),
    }
}

fn delete_document(st: &State, id: &str) -> Reply {
    let id = doc_id(id)?;
    if !st.engine.delete(id).map_err(internal)? {
        return Err(fail(404, "NOT_FOUND", "no such document"));
    }
    st.dirty.store(true, Ordering::Release);
    Ok(ok(Json::obj([("deleted", true.into())]), Json::Null))
}

fn hit_json(st: &State, h: crate::search::Hit, query: Option<&str>) -> Json {
    let mut f = vec![("id", Json::from(h.doc_id)), ("score", h.score.into())];
    if let Some(q) = query {
        let snip = st.engine.highlight(h.doc_id, q, &HighlightOpts::default()).map(|s| s.text);
        f.push(("snippet", snip.into()));
    }
    Json::Obj(f.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

fn search(st: &State, req: &Request) -> Reply {
    let q = req.param("q").filter(|q| !q.trim().is_empty()).ok_or_else(|| invalid("q is required"))?;
    let k = limit(req, 10)?;
    let after = match req.param("cursor") {
        None | Some("") => None,
        Some(c) => Some(c.parse::<Cursor>().map_err(internal)?),
    };
    let snippets = matches!(req.param("highlight"), Some("1" | "true"));

    let t = Instant::now();
    let page = st.engine.search_page(q, k, after).map_err(internal)?;
    let took = t.elapsed();

    let mut meta = vec![
        ("took_ms", Json::Num(took.as_secs_f64() * 1000.0)),
        ("next_cursor", page.next.map(|c| c.to_string()).into()),
    ];
    if matches!(req.param("count"), Some("1" | "true")) {
        meta.push(("total", st.engine.count(q).map_err(internal)?.into()));
    }
    if page.hits.is_empty() && after.is_none() {
        meta.push(("did_you_mean", st.engine.did_you_mean(q).into()));
    }
    let data: Vec<Json> = page.hits.into_iter().map(|h| hit_json(st, h, snippets.then_some(q))).collect();
    Ok(ok(Json::Arr(data), Json::Obj(meta.into_iter().map(|(k, v)| (k.to_string(), v)).collect())))
}

fn similar(st: &State, req: &Request, id: &str) -> Reply {
    let id = doc_id(id)?;
    if st.engine.get(id).is_none() {
        return Err(fail(404, "NOT_FOUND", "no such document"));
    }
    let hits = st.engine.similar(id, limit(req, 5)?).map_err(internal)?;
    Ok(ok(Json::Arr(hits.into_iter().map(|h| hit_json(st, h, None)).collect()), Json::Null))
}

fn explain(st: &State, req: &Request, id: &str) -> Reply {
    let id = doc_id(id)?;
    let q = req.param("q").ok_or_else(|| invalid("q is required"))?;
    match st.engine.explain(q, id).map_err(internal)? {
        None => Err(fail(404, "NOT_FOUND", "no such document")),
        Some(x) => Ok(ok(
            Json::obj([
                ("matched", x.matched.into()),
                ("score", x.score.into()),
                ("explanation", x.to_string().into()),
            ]),
            Json::Null,
        )),
    }
}

fn suggest(st: &State, req: &Request) -> Reply {
    let p = req.param("prefix").ok_or_else(|| invalid("prefix is required"))?;
    let out: Vec<Json> = st
        .engine
        .complete(p, limit(req, 10)?)
        .into_iter()
        .map(|s| Json::obj([("text", s.text.into()), ("doc_freq", s.doc_freq.into())]))
        .collect();
    Ok(ok(Json::Arr(out), Json::Null))
}

fn commit(st: &State) -> Reply {
    st.engine.commit().map_err(internal)?;
    st.dirty.store(false, Ordering::Release);
    Ok(ok(Json::obj([("committed", true.into())]), Json::Null))
}

fn stats(st: &State) -> Reply {
    let s = st.engine.stats();
    Ok(ok(
        Json::obj([
            ("segments", s.segments.into()),
            ("live_docs", s.live_docs.into()),
            ("deleted_docs", s.deleted_docs.into()),
            ("buffered_docs", s.buffered_docs.into()),
            ("terms", s.terms.into()),
            ("disk_bytes", s.disk_bytes.into()),
        ]),
        Json::Null,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("a%20b+c", true), "a b c");
        assert_eq!(percent_decode("a%20b+c", false), "a b+c");
        assert_eq!(percent_decode("%D9%85%D8%B1%D8%AD%D8%A8%D8%A7", true), "مرحبا");
        // broken escapes pass through
        assert_eq!(percent_decode("100%", true), "100%");
        assert_eq!(percent_decode("%zz%4", true), "%zz%4");
        assert_eq!(percent_decode("%ff", true), "\u{fffd}");
    }

    #[test]
    fn query_strings() {
        let q = parse_query("q=%22quick+fox%22&limit=5&flag&&x=");
        assert_eq!(q[0], ("q".into(), "\"quick fox\"".into()));
        assert_eq!(q[2], ("flag".into(), String::new()));
        assert_eq!(q.len(), 4);
    }
}
