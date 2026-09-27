//! Upload side of gh-img. nginx serves the stored files publicly; this
//! process only accepts uploads and deletes on the Tailscale address.
//!
//! `gh-img-server` serves. `gh-img-server sweep` deletes expired images and
//! is run daily by a systemd timer. `gh-img-server adopt` adds stored images
//! that are missing from the index, with the default expiry from their mtime.

use std::collections::HashMap;
use std::convert::Infallible;
use std::io::{self, Write};
use std::net::{IpAddr, SocketAddr};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gh_img::index::{self, Index};
use gh_img::{ProcessError, new_name, parse_ttl, process, sanitise_alt, valid_name};
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::header::{
    ALLOW, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HeaderValue, WWW_AUTHENTICATE,
};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, Semaphore};

const MAX_BYTES: u64 = 10 * 1024 * 1024;
const MAX_CONNECTIONS: usize = 16;
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
const BODY_TIMEOUT: Duration = Duration::from_secs(30);
const FAILED_AUTH_DELAY: Duration = Duration::from_secs(1);
const STALE_TMP_AGE: Duration = Duration::from_secs(3600);
const MIN_TOKEN_LEN: usize = 32;

struct Config {
    img_dir: PathBuf,
    index_path: PathBuf,
    bind: SocketAddr,
    public_base: String,
    token_digest: Vec<u8>,
    // the disk may be shared with other services, so uploads stop well before it fills
    min_free_bytes: u64,
    store_cap_bytes: u64,
    default_ttl_secs: u64,
}

struct App {
    cfg: Config,
    // one decode at a time keeps peak memory to a single image on a 1 GB host
    decode: Semaphore,
    // serialises index read-modify-write between uploads and deletes
    index: Mutex<()>,
}

type Resp = Response<Full<Bytes>>;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

fn env_u64(key: &str, default: u64) -> Result<u64, String> {
    match std::env::var(key) {
        Ok(v) => v
            .parse()
            .map_err(|_| format!("{key} must be a whole number")),
        Err(_) => Ok(default),
    }
}

fn load_config() -> Result<Config, String> {
    // systemd LoadCredential puts the token in $CREDENTIALS_DIRECTORY/token
    let token_path = match std::env::var_os("CREDENTIALS_DIRECTORY") {
        Some(dir) => PathBuf::from(dir).join("token"),
        None => std::env::var_os("TOKEN_FILE")
            .map(PathBuf::from)
            .ok_or("no token: set CREDENTIALS_DIRECTORY or TOKEN_FILE")?,
    };
    let token = std::fs::read_to_string(&token_path)
        .map_err(|e| format!("reading {}: {e}", token_path.display()))?;
    let token = token.trim();
    if token.len() < MIN_TOKEN_LEN {
        return Err(format!("token is shorter than {MIN_TOKEN_LEN} characters"));
    }

    let host: IpAddr = env_or("BIND_HOST", "127.0.0.1")
        .parse()
        .map_err(|_| "BIND_HOST is not an IP address")?;
    let port: u16 = env_or("PORT", "8787")
        .parse()
        .map_err(|_| "PORT is not a port number")?;
    let ttl = env_or("GH_IMG_DEFAULT_TTL", "90d");

    Ok(Config {
        img_dir: env_or("IMG_DIR", "/srv/gh-img").into(),
        index_path: env_or("INDEX_PATH", "/var/lib/gh-img/index.json").into(),
        bind: SocketAddr::new(host, port),
        public_base: std::env::var("PUBLIC_BASE")
            .map_err(|_| "PUBLIC_BASE is not set")?
            .trim_end_matches('/')
            .to_owned(),
        token_digest: Sha256::digest(token.as_bytes()).to_vec(),
        min_free_bytes: env_u64("GH_IMG_MIN_FREE_BYTES", 2 * 1024 * 1024 * 1024)?,
        store_cap_bytes: env_u64("GH_IMG_STORE_CAP_BYTES", 5 * 1024 * 1024 * 1024)?,
        default_ttl_secs: parse_ttl(&ttl)
            .ok_or(format!("GH_IMG_DEFAULT_TTL {ttl:?} is not like 90d"))?,
    })
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn json_resp(status: StatusCode, body: Value) -> Resp {
    let mut r = Response::new(Full::new(Bytes::from(body.to_string())));
    *r.status_mut() = status;
    r.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    r
}

fn error(status: StatusCode, msg: impl Into<String>) -> Resp {
    json_resp(status, json!({ "error": msg.into() }))
}

fn method_not_allowed(allow: &'static str) -> Resp {
    let mut r = error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
    r.headers_mut()
        .insert(ALLOW, HeaderValue::from_static(allow));
    r
}

/// Compares SHA-256 digests, so the comparison is constant-time and does
/// not reveal the token's length.
fn authorised(app: &App, req: &Request<Incoming>) -> bool {
    let Some(value) = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let Some((scheme, token)) = value.split_once(' ') else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("bearer") {
        return false;
    }

    let given = Sha256::digest(token.trim().as_bytes());
    given.as_slice().ct_eq(&app.cfg.token_digest).into()
}

async fn handle(app: Arc<App>, req: Request<Incoming>, peer: IpAddr) -> Resp {
    let path = req.uri().path().to_owned();
    let method = req.method().clone();

    if path == "/health" && (method == Method::GET || method == Method::HEAD) {
        return json_resp(StatusCode::OK, json!({ "ok": true }));
    }

    if !authorised(&app, &req) {
        eprintln!("rejected token from {peer}");
        tokio::time::sleep(FAILED_AUTH_DELAY).await;
        let mut r = error(StatusCode::UNAUTHORIZED, "unauthorised");
        r.headers_mut()
            .insert(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        return r;
    }

    if path == "/upload" {
        return if method == Method::POST {
            upload(&app, req, peer).await
        } else {
            method_not_allowed("POST")
        };
    }

    let name = path.strip_prefix('/').unwrap_or(&path);
    if valid_name(name) {
        return if method == Method::DELETE {
            delete(&app, name, peer).await
        } else {
            method_not_allowed("DELETE")
        };
    }

    error(StatusCode::NOT_FOUND, "not found")
}

async fn upload(app: &App, req: Request<Incoming>, peer: IpAddr) -> Resp {
    let too_large = || {
        error(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("limit is {MAX_BYTES} bytes"),
        )
    };

    let declared = req
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
    if declared.is_some_and(|n| n > MAX_BYTES) {
        return too_large();
    }

    let params: HashMap<String, String> =
        form_urlencoded::parse(req.uri().query().unwrap_or("").as_bytes())
            .into_owned()
            .collect();
    let alt = sanitise_alt(params.get("alt").map(String::as_str).unwrap_or(""));
    let expires = if params.get("keep").is_some_and(|v| v == "1" || v == "true") {
        None
    } else if let Some(raw) = params.get("ttl") {
        match parse_ttl(raw) {
            Some(secs) => Some(now() + secs),
            None => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "ttl must look like 90d, 12h or 30m",
                );
            }
        }
    } else {
        Some(now() + app.cfg.default_ttl_secs)
    };

    let body = Limited::new(req.into_body(), MAX_BYTES as usize);
    let raw = match tokio::time::timeout(BODY_TIMEOUT, body.collect()).await {
        Err(_) => {
            return error(
                StatusCode::REQUEST_TIMEOUT,
                "body not received within 30 seconds",
            );
        }
        Ok(Err(e)) if e.downcast_ref::<LengthLimitError>().is_some() => return too_large(),
        Ok(Err(_)) => return error(StatusCode::BAD_REQUEST, "could not read the request body"),
        Ok(Ok(collected)) => collected.to_bytes(),
    };
    if raw.is_empty() {
        return error(StatusCode::BAD_REQUEST, "empty body");
    }

    let processed = {
        let _permit = app.decode.acquire().await.expect("decode semaphore closed");
        tokio::task::spawn_blocking(move || process(&raw)).await
    };
    let (bytes, kind) = match processed {
        Ok(Ok(v)) => v,
        Ok(Err(ProcessError::Unsupported(m))) => {
            return error(StatusCode::UNSUPPORTED_MEDIA_TYPE, m);
        }
        Ok(Err(ProcessError::Invalid(m))) => {
            return error(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("could not process image: {m}"),
            );
        }
        Err(e) => {
            eprintln!("image processing panicked: {e}");
            return error(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
        }
    };

    match has_room(&app.cfg, bytes.len() as u64) {
        Ok(None) => {}
        Ok(Some(msg)) => return error(StatusCode::INSUFFICIENT_STORAGE, msg),
        Err(e) => {
            eprintln!("checking disk space: {e}");
            return error(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
        }
    }

    let name = new_name(kind);
    let _guard = app.index.lock().await;
    if let Err(e) = store(&app.cfg, &name, &bytes, expires) {
        eprintln!("storing {name}: {e}");
        return error(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
    }

    eprintln!("stored {name} {}B from {peer}", bytes.len());
    let url = format!("{}/{name}", app.cfg.public_base);
    json_resp(
        StatusCode::CREATED,
        json!({ "url": url, "markdown": format!("![{alt}]({url})"), "bytes": bytes.len(), "expires": expires }),
    )
}

/// Returns a reason to refuse the upload, or `None` when there is room.
fn has_room(cfg: &Config, incoming: u64) -> io::Result<Option<String>> {
    let st = rustix::fs::statvfs(&cfg.img_dir)?;
    if st.f_bavail.saturating_mul(st.f_frsize) < cfg.min_free_bytes.saturating_add(incoming) {
        return Ok(Some("server disk is nearly full".into()));
    }

    let mut used = 0u64;
    for entry in std::fs::read_dir(&cfg.img_dir)? {
        used += entry?.metadata()?.len();
    }
    if used + incoming > cfg.store_cap_bytes {
        return Ok(Some("image store is full".into()));
    }

    Ok(None)
}

/// Write atomically: a dot-prefixed temp file (outside nginx's name pattern),
/// fsync, rename without overwriting, then record the expiry. A failure at
/// any step leaves no file behind.
fn store(cfg: &Config, name: &str, bytes: &[u8], expires: Option<u64>) -> io::Result<()> {
    let path = cfg.img_dir.join(name);
    let mut tmp = tempfile::Builder::new()
        .prefix(".")
        .suffix(".tmp")
        .tempfile_in(&cfg.img_dir)?;
    tmp.write_all(bytes)?;
    // group-readable so nginx (www-data, the directory's group) can serve it
    tmp.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o640))?;
    tmp.as_file().sync_all()?;
    tmp.persist_noclobber(&path).map_err(|e| e.error)?;
    std::fs::File::open(&cfg.img_dir)?.sync_all()?;

    let mut idx = Index::load(&cfg.index_path)?;
    idx.images.insert(name.to_owned(), expires);
    if let Err(e) = idx.save(&cfg.index_path) {
        let _ = std::fs::remove_file(&path);
        return Err(e);
    }

    Ok(())
}

async fn delete(app: &App, name: &str, peer: IpAddr) -> Resp {
    let _guard = app.index.lock().await;

    match std::fs::remove_file(app.cfg.img_dir.join(name)) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return error(StatusCode::NOT_FOUND, "not found");
        }
        Err(e) => {
            eprintln!("deleting {name}: {e}");
            return error(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
        }
    }

    let result = Index::load(&app.cfg.index_path).and_then(|mut idx| {
        idx.images.remove(name);
        idx.save(&app.cfg.index_path)
    });
    if let Err(e) = result {
        eprintln!("updating index after deleting {name}: {e}");
    }

    eprintln!("deleted {name} from {peer}");
    json_resp(
        StatusCode::OK,
        json!({ "deleted": name, "note": "Cloudflare and GitHub may serve cached copies for up to a day" }),
    )
}

fn remove_stale_tmp(dir: &Path) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with('.') && name.ends_with(".tmp")) {
            continue;
        }

        let age = entry.metadata()?.modified()?.elapsed().unwrap_or_default();
        if age > STALE_TMP_AGE {
            std::fs::remove_file(entry.path())?;
            eprintln!("removed stale {name}");
        }
    }

    Ok(())
}

fn listen(addr: SocketAddr) -> io::Result<TcpListener> {
    use socket2::{Domain, Socket, Type};

    let sock = Socket::new(Domain::for_address(addr), Type::STREAM, None)?;
    sock.set_reuse_address(true)?;
    // the Tailscale address may not exist yet at boot; freebind lets the bind succeed anyway
    #[cfg(target_os = "linux")]
    if addr.is_ipv4() {
        sock.set_freebind_v4(true)?;
    } else {
        sock.set_freebind_v6(true)?;
    }
    sock.bind(&addr.into())?;
    sock.listen(64)?;
    sock.set_nonblocking(true)?;

    TcpListener::from_std(sock.into())
}

async fn serve(cfg: Config) -> io::Result<()> {
    remove_stale_tmp(&cfg.img_dir)?;
    if let Some(dir) = cfg.index_path.parent() {
        remove_stale_tmp(dir)?;
    }

    let listener = listen(cfg.bind)?;
    eprintln!(
        "gh-img listening on {}, storing in {}",
        listener.local_addr()?,
        cfg.img_dir.display()
    );

    let app = Arc::new(App {
        cfg,
        decode: Semaphore::new(1),
        index: Mutex::new(()),
    });
    let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));

    loop {
        let permit = connections
            .clone()
            .acquire_owned()
            .await
            .expect("connection semaphore closed");
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("accept: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };

        let app = app.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let svc = service_fn(move |req| {
                let app = app.clone();
                async move { Ok::<_, Infallible>(handle(app, req, peer.ip()).await) }
            });
            let conn = http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(HEADER_TIMEOUT)
                .keep_alive(false)
                .serve_connection(TokioIo::new(stream), svc);
            if let Err(e) = conn.await {
                eprintln!("connection from {peer}: {e}");
            }
        });
    }
}

fn main() -> ExitCode {
    let result = match std::env::args().nth(1).as_deref() {
        None => {
            let cfg = match load_config() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("gh-img: {e}");
                    return ExitCode::FAILURE;
                }
            };
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .and_then(|rt| rt.block_on(serve(cfg)))
        }
        // sweep needs only the paths, not the token
        Some("sweep") => index::sweep(
            Path::new(&env_or("IMG_DIR", "/srv/gh-img")),
            Path::new(&env_or("INDEX_PATH", "/var/lib/gh-img/index.json")),
            now(),
        )
        .map(|deleted| {
            for name in &deleted {
                eprintln!("expired {name}");
            }
            eprintln!("sweep removed {} images", deleted.len());
        }),
        Some("adopt") => {
            let ttl = env_or("GH_IMG_DEFAULT_TTL", "90d");
            let Some(ttl_secs) = parse_ttl(&ttl) else {
                eprintln!("gh-img: GH_IMG_DEFAULT_TTL {ttl:?} is not like 90d");
                return ExitCode::FAILURE;
            };
            index::adopt(
                Path::new(&env_or("IMG_DIR", "/srv/gh-img")),
                Path::new(&env_or("INDEX_PATH", "/var/lib/gh-img/index.json")),
                ttl_secs,
            )
            .map(|added| eprintln!("adopted {} images into the index", added.len()))
        }
        Some(other) => {
            eprintln!(
                "gh-img: unknown command {other:?}; use no argument to serve, `sweep` or `adopt`"
            );
            return ExitCode::from(2);
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("gh-img: {e}");
            ExitCode::FAILURE
        }
    }
}
