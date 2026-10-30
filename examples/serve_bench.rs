//! cargo run --release --example serve_bench -- [docs] [clients] [seconds]
//!
//! Requests per second and latency of GET /api/v1/search over keep-alive connections.
//! Client threads and the server share this machine, so they fight for the same cores.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hiis::server::{Server, ServerConfig};
use hiis::{Config, Engine};

fn get(s: &mut TcpStream, target: &str) -> std::io::Result<()> {
    write!(s, "GET {target} HTTP/1.1\r\nHost: x\r\n\r\n")?;
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        s.read_exact(&mut b)?;
        head.push(b[0]);
    }
    let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
    let len: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:").and_then(|v| v.trim().parse().ok()))
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    s.read_exact(&mut body)
}

fn main() -> hiis::Result<()> {
    let arg = |i: usize, d: usize| std::env::args().nth(i).and_then(|a| a.parse().ok()).unwrap_or(d);
    let (docs, clients, secs) = (arg(1, 20_000), arg(2, 8), arg(3, 3));

    let dir = std::env::temp_dir().join(format!("hiis-serve-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let engine = Arc::new(Engine::open(&dir, Config::default())?);
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let batch: Vec<String> = (0..docs)
        .map(|_| {
            (0..60).map(|_| format!("w{}", next() % 5000 % (1 + next() % 5000))).collect::<Vec<_>>().join(" ")
        })
        .collect();
    for c in batch.chunks(1000) {
        engine.add_documents(c)?;
    }
    engine.commit()?;

    let cfg = ServerConfig { addr: "127.0.0.1:0".into(), ..Default::default() };
    let server = Server::start(Arc::clone(&engine), cfg)?;
    let addr = server.local_addr();
    println!("{docs} docs, {clients} clients, {secs}s per round, keep-alive");

    let queries = ["w1", "w2 w40", "w3 w700 w2000", "\"w1 w2\"", "w5*"];
    for round in 1..=3 {
        let stop = Instant::now() + Duration::from_secs(secs as u64);
        let mut lat: Vec<f64> = std::thread::scope(|sc| {
            let jobs: Vec<_> = (0..clients)
                .map(|c| {
                    sc.spawn(move || {
                        let mut s = TcpStream::connect(addr).unwrap();
                        s.set_nodelay(true).unwrap();
                        let mut v = Vec::new();
                        let mut i = c;
                        while Instant::now() < stop {
                            let q = queries[i % queries.len()].replace(' ', "%20").replace('"', "%22");
                            let t = Instant::now();
                            get(&mut s, &format!("/api/v1/search?q={q}&k=10")).unwrap();
                            v.push(t.elapsed().as_micros() as f64);
                            i += 1;
                        }
                        v
                    })
                })
                .collect();
            jobs.into_iter().flat_map(|j| j.join().unwrap()).collect()
        });
        lat.sort_by(f64::total_cmp);
        let at = |p: usize| lat[(lat.len() * p / 100).min(lat.len() - 1)];
        println!(
            "round {round}: {:.0} req/s   p50 {:.0} us   p95 {:.0} us   p99 {:.0} us",
            lat.len() as f64 / secs as f64,
            at(50),
            at(95),
            at(99)
        );
    }
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
