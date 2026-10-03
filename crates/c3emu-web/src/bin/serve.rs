//! Minimal static file server for the web build: `cargo run --release -p c3emu-web --bin
//! serve` serves ./site (scripts/build-web.sh; or the directory given) on
//! http://localhost:8080 (or $PORT).

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};

fn mime(p: &Path) -> &'static str {
    match p.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "wasm" => "application/wasm",
        "png" => "image/png",
        "css" => "text/css",
        "json" => "application/json",
        _ => "application/octet-stream",
    }
}

fn serve(mut s: TcpStream, root: &Path) -> std::io::Result<()> {
    let mut line = String::new();
    let mut r = BufReader::new(s.try_clone()?);
    r.read_line(&mut line)?;
    loop {
        let mut h = String::new();
        if r.read_line(&mut h)? == 0 || h == "\r\n" || h == "\n" {
            break;
        }
    }
    let path = line.split_whitespace().nth(1).unwrap_or("/").split('?').next().unwrap_or("/");
    let rel = path.trim_start_matches('/');
    let mut file = root.join(if rel.is_empty() { "index.html" } else { rel });
    if rel.split('/').any(|c| c == "..") {
        file = PathBuf::new();
    }
    match std::fs::read(&file) {
        Ok(body) => {
            write!(s, "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
                   mime(&file), body.len())?;
            s.write_all(&body)
        }
        Err(_) => s.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot found"),
    }
}

fn main() {
    let root = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../site").to_string()
    }));
    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".into());
    let l = TcpListener::bind(format!("127.0.0.1:{port}")).expect("bind");
    println!("c3emu web: http://localhost:{port}/  (serving {})", root.display());
    for s in l.incoming().flatten() {
        let root = root.clone();
        std::thread::spawn(move || {
            let _ = serve(s, &root);
        });
    }
}
