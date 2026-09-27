//! Starts the real binary on 127.0.0.1 and talks raw HTTP/1.1 to it, so
//! chunked bodies and missing headers can be sent exactly.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const TOKEN: &str = "test-token-0123456789abcdefghijklmnopqrstuvwxyz";
const MAX: usize = 10 * 1024 * 1024;

struct Server {
    child: Child,
    addr: String,
    dir: tempfile::TempDir,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    fn start(extra_env: &[(&str, &str)]) -> Server {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("img")).unwrap();
        std::fs::write(dir.path().join("token"), format!("{TOKEN}\n")).unwrap();

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_gh-img-server"));
        cmd.env_clear()
            .env("IMG_DIR", dir.path().join("img"))
            .env("INDEX_PATH", dir.path().join("index.json"))
            .env("TOKEN_FILE", dir.path().join("token"))
            .env("BIND_HOST", "127.0.0.1")
            .env("PORT", "0")
            .env("PUBLIC_BASE", "https://img.example.test/")
            .envs(extra_env.iter().copied())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();

        let (tx, rx) = mpsc::channel();
        let stderr = child.stderr.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if let Some(rest) = line.strip_prefix("gh-img listening on ") {
                    let _ = tx.send(rest.split(',').next().unwrap().to_owned());
                }
            }
        });
        let addr = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("server did not start");

        Server { child, addr, dir }
    }

    fn img_dir(&self) -> PathBuf {
        self.dir.path().join("img")
    }

    fn stored(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.img_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// Sends `head` (request line and headers, without the blank line) and
    /// `body`, and returns (status, lower-cased headers, body).
    fn raw(&self, head: &str, body: &[u8]) -> (u16, String, String) {
        let mut s = TcpStream::connect(&self.addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        s.write_all(format!("{head}\r\nHost: test\r\nConnection: close\r\n\r\n").as_bytes())
            .unwrap();
        // the server may answer and close before reading a rejected body
        let _ = s.write_all(body);

        let mut buf = Vec::new();
        let _ = s.read_to_end(&mut buf);
        let text = String::from_utf8_lossy(&buf).into_owned();
        let (headers, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        let status = headers
            .split(' ')
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        (status, headers.to_ascii_lowercase(), body.to_owned())
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: &[u8],
    ) -> (u16, String, serde_json::Value) {
        let mut head = format!("{method} {path} HTTP/1.1\r\nContent-Length: {}", body.len());
        if let Some(t) = token {
            head.push_str(&format!("\r\nAuthorization: Bearer {t}"));
        }
        let (status, headers, body) = self.raw(&head, body);
        (
            status,
            headers,
            serde_json::from_str(&body).unwrap_or(serde_json::Value::Null),
        )
    }

    fn upload(&self, query: &str, body: &[u8]) -> (u16, serde_json::Value) {
        let (status, _, json) = self.request("POST", &format!("/upload{query}"), Some(TOKEN), body);
        (status, json)
    }
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
    )
    .unwrap()
}

fn name_of(url: &str) -> &str {
    url.rsplit('/').next().unwrap()
}

fn valid_png_of_size(size: usize) -> Vec<u8> {
    // a valid PNG followed by padding inside a private ancillary chunk, which
    // decoders skip, so the file is exactly `size` bytes and still decodes
    let png = fixture("in.png");
    let iend = png.windows(4).position(|w| w == b"IEND").unwrap() - 4;
    let head = &png[..iend];
    let pad = size - head.len() - 12 - 12;
    let mut out = head.to_vec();
    out.extend((pad as u32).to_be_bytes());
    out.extend(b"zzPd");
    out.extend(std::iter::repeat_n(0u8, pad));
    out.extend([0u8; 4]); // decoders do not check CRCs of unknown ancillary chunks
    out.extend([0, 0, 0, 0]);
    out.extend(b"IEND");
    out.extend([0xae, 0x42, 0x60, 0x82]);
    assert_eq!(out.len(), size);
    out
}

#[test]
fn health_needs_no_token_and_answers_head() {
    let s = Server::start(&[]);
    let (status, _, body) = s.request("GET", "/health", None, b"");
    assert_eq!((status, body), (200, serde_json::json!({ "ok": true })));

    let (status, _, body) = s.raw("HEAD /health HTTP/1.1", b"");
    assert_eq!(status, 200);
    assert_eq!(body, "");
}

#[test]
fn bad_tokens_are_rejected() {
    let s = Server::start(&[]);
    let same_length = "x".repeat(TOKEN.len());
    let started = Instant::now();

    for token in [None, Some("wrong"), Some(same_length.as_str())] {
        let (status, headers, body) = s.request("POST", "/upload", token, &fixture("in.png"));
        assert_eq!(status, 401, "{token:?}");
        assert!(headers.contains("www-authenticate: bearer"));
        assert_eq!(body["error"], "unauthorised");
    }

    assert!(
        started.elapsed() >= Duration::from_secs(3),
        "failed auth was not delayed"
    );
    assert!(s.stored().is_empty());
}

#[test]
fn bearer_scheme_is_case_insensitive() {
    let s = Server::start(&[]);
    let body = fixture("in.png");
    let head = format!(
        "POST /upload HTTP/1.1\r\nContent-Length: {}\r\nAuthorization: bEaReR {TOKEN}",
        body.len()
    );
    assert_eq!(s.raw(&head, &body).0, 201);
}

#[test]
fn upload_stores_a_clean_file_and_reports_it() {
    let s = Server::start(&[]);

    for fixture_name in ["in.png", "in.jpg", "in.gif", "in.webp", "anim.gif"] {
        let (status, json) = s.upload("?alt=a]b", &fixture(fixture_name));
        assert_eq!(status, 201, "{fixture_name}: {json}");

        let url = json["url"].as_str().unwrap();
        let name = name_of(url);
        assert!(gh_img::valid_name(name), "{name}");
        assert_eq!(url, format!("https://img.example.test/{name}"));
        assert_eq!(json["markdown"], format!("![a b]({url})"));

        let path = s.img_dir().join(name);
        let stored = std::fs::read(&path).unwrap();
        assert_eq!(json["bytes"], stored.len());
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert!(!stored.windows(6).any(|w| w == b"MARKER"), "{fixture_name}");
    }

    assert!(s.stored().iter().all(|n| !n.ends_with(".tmp")));
}

#[test]
fn alt_defaults_and_expiry_options() {
    let s = Server::start(&[]);
    let png = fixture("in.png");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let (_, json) = s.upload("", &png);
    assert!(json["markdown"].as_str().unwrap().starts_with("![image]("));
    let default_expiry = json["expires"].as_u64().unwrap();
    assert!((now + 90 * 86_400..now + 90 * 86_400 + 60).contains(&default_expiry));

    let (_, json) = s.upload("?ttl=2h", &png);
    let expiry = json["expires"].as_u64().unwrap();
    assert!((now + 7_200..now + 7_260).contains(&expiry));

    let (_, kept) = s.upload("?keep=1", &png);
    assert!(kept["expires"].is_null());

    let index: serde_json::Value =
        serde_json::from_slice(&std::fs::read(s.dir.path().join("index.json")).unwrap()).unwrap();
    assert!(index["images"][name_of(kept["url"].as_str().unwrap())].is_null());
    assert_eq!(index["images"].as_object().unwrap().len(), 3);

    let (status, json) = s.upload("?ttl=forever", &png);
    assert_eq!(
        (status, json["error"].as_str().unwrap()),
        (400, "ttl must look like 90d, 12h or 30m")
    );
}

#[test]
fn rejected_uploads_store_nothing() {
    let s = Server::start(&[]);
    let png = fixture("in.png");

    let cases: [(&str, Vec<u8>, u16); 4] = [
        ("empty", Vec::new(), 400),
        (
            "svg",
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>".to_vec(),
            415,
        ),
        ("animated webp", fixture("anim.webp"), 415),
        ("truncated png", png[..60].to_vec(), 422),
    ];
    for (label, body, expected) in cases {
        let (status, json) = s.upload("", &body);
        assert_eq!(status, expected, "{label}: {json}");
        assert!(json["error"].is_string(), "{label}");
    }

    assert!(s.stored().is_empty(), "{:?}", s.stored());
}

#[test]
fn size_limit_applies_to_declared_and_chunked_bodies() {
    let s = Server::start(&[]);

    let (status, json) = s.upload("", &vec![0u8; MAX + 1]);
    assert_eq!(
        (status, json["error"].as_str().unwrap()),
        (413, "limit is 10485760 bytes")
    );

    // chunked, so the server cannot see the size up front
    let mut chunked = Vec::new();
    let chunk = vec![0u8; 1024 * 1024];
    for _ in 0..11 {
        chunked.extend(format!("{:x}\r\n", chunk.len()).as_bytes());
        chunked.extend(&chunk);
        chunked.extend(b"\r\n");
    }
    chunked.extend(b"0\r\n\r\n");
    let head = format!(
        "POST /upload HTTP/1.1\r\nTransfer-Encoding: chunked\r\nAuthorization: Bearer {TOKEN}"
    );
    let (status, _, body) = s.raw(&head, &chunked);
    assert_eq!(status, 413, "{body}");

    let (status, json) = s.upload("", &valid_png_of_size(MAX));
    assert_eq!(status, 201, "{json}");
}

#[test]
fn declared_oversize_is_rejected_without_waiting_for_the_body() {
    let s = Server::start(&[]);
    let mut stream = TcpStream::connect(&s.addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let head = format!(
        "POST /upload HTTP/1.1\r\nHost: test\r\nContent-Length: {}\r\nAuthorization: Bearer {TOKEN}\r\n\r\n",
        MAX + 1
    );
    stream.write_all(head.as_bytes()).unwrap();

    // no body is sent: only the Content-Length check can answer before the timeout
    let mut buf = [0u8; 12];
    stream
        .read_exact(&mut buf)
        .expect("no response within 5 seconds");
    assert_eq!(&buf, b"HTTP/1.1 413");
}

#[test]
fn disk_floor_returns_507() {
    let s = Server::start(&[("GH_IMG_MIN_FREE_BYTES", &u64::MAX.to_string())]);
    let (status, json) = s.upload("", &fixture("in.png"));
    assert_eq!(
        (status, json["error"].as_str().unwrap()),
        (507, "server disk is nearly full")
    );
    assert!(s.stored().is_empty());
}

#[test]
fn store_cap_returns_507() {
    let s = Server::start(&[("GH_IMG_STORE_CAP_BYTES", "10")]);
    let (status, json) = s.upload("", &fixture("in.png"));
    assert_eq!(
        (status, json["error"].as_str().unwrap()),
        (507, "image store is full")
    );
}

#[test]
fn delete_removes_once_and_ignores_traversal() {
    let s = Server::start(&[]);
    std::fs::write(s.dir.path().join("secret"), b"keep").unwrap();
    let (_, json) = s.upload("", &fixture("in.png"));
    let name = name_of(json["url"].as_str().unwrap()).to_owned();

    for path in [
        "/../secret",
        "/%2e%2e/secret",
        "/AAAAAAAAAAAAAAAAAAAAA.png",
        "/..%2Fsecret",
    ] {
        let (status, _, _) = s.request("DELETE", path, Some(TOKEN), b"");
        assert!(status == 404 || status == 400, "{path}: {status}");
    }
    assert!(s.dir.path().join("secret").exists());

    let (status, _, json) = s.request("DELETE", &format!("/{name}"), Some(TOKEN), b"");
    assert_eq!(
        (status, json["deleted"].as_str().unwrap()),
        (200, name.as_str())
    );
    assert!(s.stored().is_empty());

    let (status, _, _) = s.request("DELETE", &format!("/{name}"), Some(TOKEN), b"");
    assert_eq!(status, 404);

    let index = std::fs::read_to_string(s.dir.path().join("index.json")).unwrap();
    assert!(!index.contains(&name));
}

#[test]
fn routing_without_and_with_a_token() {
    let s = Server::start(&[]);

    assert_eq!(s.request("GET", "/nope", None, b"").0, 401);
    assert_eq!(s.request("GET", "/nope", Some(TOKEN), b"").0, 404);

    let (status, headers, _) = s.request("GET", "/upload", Some(TOKEN), b"");
    assert_eq!(status, 405);
    assert!(headers.contains("allow: post"));

    let (status, headers, _) = s.request("GET", "/AAAAAAAAAAAAAAAAAAAAAA.png", Some(TOKEN), b"");
    assert_eq!(status, 405);
    assert!(headers.contains("allow: delete"));
}

#[test]
fn stale_temp_files_are_removed_at_startup() {
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("img");
    std::fs::create_dir(&img).unwrap();
    let stale = img.join(".old.tmp");
    let fresh = img.join(".new.tmp");
    std::fs::write(&stale, b"x").unwrap();
    std::fs::write(&fresh, b"x").unwrap();
    let two_hours_ago = std::time::SystemTime::now() - Duration::from_secs(7200);
    std::fs::File::options()
        .write(true)
        .open(&stale)
        .unwrap()
        .set_modified(two_hours_ago)
        .unwrap();
    std::fs::write(dir.path().join("token"), TOKEN).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_gh-img-server"))
        .env_clear()
        .env("IMG_DIR", &img)
        .env("INDEX_PATH", dir.path().join("index.json"))
        .env("TOKEN_FILE", dir.path().join("token"))
        .env("PUBLIC_BASE", "https://img.example.test")
        .env("PORT", "0")
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stderr.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let _ = child.kill();
    let _ = child.wait();

    assert!(!stale.exists());
    assert!(fresh.exists());
}

#[test]
fn startup_fails_without_a_usable_token() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("short"), "x".repeat(31)).unwrap();

    for token_file in [dir.path().join("short"), dir.path().join("missing")] {
        let out = Command::new(env!("CARGO_BIN_EXE_gh-img-server"))
            .env_clear()
            .env("IMG_DIR", dir.path())
            .env("TOKEN_FILE", &token_file)
            .env("PUBLIC_BASE", "https://img.example.test")
            .env("PORT", "0")
            .output()
            .unwrap();
        assert!(!out.status.success(), "{}", token_file.display());
    }
}

#[test]
fn sweep_command_deletes_expired_images() {
    let s = Server::start(&[]);
    let (_, short) = s.upload("?ttl=1m", &fixture("in.png"));
    let (_, kept) = s.upload("?keep=1", &fixture("in.png"));

    // rewrite the short-lived entry's expiry into the past
    let idx_path = s.dir.path().join("index.json");
    let mut index: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&idx_path).unwrap()).unwrap();
    index["images"][name_of(short["url"].as_str().unwrap())] = serde_json::json!(1);
    std::fs::write(&idx_path, index.to_string()).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_gh-img-server"))
        .arg("sweep")
        .env_clear()
        .env("IMG_DIR", s.img_dir())
        .env("INDEX_PATH", &idx_path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert_eq!(
        s.stored(),
        vec![name_of(kept["url"].as_str().unwrap()).to_owned()]
    );
}
