//! `pipeline explore`: a read-only browser over a store.
//!
//! One page of vanilla JavaScript (`index.html`, embedded in the binary)
//! and a small JSON API on localhost. Random works because keys are
//! uniform hashes: draw 16 random bytes and seek to the first key at or
//! after them. With an expression the seek is followed by a short forward
//! scan, which is what "a random document that matches" costs. The server
//! answers GET only; nothing here writes to the store.
//!
//! Routes (all GET, all JSON except the page):
//!
//! ```text
//! /                          the page
//! /api/info                  store facts; reads no record
//! /api/doc/{id}              one document, by full id or prefix
//! /api/random?expr=&tries=   a random document, matching expr when given
//! /api/next[/{id}]?n=1       the n-th document after id (from the start without an id)
//! /api/prev[/{id}]?n=1       the n-th document before id (from the end without an id)
//! /api/query?expr=&from=&dir=&limit=&scan=&head=
//!                            matches in key order from a cursor, with a text snippet
//! /api/count?expr=           a full parallel scan
//! ```
//!
//! Handlers are plain functions from a method and a URL to a [`Reply`], so
//! they are tested without a socket; [`Server`] is the thin tiny_http layer.

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::error::{Error, Result};
use crate::expr::{self, Compiled};
use crate::hash::{Key, key_from_hex, key_to_hex};
use crate::meta::{FEATURES, Kind, Meta, SCHEMA_VERSION};
use crate::show::{self, resolve_id};
use crate::stats::{ScanConfig, matches as accepts, scan};
use crate::store::{self, Keyspace, KvIter, Sources, Store};
use crate::util::{fmt_int, fmt_mb};

pub const INDEX_HTML: &str = include_str!("index.html");
pub const DEFAULT_PORT: u16 = 8080;
/// Documents a filtered random draw examines before giving up.
pub const DEFAULT_TRIES: u64 = 10_000;
pub const MAX_TRIES: u64 = 1_000_000;
/// Rows one query returns.
pub const DEFAULT_LIMIT: usize = 50;
pub const MAX_LIMIT: usize = 1000;
/// Documents one query examines before returning what it has.
pub const DEFAULT_SCAN: u64 = 1_000_000;
/// Characters of text in a query row.
pub const DEFAULT_HEAD: usize = 160;
/// Request handler threads.
pub const WORKERS: usize = 4;

#[derive(Debug, Clone)]
pub struct ExploreOptions {
    pub db: PathBuf,
    pub bind: String,
    pub port: u16,
    /// Scan threads for `count`.
    pub threads: usize,
}

/// One HTTP answer, independent of the server library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    pub content_type: &'static str,
    pub body: String,
}

impl Reply {
    pub fn json(status: u16, v: &Value) -> Reply {
        Reply { status, content_type: "application/json; charset=utf-8", body: v.to_string() }
    }

    pub fn error(status: u16, msg: impl Into<String>) -> Reply {
        Reply::json(status, &json!({ "error": msg.into() }))
    }

    pub fn html(body: &str) -> Reply {
        Reply { status: 200, content_type: "text/html; charset=utf-8", body: body.to_string() }
    }

    /// Caller mistakes are 400, everything else is the store's fault.
    fn from_error(e: Error) -> Reply {
        let status = match e {
            Error::Usage(_) | Error::Expr(_) => 400,
            _ => 500,
        };
        Reply::error(status, e.to_string())
    }

    /// The parsed body, for tests and the banner.
    pub fn value(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

/// A parsed request line: method, path and decoded query parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub params: Vec<(String, String)>,
}

impl Request {
    pub fn parse(method: &str, url: &str) -> Request {
        let (path, query) = url.split_once('?').unwrap_or((url, ""));
        let params = query
            .split('&')
            .filter(|p| !p.is_empty())
            .map(|p| {
                let (k, v) = p.split_once('=').unwrap_or((p, ""));
                (percent_decode(k), percent_decode(v))
            })
            .collect();
        Request { method: method.to_string(), path: path.to_string(), params }
    }

    pub fn param(&self, name: &str) -> Option<&str> {
        self.params.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// A numeric parameter, `default` when absent or empty.
    pub fn num<T: FromStr>(&self, name: &str, default: T) -> Result<T> {
        match self.param(name) {
            None | Some("") => Ok(default),
            Some(v) => v.trim().parse().map_err(|_| Error::Usage(format!("{name} must be a whole number, not {v:?}"))),
        }
    }
}

/// `%XX` and `+` decoding; bad escapes stay as they are.
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = |b: u8| (b as char).to_digit(16);
                match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dir {
    Next,
    Prev,
}

impl Dir {
    fn as_str(self) -> &'static str {
        match self {
            Dir::Next => "next",
            Dir::Prev => "prev",
        }
    }
}

/// The key right after `key`, `None` past the last one.
pub fn succ(key: &Key) -> Option<Key> {
    let mut k = *key;
    for b in k.iter_mut().rev() {
        if *b == u8::MAX {
            *b = 0;
        } else {
            *b += 1;
            return Some(k);
        }
    }
    None
}

/// Sixteen random bytes from the standard library's per-process keys.
pub fn random_key() -> Key {
    let s = RandomState::new();
    let mut k = [0u8; 16];
    k[..8].copy_from_slice(&s.hash_one(1u64).to_be_bytes());
    k[8..].copy_from_slice(&s.hash_one(2u64).to_be_bytes());
    k
}

fn is_not_found(e: &Error) -> bool {
    matches!(e, Error::Usage(m) if m.contains("no such document") || m.contains("no document starts with"))
}

fn secs(t0: Instant) -> f64 {
    (t0.elapsed().as_secs_f64() * 1000.0).round() / 1000.0
}

/// Every feature by name: integers as integers, percents and bits as the
/// presented decimal, `null` when the record predates the feature.
fn features_json(meta: &Meta) -> Value {
    let mut m = Map::new();
    for f in FEATURES {
        let v = match ((f.raw)(meta), f.kind) {
            (None, _) => Value::Null,
            (Some(raw), Kind::Int) => json!(raw),
            (Some(raw), _) => json!(f.present(raw as f64)),
        };
        m.insert(f.name.to_string(), v);
    }
    Value::Object(m)
}

fn extra_json(extra: Option<&[u8]>) -> Result<Value> {
    match extra {
        None => Ok(Value::Null),
        Some(b) => serde_json::from_slice(b).map_err(|e| Error::Store(format!("stored extra is not JSON: {e}"))),
    }
}

/// An open store and the handlers over it.
pub struct Explorer {
    store: Box<dyn Store>,
    sources: Sources,
    dir: String,
    threads: usize,
}

impl Explorer {
    pub fn new(store: Box<dyn Store>, sources: Sources, dir: String, threads: usize) -> Explorer {
        Explorer { store, sources, dir, threads: threads.max(1) }
    }

    pub fn open(opts: &ExploreOptions) -> Result<Explorer> {
        let store = store::open(&opts.db, false)?;
        let sources = Sources::load(store.as_ref())?;
        Ok(Explorer::new(store, sources, opts.db.display().to_string(), opts.threads))
    }

    pub fn dir(&self) -> &str {
        &self.dir
    }

    /// Route one request. Never fails: errors become JSON replies.
    pub fn handle(&self, method: &str, url: &str) -> Reply {
        let req = Request::parse(method, url);
        if req.method != "GET" {
            return Reply::error(405, "read-only server: GET only");
        }
        let result = match req.path.as_str() {
            "/" | "/index.html" => return Reply::html(INDEX_HTML),
            "/api/info" => self.info(),
            "/api/random" => self.random(&req),
            "/api/query" => self.query(&req),
            "/api/count" => self.count(&req),
            p => {
                if let Some(id) = p.strip_prefix("/api/doc/") {
                    self.doc(id)
                } else if let Some(rest) = p.strip_prefix("/api/next") {
                    self.nav(Dir::Next, rest, &req)
                } else if let Some(rest) = p.strip_prefix("/api/prev") {
                    self.nav(Dir::Prev, rest, &req)
                } else {
                    Ok(Reply::error(404, format!("no route {p}")))
                }
            }
        };
        result.unwrap_or_else(Reply::from_error)
    }

    fn info(&self) -> Result<Reply> {
        let mut labels: Vec<(u16, &str)> = self.sources.iter().collect();
        labels.sort_by(|a, b| a.1.cmp(b.1));
        let features: Vec<Value> = FEATURES.iter().map(|f| json!({ "name": f.name, "decimals": f.decimals() })).collect();
        let details: Vec<Value> = self.store.details()?.into_iter().map(|(k, v)| json!([k, v])).collect();
        Ok(Reply::json(
            200,
            &json!({
                "dir": self.dir,
                "approx_docs": self.store.approx_count()?,
                "disk": self.store.disk_size()?,
                "sources": labels.into_iter().map(|(_, n)| n).collect::<Vec<_>>(),
                "features": features,
                "schema": SCHEMA_VERSION,
                "tokenizer": crate::tokenizer::State::read(self.store.as_ref())?.describe(),
                "details": details,
            }),
        ))
    }

    fn document(&self, key: &Key) -> Result<Value> {
        let doc = show::fetch(self.store.as_ref(), key)?;
        let extra = extra_json(doc.extra.as_deref())?;
        Ok(json!({
            "id": key_to_hex(key),
            "source": self.sources.name(doc.meta.source),
            "features": features_json(&doc.meta),
            "extra": extra,
            "text": doc.text,
        }))
    }

    fn resolve(&self, id: &str) -> std::result::Result<Key, Reply> {
        resolve_id(self.store.as_ref(), id).map_err(|e| Reply::error(if is_not_found(&e) { 404 } else { 400 }, e.to_string()))
    }

    fn doc(&self, id: &str) -> Result<Reply> {
        match self.resolve(&percent_decode(id)) {
            Ok(key) => Ok(Reply::json(200, &self.document(&key)?)),
            Err(reply) => Ok(reply),
        }
    }

    fn filter(req: &Request) -> Result<Option<Compiled>> {
        match req.param("expr").map(str::trim) {
            None | Some("") => Ok(None),
            Some(e) => Ok(Some(expr::compile(e)?)),
        }
    }

    fn hit(&self, filter: Option<&Compiled>, key: &Key, meta: &Meta) -> Result<bool> {
        match filter {
            None => Ok(true),
            Some(f) => accepts(self.store.as_ref(), &self.sources, f, key, meta),
        }
    }

    fn random(&self, req: &Request) -> Result<Reply> {
        let filter = Self::filter(req)?;
        let tries = req.num("tries", DEFAULT_TRIES)?.clamp(1, MAX_TRIES);
        let t0 = Instant::now();
        let start = random_key();
        let mut looked = 0u64;
        'draw: for (lo, hi) in [(start, None), ([0u8; 16], Some(start))] {
            for item in self.store.iter(Keyspace::Meta, &lo, hi.as_ref(), true)? {
                let (key, bytes) = item?;
                looked += 1;
                let meta = Meta::decode(&bytes)?;
                if self.hit(filter.as_ref(), &key, &meta)? {
                    let mut v = self.document(&key)?;
                    v["looked_at"] = json!(looked);
                    v["secs"] = json!(secs(t0));
                    return Ok(Reply::json(200, &v));
                }
                if looked >= tries {
                    break 'draw;
                }
            }
        }
        Ok(Reply::error(
            404,
            if filter.is_none() {
                "the store is empty".to_string()
            } else {
                format!("no match in {} documents from a random start; raise tries or loosen the expression", fmt_int(looked))
            },
        ))
    }

    /// Keys in `dir` from `from` (exclusive), or from the matching end.
    fn walk<'a>(&'a self, dir: Dir, from: Option<&Key>, values: bool) -> Result<KvIter<'a>> {
        let first: Key = [0; 16];
        match (dir, from) {
            (Dir::Next, None) => self.store.iter(Keyspace::Meta, &first, None, values),
            (Dir::Next, Some(k)) => match succ(k) {
                Some(lo) => self.store.iter(Keyspace::Meta, &lo, None, values),
                None => Ok(Box::new(std::iter::empty())),
            },
            (Dir::Prev, None) => self.store.iter_rev(Keyspace::Meta, &first, None, values),
            (Dir::Prev, Some(k)) => self.store.iter_rev(Keyspace::Meta, &first, Some(k), values),
        }
    }

    fn nav(&self, dir: Dir, rest: &str, req: &Request) -> Result<Reply> {
        let from = match rest {
            "" | "/" => None,
            r if r.starts_with('/') => Some(percent_decode(&r[1..])),
            _ => return Ok(Reply::error(404, format!("no route {}", req.path))),
        };
        let n: u64 = req.num("n", 1)?;
        if n == 0 {
            return Err(Error::Usage("n must be at least 1".into()));
        }
        let from_key = match &from {
            None => None,
            Some(id) => match self.resolve(id) {
                Ok(k) => Some(k),
                Err(reply) => return Ok(reply),
            },
        };
        let mut last = None;
        let mut stepped = 0u64;
        for item in self.walk(dir, from_key.as_ref(), false)?.take(usize::try_from(n).unwrap_or(usize::MAX)) {
            let (key, _) = item?;
            last = Some(key);
            stepped += 1;
        }
        match last {
            Some(key) => {
                let mut v = self.document(&key)?;
                v["stepped"] = json!(stepped);
                Ok(Reply::json(200, &v))
            }
            None => Ok(Reply::error(
                404,
                match (dir, from) {
                    (Dir::Next, Some(id)) => format!("no document after {id}"),
                    (Dir::Prev, Some(id)) => format!("no document before {id}"),
                    _ => "the store is empty".to_string(),
                },
            )),
        }
    }

    fn row(&self, key: &Key, meta: &Meta, head: usize) -> Result<Value> {
        let snippet = if head == 0 {
            Value::Null
        } else {
            let text = self.store.get_text(key)?.ok_or_else(|| Error::Store(format!("text missing for document {}", key_to_hex(key))))?;
            Value::String(String::from_utf8_lossy(&text).chars().take(head).collect())
        };
        Ok(json!({ "id": key_to_hex(key), "source": self.sources.name(meta.source), "features": features_json(meta), "head": snippet }))
    }

    fn query(&self, req: &Request) -> Result<Reply> {
        let filter = Self::filter(req)?;
        let dir = match req.param("dir").unwrap_or("next") {
            "next" | "" => Dir::Next,
            "prev" => Dir::Prev,
            other => {
                return Err(Error::Usage(format!("dir must be next or prev, not {other:?}")));
            }
        };
        let limit = req.num("limit", DEFAULT_LIMIT)?.clamp(1, MAX_LIMIT);
        let max_scan = req.num("scan", DEFAULT_SCAN)?.max(1);
        let head: usize = req.num("head", DEFAULT_HEAD)?;
        let from = match req.param("from").map(str::trim) {
            None | Some("") => None,
            Some(h) => Some(key_from_hex(h).ok_or_else(|| Error::Usage(format!("from must be a full 32-character id, not {h:?}")))?),
        };
        let t0 = Instant::now();
        let mut rows = Vec::new();
        let mut scanned = 0u64;
        let mut last = None;
        let mut done = true;
        for item in self.walk(dir, from.as_ref(), true)? {
            let (key, bytes) = item?;
            scanned += 1;
            last = Some(key);
            let meta = Meta::decode(&bytes)?;
            if self.hit(filter.as_ref(), &key, &meta)? {
                rows.push(self.row(&key, &meta, head)?);
            }
            if rows.len() >= limit || scanned >= max_scan {
                done = false;
                break;
            }
        }
        Ok(Reply::json(
            200,
            &json!({
                "expr": filter.as_ref().map(|f| f.source.clone()),
                "dir": dir.as_str(),
                "matched": rows.len(),
                "rows": rows,
                "scanned": scanned,
                "last": last.map(|k| key_to_hex(&k)),
                "done": done,
                "secs": secs(t0),
            }),
        ))
    }

    fn count(&self, req: &Request) -> Result<Reply> {
        let filter = Self::filter(req)?;
        let t0 = Instant::now();
        let cfg = ScanConfig { filter: filter.as_ref(), threads: self.threads, limit: None };
        let (scanned, matched, _) = scan(self.store.as_ref(), &self.sources, &cfg, || (), |(), _, _| true)?;
        let pct = if scanned == 0 { 0.0 } else { matched as f64 * 100.0 / scanned as f64 };
        Ok(Reply::json(
            200,
            &json!({
                "expr": filter.as_ref().map(|f| f.source.clone()),
                "scanned": scanned,
                "matched": matched,
                "pct": pct,
                "secs": secs(t0),
            }),
        ))
    }
}

/// The listening socket plus the handlers behind it.
pub struct Server {
    inner: tiny_http::Server,
    explorer: Explorer,
    stopped: AtomicBool,
}

pub fn bind(explorer: Explorer, bind: &str, port: u16) -> Result<Server> {
    let addr = format!("{bind}:{port}");
    let inner = tiny_http::Server::http(&addr).map_err(|e| Error::Usage(format!("cannot listen on {addr}: {e}")))?;
    Ok(Server { inner, explorer, stopped: AtomicBool::new(false) })
}

impl Server {
    pub fn addr(&self) -> SocketAddr {
        self.inner.server_addr().to_ip().expect("a TCP listener has an address")
    }

    pub fn explorer(&self) -> &Explorer {
        &self.explorer
    }

    /// Answer requests on `workers` threads until [`Server::stop`].
    pub fn serve(&self, workers: usize) {
        std::thread::scope(|s| {
            for _ in 0..workers.max(1) {
                s.spawn(|| {
                    while !self.stopped.load(Ordering::Relaxed) {
                        if let Ok(Some(req)) = self.inner.recv_timeout(Duration::from_millis(200)) {
                            self.answer(req);
                        }
                    }
                });
            }
        });
    }

    fn answer(&self, req: tiny_http::Request) {
        let reply = self.explorer.handle(req.method().as_str(), req.url());
        let header = |k: &str, v: &str| tiny_http::Header::from_bytes(k, v).expect("static header");
        let resp = tiny_http::Response::from_data(reply.body.into_bytes())
            .with_status_code(reply.status)
            .with_header(header("Content-Type", reply.content_type))
            .with_header(header("Cache-Control", "no-store"));
        // a client that went away is not our problem
        let _ = req.respond(resp);
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
        self.inner.unblock();
    }
}

/// What the command prints before serving.
pub fn banner(server: &Server) -> Result<String> {
    let ex = &server.explorer;
    Ok(format!(
        "Explore: http://{}/\n  Store: {} (about {} documents, {}, {} sources)\n  Read-only; press Ctrl-C to stop.\n",
        server.addr(),
        ex.dir,
        fmt_int(ex.store.approx_count()?),
        fmt_mb(ex.store.disk_size()?),
        ex.sources.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::feature_names;
    use crate::store::{NullStore, Record};

    /// `n` documents keyed `[i; 16]` plus one keyed `[1, .., 1, 9]`, so the
    /// prefix `0101` is ambiguous. Odd keys carry `extra`; sources alternate.
    fn store_with(n: u8) -> (tempfile::TempDir, Explorer) {
        let dir = tempfile::tempdir().unwrap();
        let store = store::open(&dir.path().join("db"), true).unwrap();
        let rec = |key: Key, i: u8, words: u32| Record {
            key,
            meta: Meta { words, source: u16::from(i % 2), entropy: Some(9000), ..Default::default() }.encode(),
            text: format!("doc {i} has words").into_bytes(),
            extra: (i % 2 == 1).then(|| format!("{{\"year\":{}}}", 1800 + u32::from(i)).into_bytes()),
            replace: false,
        };
        let mut recs: Vec<Record> = (1..=n).map(|i| rec([i; 16], i, u32::from(i))).collect();
        recs.push(rec(amb(), 0, 100));
        store.put_batch(&recs).unwrap();
        let mut sources = Sources::default();
        sources.intern("cli", store.as_ref()).unwrap();
        sources.intern("gut", store.as_ref()).unwrap();
        (dir, Explorer::new(store, sources, "/x".into(), 2))
    }

    fn amb() -> Key {
        let mut k = [1u8; 16];
        k[15] = 9;
        k
    }

    fn hex(i: u8) -> String {
        key_to_hex(&[i; 16])
    }

    fn get(ex: &Explorer, url: &str) -> (u16, Value) {
        let r = ex.handle("GET", url);
        assert!(r.content_type.starts_with("application/json"), "{url}");
        (r.status, r.value())
    }

    fn ids(v: &Value) -> Vec<String> {
        v["rows"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap().to_string()).collect()
    }

    #[test]
    fn request_parsing_and_decoding() {
        let r = Request::parse("GET", "/api/query?expr=words%20%3E%201%26%26+a&limit=5&flag&=x");
        assert_eq!(r.path, "/api/query");
        assert_eq!(r.param("expr"), Some("words > 1&& a"));
        assert_eq!(r.param("flag"), Some(""));
        assert_eq!(r.param(""), Some("x"));
        assert_eq!(r.param("nope"), None);
        assert_eq!(r.num("limit", 1u64).unwrap(), 5);
        assert_eq!(r.num("nope", 7u64).unwrap(), 7);
        assert_eq!(r.num("flag", 9u64).unwrap(), 9, "empty means default");
        let e = r.num::<u64>("expr", 1).unwrap_err().to_string();
        assert!(e.contains("expr must be a whole number"), "{e}");
        assert_eq!(Request::parse("GET", "/").params, vec![]);
        assert_eq!(percent_decode("a+b%zz%2"), "a b%zz%2");
        assert_eq!(percent_decode("%C3%A9%2F"), "é/");
        assert_eq!(percent_decode("%ff"), "\u{FFFD}", "invalid UTF-8 is replaced, not refused");
    }

    #[test]
    fn successor_and_random_keys() {
        let mut one = [0u8; 16];
        one[15] = 1;
        assert_eq!(succ(&[0; 16]), Some(one));
        let mut carry = [0u8; 16];
        carry[14] = 1;
        let mut before = [0u8; 16];
        before[15] = 0xff;
        assert_eq!(succ(&before), Some(carry));
        assert_eq!(succ(&[0xff; 16]), None);
        assert_ne!(random_key(), random_key(), "two draws differ");
    }

    #[test]
    fn page_methods_and_unknown_routes() {
        let (_dir, ex) = store_with(2);
        let r = ex.handle("GET", "/");
        assert_eq!((r.status, r.content_type), (200, "text/html; charset=utf-8"));
        assert!(r.body.contains("pipeline explore") && r.body.contains("/api/random"));
        assert_eq!(ex.handle("GET", "/index.html").status, 200);
        let (s, v) = get(&ex, "/nope");
        assert_eq!((s, v["error"].as_str().unwrap()), (404, "no route /nope"));
        let (s, _) = get(&ex, "/api/nextfoo");
        assert_eq!(s, 404);
        let r = ex.handle("POST", "/api/info");
        assert_eq!(r.status, 405);
        assert!(r.body.contains("GET only"));
        assert_eq!(Reply::from_error(Error::Store("x".into())).status, 500);
        assert_eq!(Reply::from_error(Error::Usage("x".into())).status, 400);
        assert_eq!(Reply::from_error(Error::Expr("x".into())).status, 400);
        assert_eq!(Reply::html("<p>").value(), Value::Null);
        assert_eq!(ex.dir(), "/x");
    }

    #[test]
    fn info_lists_the_store_facts() {
        let (_dir, ex) = store_with(3);
        let (s, v) = get(&ex, "/api/info");
        assert_eq!(s, 200);
        assert_eq!(v["dir"], "/x");
        assert!(v["approx_docs"].as_u64().unwrap() >= 1);
        assert!(v["disk"].as_u64().unwrap() > 0);
        assert_eq!(v["sources"], json!(["cli", "gut"]));
        let names: Vec<&str> = v["features"].as_array().unwrap().iter().map(|f| f["name"].as_str().unwrap()).collect();
        assert_eq!(names, feature_names());
        assert_eq!(v["features"][6]["decimals"], 2, "vowel is a percent");
        assert_eq!(v["schema"], SCHEMA_VERSION);
        assert!(v["details"].as_array().unwrap().iter().any(|d| d[0] == "meta"));
    }

    #[test]
    fn one_document_by_id_or_prefix() {
        let (_dir, ex) = store_with(3);
        let (s, v) = get(&ex, &format!("/api/doc/{}", hex(3)));
        assert_eq!(s, 200);
        assert_eq!(v["id"], hex(3));
        assert_eq!(v["source"], "gut");
        assert_eq!(v["features"]["words"], 3);
        assert_eq!(v["features"]["entropy"], 90.0);
        assert_eq!(v["features"]["dictHit"], Value::Null);
        assert_eq!(v["extra"]["year"], 1803);
        assert_eq!(v["text"], "doc 3 has words");
        assert!(v.get("chars").is_none(), "len and bytes live in features");
        let (s, v) = get(&ex, "/api/doc/0303");
        assert_eq!((s, v["id"].as_str().unwrap()), (200, hex(3).as_str()));
        let (s, v) = get(&ex, &format!("/api/doc/{}", hex(2)));
        assert_eq!((s, v["extra"].clone(), v["source"].clone()), (200, Value::Null, json!("cli")));
        let (s, v) = get(&ex, &format!("/api/doc/{}", hex(9)));
        assert_eq!(s, 404);
        assert!(v["error"].as_str().unwrap().contains("no such document"));
        let (s, _) = get(&ex, "/api/doc/ffff");
        assert_eq!(s, 404);
        let (s, v) = get(&ex, "/api/doc/0101");
        assert_eq!(s, 400);
        assert!(v["error"].as_str().unwrap().contains("2 documents start with it"), "{v}");
        let (s, _) = get(&ex, "/api/doc/zzzz");
        assert_eq!(s, 400);
        let (s, _) = get(&ex, "/api/doc/%30%33%30%33");
        assert_eq!(s, 200, "the id segment is percent-decoded");
    }

    #[test]
    fn random_with_and_without_a_filter() {
        let (_dir, ex) = store_with(6);
        let (s, v) = get(&ex, "/api/random");
        assert_eq!(s, 200);
        assert!(v["looked_at"].as_u64().unwrap() >= 1);
        assert!(v["secs"].is_number());
        assert_eq!(v["id"].as_str().unwrap().len(), 32);
        let (s, v) = get(&ex, "/api/random?expr=words%20%3D%3D%204");
        assert_eq!((s, v["id"].as_str().unwrap()), (200, hex(4).as_str()));
        let (s, v) = get(&ex, "/api/random?expr=text.contains(%22doc%202%22)");
        assert_eq!((s, v["id"].as_str().unwrap()), (200, hex(2).as_str()));
        let (s, v) = get(&ex, "/api/random?expr=extra.year%20%3E%201804");
        assert_eq!((s, v["id"].as_str().unwrap()), (200, hex(5).as_str()));
        let (s, v) = get(&ex, "/api/random?expr=words%20%3E%201000&tries=3");
        assert_eq!(s, 404);
        assert_eq!(v["error"], "no match in 3 documents from a random start; raise tries or loosen the expression");
        let (s, v) = get(&ex, "/api/random?expr=words%20%3E%201000&tries=0");
        assert_eq!(s, 404, "tries is clamped to at least 1");
        assert!(v["error"].as_str().unwrap().contains("in 1 documents"), "{v}");
        let (s, _) = get(&ex, "/api/random?tries=abc");
        assert_eq!(s, 400);
        let (s, v) = get(&ex, "/api/random?expr=nosuch%20%3E%201");
        assert_eq!(s, 400);
        assert!(v["error"].as_str().unwrap().starts_with("expression error"));
        let empty = Explorer::new(Box::new(NullStore::default()), Sources::default(), "/dev/null".into(), 0);
        let (s, v) = get(&empty, "/api/random");
        assert_eq!((s, v["error"].as_str().unwrap()), (404, "the store is empty"));
    }

    #[test]
    fn next_prev_and_the_edges() {
        let (_dir, ex) = store_with(4);
        let (s, v) = get(&ex, "/api/next?n=1");
        assert_eq!((s, v["id"].as_str().unwrap(), v["stepped"].as_u64().unwrap()), (200, hex(1).as_str(), 1));
        let (s, v) = get(&ex, &format!("/api/next/{}", hex(1)));
        assert_eq!((s, v["id"].as_str().unwrap()), (200, key_to_hex(&amb()).as_str()));
        let (s, v) = get(&ex, "/api/next/0202?n=2");
        assert_eq!((s, v["id"].as_str().unwrap(), v["stepped"].as_u64().unwrap()), (200, hex(4).as_str(), 2));
        let (s, v) = get(&ex, "/api/next?n=100");
        assert_eq!(
            (s, v["id"].as_str().unwrap(), v["stepped"].as_u64().unwrap()),
            (200, hex(4).as_str(), 5),
            "a long step stops at the last document"
        );
        let (s, v) = get(&ex, &format!("/api/next/{}", hex(4)));
        assert_eq!((s, v["error"].as_str().unwrap()), (404, format!("no document after {}", hex(4)).as_str()));
        let (s, v) = get(&ex, "/api/prev");
        assert_eq!((s, v["id"].as_str().unwrap()), (200, hex(4).as_str()));
        let (s, v) = get(&ex, &format!("/api/prev/{}?n=1", hex(3)));
        assert_eq!((s, v["id"].as_str().unwrap()), (200, hex(2).as_str()));
        let (s, v) = get(&ex, &format!("/api/prev/{}?n=2", hex(3)));
        assert_eq!((s, v["id"].as_str().unwrap()), (200, key_to_hex(&amb()).as_str()));
        let (s, v) = get(&ex, &format!("/api/prev/{}/", hex(1)));
        assert_eq!(s, 400, "a trailing slash is not an id: {v}");
        let (s, v) = get(&ex, &format!("/api/prev/{}", hex(1)));
        assert_eq!((s, v["error"].as_str().unwrap()), (404, format!("no document before {}", hex(1)).as_str()));
        let (s, v) = get(&ex, "/api/next?n=0");
        assert_eq!((s, v["error"].as_str().unwrap()), (400, "n must be at least 1"));
        let (s, _) = get(&ex, "/api/next?n=x");
        assert_eq!(s, 400);
        let (s, _) = get(&ex, "/api/next/zzzz");
        assert_eq!(s, 400);
        let (s, _) = get(&ex, "/api/next/ffff");
        assert_eq!(s, 404);
        let empty = Explorer::new(Box::new(NullStore::default()), Sources::default(), "/dev/null".into(), 1);
        let (s, v) = get(&empty, "/api/next/");
        assert_eq!((s, v["error"].as_str().unwrap()), (404, "the store is empty"));
        let (s, _) = get(&empty, "/api/prev");
        assert_eq!(s, 404);
    }

    #[test]
    fn past_the_last_possible_key() {
        let dir = tempfile::tempdir().unwrap();
        let store = store::open(&dir.path().join("db"), true).unwrap();
        store
            .put_batch(&[Record { key: [0xff; 16], meta: Meta::default().encode(), text: b"end".to_vec(), extra: None, replace: false }])
            .unwrap();
        let ex = Explorer::new(store, Sources::default(), "/x".into(), 1);
        let last = key_to_hex(&[0xff; 16]);
        let (s, v) = get(&ex, &format!("/api/next/{last}"));
        assert_eq!(s, 404, "{v}");
        let (s, v) = get(&ex, &format!("/api/query?from={last}"));
        assert_eq!((s, v["scanned"].as_u64().unwrap(), v["done"].as_bool().unwrap()), (200, 0, true));
        assert_eq!(v["last"], Value::Null);
        let (s, v) = get(&ex, &format!("/api/doc/{last}"));
        assert_eq!((s, v["source"].as_str().unwrap()), (200, "?"), "an unknown source id is shown as ?");
    }

    #[test]
    fn query_pages_through_matches_both_ways() {
        let (_dir, ex) = store_with(6);
        let a = key_to_hex(&amb());
        let (s, v) = get(&ex, "/api/query?limit=2");
        assert_eq!(s, 200);
        assert_eq!(ids(&v), vec![hex(1), a.clone()]);
        assert_eq!((v["scanned"].as_u64().unwrap(), v["matched"].as_u64().unwrap(), v["done"].as_bool().unwrap()), (2, 2, false));
        assert_eq!(v["last"], a);
        assert_eq!(v["dir"], "next");
        assert_eq!(v["expr"], Value::Null);
        assert_eq!(v["rows"][0]["head"], "doc 1 has words");
        assert_eq!(v["rows"][0]["source"], "gut");
        assert_eq!(v["rows"][0]["features"]["words"], 1);
        let (_, v) = get(&ex, &format!("/api/query?limit=2&from={a}&head=3"));
        assert_eq!(ids(&v), vec![hex(2), hex(3)]);
        assert_eq!(v["rows"][1]["head"], "doc");
        let (_, v) = get(&ex, &format!("/api/query?dir=prev&from={}&head=0", hex(3)));
        assert_eq!(ids(&v), vec![hex(2), a.clone(), hex(1)]);
        assert_eq!((v["done"].as_bool().unwrap(), v["rows"][0]["head"].clone()), (true, Value::Null));
        assert_eq!(v["last"], hex(1));
        let (_, v) = get(&ex, "/api/query?dir=prev&limit=1");
        assert_eq!(ids(&v), vec![hex(6)]);
        let (_, v) = get(&ex, "/api/query?expr=words%20%3E%3D%205");
        assert_eq!(ids(&v), vec![a.clone(), hex(5), hex(6)], "key order: the ambiguous key sorts right after [1; 16]");
        assert_eq!((v["scanned"].as_u64().unwrap(), v["done"].as_bool().unwrap()), (7, true));
        assert_eq!(v["expr"], "words >= 5");
        let (_, v) = get(&ex, "/api/query?scan=1&expr=words%20%3E%3D%205");
        assert_eq!((ids(&v).len(), v["scanned"].as_u64().unwrap(), v["done"].as_bool().unwrap()), (0, 1, false));
        assert_eq!(v["last"], hex(1));
        let (_, v) = get(&ex, "/api/query?limit=0");
        assert_eq!(ids(&v).len(), 1, "limit is clamped to at least 1");
        let (_, v) = get(&ex, &format!("/api/query?from={}", hex(6)));
        assert_eq!((ids(&v).len(), v["done"].as_bool().unwrap(), v["last"].clone()), (0, true, Value::Null));
        for bad in
            ["/api/query?from=0101", "/api/query?dir=sideways", "/api/query?expr=words%20%3E", "/api/query?limit=x", "/api/query?head=-1"]
        {
            let (s, _) = get(&ex, bad);
            assert_eq!(s, 400, "{bad}");
        }
        let empty = Explorer::new(Box::new(NullStore::default()), Sources::default(), "/dev/null".into(), 1);
        let (s, v) = get(&empty, "/api/query");
        assert_eq!((s, ids(&v).len(), v["done"].as_bool().unwrap()), (200, 0, true));
    }

    #[test]
    fn count_is_a_full_scan() {
        let (_dir, ex) = store_with(5);
        let (s, v) = get(&ex, "/api/count");
        assert_eq!(s, 200);
        assert_eq!((v["scanned"].as_u64().unwrap(), v["matched"].as_u64().unwrap()), (6, 6));
        assert_eq!(v["pct"], 100.0);
        assert_eq!(v["expr"], Value::Null);
        let (_, v) = get(&ex, "/api/count?expr=source%20%3D%3D%20%22gut%22");
        assert_eq!((v["scanned"].as_u64().unwrap(), v["matched"].as_u64().unwrap()), (6, 3));
        assert_eq!(v["pct"], 50.0);
        assert_eq!(v["expr"], "source == \"gut\"");
        let (s, _) = get(&ex, "/api/count?expr=(");
        assert_eq!(s, 400);
        let empty = Explorer::new(Box::new(NullStore::default()), Sources::default(), "/dev/null".into(), 1);
        let (_, v) = get(&empty, "/api/count");
        assert_eq!((v["scanned"].as_u64().unwrap(), v["pct"].as_f64().unwrap()), (0, 0.0));
    }

    #[test]
    fn damaged_records_are_server_errors() {
        let dir = tempfile::tempdir().unwrap();
        let store = store::open(&dir.path().join("db"), true).unwrap();
        store.put_raw(Keyspace::Meta, &[7; 16], Some(b"\xff")).unwrap();
        store.put_raw(Keyspace::Text, &[7; 16], Some(b"x")).unwrap();
        store.put_raw(Keyspace::Meta, &[8; 16], Some(&Meta::default().encode())).unwrap();
        store.put_raw(Keyspace::Meta, &[9; 16], Some(&Meta::default().encode())).unwrap();
        store.put_raw(Keyspace::Text, &[9; 16], Some(b"ok")).unwrap();
        store.put_raw(Keyspace::Extra, &[9; 16], Some(b"not json")).unwrap();
        let ex = Explorer::new(store, Sources::default(), "/x".into(), 1);
        let (s, v) = get(&ex, &format!("/api/doc/{}", hex(7)));
        assert_eq!(s, 500);
        assert!(v["error"].as_str().unwrap().starts_with("corrupt meta record"), "{v}");
        let (s, _) = get(&ex, "/api/random");
        assert_eq!(s, 500, "the scan hits the corrupt record");
        let (s, _) = get(&ex, "/api/query");
        assert_eq!(s, 500);
        let (s, _) = get(&ex, "/api/count");
        assert_eq!(s, 500);
        let (s, v) = get(&ex, &format!("/api/doc/{}", hex(8)));
        assert_eq!(s, 500);
        assert!(v["error"].as_str().unwrap().contains("text missing"), "{v}");
        let (s, v) = get(&ex, &format!("/api/query?from={}&limit=1", hex(7)));
        assert_eq!(s, 500);
        assert!(v["error"].as_str().unwrap().contains("text missing"), "{v}");
        let (s, v) = get(&ex, &format!("/api/doc/{}", hex(9)));
        assert_eq!(s, 500);
        assert!(v["error"].as_str().unwrap().contains("stored extra is not JSON"), "{v}");
        let (s, _) = get(&ex, &format!("/api/next/{}", hex(8)));
        assert_eq!(s, 500, "navigation lands on the document with the bad extra");
    }

    #[test]
    fn bind_and_banner() {
        let (_dir, ex) = store_with(2);
        let server = bind(ex, "127.0.0.1", 0).unwrap();
        let addr = server.addr();
        assert_ne!(addr.port(), 0);
        let b = banner(&server).unwrap();
        assert!(b.starts_with(&format!("Explore: http://{addr}/\n  Store: /x (about ")), "{b}");
        assert!(b.contains(" MB, 2 sources)\n  Read-only; press Ctrl-C to stop.\n"), "{b}");
        assert_eq!(server.explorer().dir(), "/x");
        let (_dir2, ex2) = store_with(1);
        let e = bind(ex2, "999.999.999.999", 0).err().expect("no listener on a bad address").to_string();
        assert!(e.contains("cannot listen on 999.999.999.999:0"), "{e}");
    }
}
