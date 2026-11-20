//! A small in-process HTTP/1.1 server standing in for an origin: blobs at `/blobs/<sha>`, the
//! list at `/blobs/known`, uploads by PUT, and Cloudflare's API and R2's S3 paths for the R2
//! origin. One thread per connection, `Connection: close` on every answer.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct State {
    pub blobs: HashMap<String, Vec<u8>>,
    /// sha -> how many more GETs answer 503.
    pub fail: HashMap<String, u32>,
    /// sha -> how many more GETs answer with a flipped byte.
    pub corrupt: HashMap<String, u32>,
    /// sha -> how many more GETs end half way (Content-Length says all, the body is half).
    pub truncate: HashMap<String, u32>,
    /// Required as `Authorization: Bearer <token>` on /blobs when set.
    pub token: Option<String>,
    /// Every request: (method, path).
    pub log: Vec<(String, String)>,
    /// Range headers seen.
    pub ranges: Vec<String>,
    /// The R2 listing's page size.
    pub page: usize,
}

pub struct Server {
    pub base: String,
    pub state: Arc<Mutex<State>>,
}

struct Req {
    method: String,
    path: String,
    query: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn read_req(stream: &mut TcpStream) -> Option<Req> {
    let mut r = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    r.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let (path, query) = target
        .split_once('?')
        .map_or((target.clone(), String::new()), |(p, q)| {
            (p.to_string(), q.to_string())
        });
    let mut headers = HashMap::new();
    loop {
        let mut h = String::new();
        r.read_line(&mut h).ok()?;
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let len: usize = headers
        .get("content-length")
        .and_then(|l| l.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0; len];
    r.read_exact(&mut body).ok()?;
    Some(Req {
        method,
        path,
        query,
        headers,
        body,
    })
}

fn answer(stream: &mut TcpStream, status: u16, headers: &[(&str, String)], body: &[u8]) {
    let reason = match status {
        200 => "OK",
        206 => "Partial Content",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Status",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
}

fn take(m: &mut HashMap<String, u32>, k: &str) -> bool {
    match m.get_mut(k) {
        Some(n) if *n > 0 => {
            *n -= 1;
            true
        }
        _ => false,
    }
}

fn param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v)
}

fn serve_blob(stream: &mut TcpStream, st: &mut State, sha: &str, req: &Req) {
    let Some(bytes) = st.blobs.get(sha).cloned() else {
        return answer(stream, 404, &[], b"no such blob");
    };
    if take(&mut st.fail, sha) {
        return answer(stream, 503, &[], b"try later");
    }
    let mut bytes = bytes;
    if take(&mut st.corrupt, sha) && !bytes.is_empty() {
        bytes[0] ^= 0xff;
    }
    if take(&mut st.truncate, sha) {
        // Promise all of it, send half, hang up.
        let head = format!(
            "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
            bytes.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(&bytes[..bytes.len() / 2]);
        return;
    }
    if let Some(range) = req.headers.get("range") {
        st.ranges.push(range.clone());
        if let Some(from) = range
            .strip_prefix("bytes=")
            .and_then(|r| r.strip_suffix('-'))
            .and_then(|f| f.parse::<usize>().ok())
            && from < bytes.len()
        {
            let total = bytes.len();
            return answer(
                stream,
                206,
                &[(
                    "Content-Range",
                    format!("bytes {from}-{}/{total}", total - 1),
                )],
                &bytes[from..],
            );
        }
    }
    answer(stream, 200, &[], &bytes)
}

fn handle(mut stream: TcpStream, state: Arc<Mutex<State>>) {
    let Some(req) = read_req(&mut stream) else {
        return;
    };
    let mut st = state.lock().unwrap();
    st.log.push((req.method.clone(), req.path.clone()));
    let json = |v: serde_json::Value| v.to_string().into_bytes();

    // The HTTP origin.
    if let Some(rest) = req.path.strip_prefix("/blobs/") {
        if let Some(t) = &st.token
            && req.headers.get("authorization") != Some(&format!("Bearer {t}"))
        {
            return answer(&mut stream, 401, &[], b"who are you");
        }
        return match (req.method.as_str(), rest) {
            ("GET", "known") => {
                let mut list: Vec<_> = st.blobs.keys().cloned().collect();
                list.sort();
                answer(&mut stream, 200, &[], (list.join("\n") + "\n").as_bytes())
            }
            ("GET", sha) => serve_blob(&mut stream, &mut st, sha, &req),
            ("PUT", sha) => {
                st.blobs.insert(sha.to_string(), req.body.clone());
                answer(&mut stream, 200, &[], b"")
            }
            _ => answer(&mut stream, 404, &[], b""),
        };
    }

    // Cloudflare's API.
    if let Some(rest) = req.path.strip_prefix("/client/v4/") {
        let ok = req.headers.get("authorization") == Some(&"Bearer cf-token".to_string());
        if !ok {
            return answer(
                &mut stream,
                401,
                &[],
                &json(serde_json::json!({"success": false})),
            );
        }
        if rest.ends_with("/tokens/verify") {
            return answer(
                &mut stream,
                200,
                &[],
                &json(serde_json::json!({"result": {"id": "parent-key"}})),
            );
        }
        if rest.ends_with("/r2/temp-access-credentials") {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            assert_eq!(body["parentAccessKeyId"], "parent-key");
            return answer(
                &mut stream,
                200,
                &[],
                &json(serde_json::json!({"result": {
                    "accessKeyId": "AK", "secretAccessKey": "SK", "sessionToken": "ST"
                }})),
            );
        }
        if rest.contains("/r2/buckets/") && rest.ends_with("/objects") {
            let prefix = param(&req.query, "prefix")
                .unwrap_or("")
                .replace("%2F", "/");
            let mut keys: Vec<_> = st.blobs.keys().cloned().collect();
            keys.sort();
            let from: usize = param(&req.query, "cursor")
                .and_then(|c| c.parse().ok())
                .unwrap_or(0);
            let page = st.page.max(1);
            let slice: Vec<_> = keys.iter().skip(from).take(page).collect();
            let result: Vec<_> = slice
                .iter()
                .map(|k| serde_json::json!({"key": format!("{prefix}{k}"), "size": st.blobs[*k].len()}))
                .collect();
            let cursor = if from + page < keys.len() {
                (from + page).to_string()
            } else {
                String::new()
            };
            return answer(
                &mut stream,
                200,
                &[],
                &json(serde_json::json!({"result": result, "result_info": {"cursor": cursor}})),
            );
        }
        return answer(&mut stream, 404, &[], b"");
    }

    // R2's S3 endpoint, path style: /<bucket>/blob/<sha>, presigned.
    if let Some(rest) = req.path.strip_prefix("/bucket/blob/") {
        if !req.query.contains("X-Amz-Signature") {
            return answer(&mut stream, 401, &[], b"unsigned");
        }
        return match req.method.as_str() {
            "GET" => serve_blob(&mut stream, &mut st, rest, &req),
            "PUT" => {
                st.blobs.insert(rest.to_string(), req.body.clone());
                answer(&mut stream, 200, &[], b"")
            }
            _ => answer(&mut stream, 404, &[], b""),
        };
    }
    answer(&mut stream, 404, &[], b"")
}

impl Server {
    pub fn start() -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(State {
            page: 1000,
            ..State::default()
        }));
        let s = state.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let s = s.clone();
                std::thread::spawn(move || handle(stream, s));
            }
        });
        Server { base, state }
    }

    pub fn add(&self, bytes: &[u8]) -> String {
        let sha = assetsync::store::hash_bytes(bytes);
        self.state
            .lock()
            .unwrap()
            .blobs
            .insert(sha.clone(), bytes.to_vec());
        sha
    }

    pub fn gets(&self, sha: &str) -> usize {
        self.state
            .lock()
            .unwrap()
            .log
            .iter()
            .filter(|(m, p)| m == "GET" && p.ends_with(sha))
            .count()
    }
}
