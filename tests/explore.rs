//! The HTTP layer of `explore`: a real socket, every route, a clean stop.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;

use pipeline::explore::{self, ExploreOptions, Explorer};
use pipeline::meta::Meta;
use pipeline::store::{self, Record};

fn request(addr: SocketAddr, method: &str, path: &str) -> (u16, String, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    write!(s, "{method} {path} HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\n\r\n").unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).unwrap();
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status: u16 = text.split_whitespace().nth(1).unwrap().parse().unwrap();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    (status, head.to_string(), body.to_string())
}

fn store_with(dir: &Path, n: u8) {
    let store = store::open(dir, true).unwrap();
    let recs: Vec<Record> = (1..=n)
        .map(|i| Record {
            key: [i; 16],
            meta: Meta { words: u32::from(i), ..Default::default() }.encode(),
            text: format!("doc {i}").into_bytes(),
            extra: None,
            replace: false,
        })
        .collect();
    store.put_batch(&recs).unwrap();
    store.put_source(0, "cli").unwrap();
    store.persist().unwrap();
}

#[test]
fn every_route_over_a_socket() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db");
    store_with(&db, 3);
    let opts = ExploreOptions { db: db.clone(), bind: "127.0.0.1".into(), port: 0, threads: 2 };
    let explorer = Explorer::open(&opts).unwrap();
    let server = explore::bind(explorer, &opts.bind, opts.port).unwrap();
    let addr = server.addr();
    std::thread::scope(|s| {
        s.spawn(|| server.serve(2));

        let (status, head, body) = request(addr, "GET", "/");
        assert_eq!(status, 200);
        assert!(head.contains("Content-Type: text/html; charset=utf-8"), "{head}");
        assert!(head.contains("Cache-Control: no-store"), "{head}");
        assert!(body.starts_with("<!doctype html>"), "{body}");

        let (status, head, body) = request(addr, "GET", "/api/info");
        assert_eq!(status, 200);
        assert!(head.contains("Content-Type: application/json; charset=utf-8"), "{head}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["dir"], db.to_str().unwrap());
        assert_eq!(v["sources"], serde_json::json!(["cli"]));

        let id = pipeline::hash::key_to_hex(&[2; 16]);
        let (status, _, body) = request(addr, "GET", &format!("/api/doc/{id}"));
        assert_eq!(status, 200);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["text"], "doc 2");

        let (status, _, body) = request(addr, "GET", "/api/random?expr=words%20%3E%202&tries=10");
        assert_eq!(status, 200, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["features"]["words"], 3);

        let (status, _, body) = request(addr, "GET", &format!("/api/next/{id}?n=1"));
        assert_eq!(status, 200);
        assert!(body.contains("\"doc 3\""), "{body}");
        let (status, _, body) = request(addr, "GET", &format!("/api/prev/{id}"));
        assert_eq!(status, 200);
        assert!(body.contains("\"doc 1\""), "{body}");

        let (status, _, body) = request(addr, "GET", "/api/query?limit=2&head=5");
        assert_eq!(status, 200);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["rows"].as_array().unwrap().len(), 2);
        assert_eq!(v["rows"][0]["head"], "doc 1");

        let (status, _, body) = request(addr, "GET", "/api/count?expr=words%20%3C%203");
        assert_eq!(status, 200);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!((v["scanned"].as_u64().unwrap(), v["matched"].as_u64().unwrap()), (3, 2));

        let (status, _, body) = request(addr, "GET", "/nope");
        assert_eq!(status, 404);
        assert!(body.contains("no route"), "{body}");
        let (status, _, body) = request(addr, "POST", "/api/info");
        assert_eq!(status, 405);
        assert!(body.contains("GET only"), "{body}");
        let (status, _, _) = request(addr, "GET", "/api/random?expr=(");
        assert_eq!(status, 400);

        server.stop();
    });
    // once the server is dropped the port is free again
    drop(server);
    let mut released = false;
    for _ in 0..40 {
        if TcpStream::connect(addr).is_err() {
            released = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(released, "the listener stayed open after stop and drop");
}
