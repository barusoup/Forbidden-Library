//! Ephemeral, upload-only LAN endpoint. No listener exists outside the QR dialog.
use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post, put},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use std::{
    net::{IpAddr, SocketAddr, TcpListener},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use subtle::ConstantTimeEq;
use tokio::io::AsyncWriteExt;

const FILE_LIMIT: u64 = 2 * 1024 * 1024 * 1024;
const TOTAL_LIMIT: u64 = 10 * 1024 * 1024 * 1024;
const FILE_COUNT: usize = 500;
const LIFETIME: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Debug)]
pub enum Status {
    Waiting,
    Receiving,
    Ready(Vec<PathBuf>),
    Closed(String),
}

pub struct Session {
    #[cfg(test)]
    url: String,
    pub address: String,
    pub fingerprint: String,
    pub qr: qrcode::QrCode,
    pub staging: Arc<tempfile::TempDir>,
    #[cfg(test)]
    certificate: rustls::pki_types::CertificateDer<'static>,
    status: Arc<Mutex<Status>>,
    stop: tokio::sync::watch::Sender<bool>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteImportSession")
            .finish_non_exhaustive()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        if let Some(thread) = self.thread.get_mut().unwrap().take() {
            let _ = thread.join();
        }
    }
}

#[derive(Clone)]
struct UploadState {
    token: Arc<String>,
    origin: String,
    staging: Arc<tempfile::TempDir>,
    status: Arc<Mutex<Status>>,
    // One request at a time, with no queue of unbounded upload tasks.
    gate: Arc<tokio::sync::Mutex<Batch>>,
    wake: Arc<dyn Fn() + Send + Sync>,
    finished: tokio::sync::watch::Sender<bool>,
}

#[derive(Default)]
struct Batch {
    paths: Vec<PathBuf>,
    bytes: u64,
    finished: bool,
}

fn private_v4(ip: IpAddr) -> bool {
    matches!(ip, IpAddr::V4(ip) if ip.is_private() || ip.is_link_local())
}

/// Prefer real LAN hardware even when a VPN owns the default route. Inspecting
/// adapter metadata does not change routes, firewall rules or VPN settings.
#[cfg(windows)]
fn physical_lan_interfaces() -> Vec<String> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        FreeMibTable, GetIfTable2, MIB_IF_TABLE2,
    };
    struct Table(*mut MIB_IF_TABLE2);
    impl Drop for Table {
        fn drop(&mut self) {
            unsafe {
                FreeMibTable(self.0.cast());
            }
        }
    }
    let mut raw = std::ptr::null_mut();
    // SAFETY: Windows allocates a table on success; Table owns it until the
    // aliases have been copied and frees it with the corresponding API.
    unsafe {
        if GetIfTable2(&mut raw) != 0 || raw.is_null() {
            return Vec::new();
        }
        let table = Table(raw);
        let rows =
            std::slice::from_raw_parts((*table.0).Table.as_ptr(), (*table.0).NumEntries as usize);
        rows.iter()
            .filter(|row| {
                // HardwareInterface is the first bit in InterfaceAndOperStatusFlags.
                // IANA types 6 and 71 are Ethernet and IEEE 802.11; OperStatus 1 is Up.
                row.InterfaceAndOperStatusFlags._bitfield & 1 != 0
                    && row.OperStatus == 1
                    && matches!(row.Type, 6 | 71)
            })
            .map(|row| {
                let end = row
                    .Alias
                    .iter()
                    .position(|c| *c == 0)
                    .unwrap_or(row.Alias.len());
                String::from_utf16_lossy(&row.Alias[..end])
            })
            .collect()
    }
}

#[cfg(not(windows))]
fn physical_lan_interfaces() -> Vec<String> {
    Vec::new()
}

fn select_lan_address(
    default: Option<IpAddr>,
    mut interfaces: Vec<(String, IpAddr)>,
    physical: &[String],
) -> Option<IpAddr> {
    interfaces.retain(|(_, ip)| private_v4(*ip));
    interfaces.sort_by_key(|(name, ip)| {
        let lower = name.to_ascii_lowercase();
        let virtual_adapter = [
            "vpn",
            "wireguard",
            "wintun",
            "tailscale",
            "zerotier",
            "tap",
            "tun",
            "virtual",
            "veth",
            "vethernet",
            "docker",
            "wsl",
        ]
        .iter()
        .any(|term| lower.contains(term));
        let named_lan = [
            "wi-fi", "wifi", "wlan", "ethernet", "eth", "en0", "en1", "enp", "eno", "wlp",
        ]
        .iter()
        .any(|term| lower.starts_with(term));
        let priority = if physical.contains(name) {
            0
        } else if virtual_adapter {
            3
        } else if named_lan {
            1
        } else {
            2
        };
        let link_local = matches!(ip, IpAddr::V4(ip) if ip.is_link_local());
        (link_local, priority, Some(*ip) != default)
    });
    interfaces
        .first()
        .map(|(_, ip)| *ip)
        .or(default.filter(|ip| private_v4(*ip)))
}

impl Session {
    #[cfg(test)]
    pub(crate) fn test_session() -> Arc<Self> {
        Self::bind("127.0.0.1".parse().unwrap(), LIFETIME, Arc::new(|| {})).unwrap()
    }
    #[cfg(test)]
    pub(crate) fn test_complete(&self) {
        let path = self.staging.path().join("received.jpg");
        std::fs::write(&path, b"test transfer").unwrap();
        *self.status.lock().unwrap() = Status::Ready(vec![path]);
    }
    pub fn start(wake: impl Fn() + Send + Sync + 'static) -> Result<Arc<Self>, String> {
        let ip = select_lan_address(
            local_ip_address::local_ip().ok(),
            local_ip_address::list_afinet_netifas().unwrap_or_default(),
            &physical_lan_interfaces(),
        )
        .ok_or("No private IPv4 LAN address found. Connect to Wi-Fi or Ethernet first.")?;
        Self::bind(ip, LIFETIME, Arc::new(wake))
    }

    pub(crate) fn bind(
        ip: IpAddr,
        lifetime: Duration,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Arc<Self>, String> {
        let listener = TcpListener::bind(SocketAddr::new(ip, 0)).map_err(|e| e.to_string())?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let address = listener.local_addr().map_err(|e| e.to_string())?;
        let mut secret = [0u8; 32];
        getrandom::fill(&mut secret).map_err(|e| e.to_string())?;
        let token = Arc::new(URL_SAFE_NO_PAD.encode(secret));
        let cert =
            rcgen::generate_simple_self_signed(vec![ip.to_string()]).map_err(|e| e.to_string())?;
        let fingerprint = Sha256::digest(cert.cert.der())
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(":");
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.cert.der().clone()],
                rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der())
                    .into(),
            )
            .map_err(|e| e.to_string())?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let origin = format!("https://{address}");
        let url = format!("{origin}/#{token}");
        let qr = qrcode::QrCode::with_error_correction_level(&url, qrcode::EcLevel::M)
            .map_err(|e| e.to_string())?;
        let staging = Arc::new(
            tempfile::Builder::new()
                .prefix("forbidden-library-upload-")
                .tempdir()
                .map_err(|e| e.to_string())?,
        );
        let status = Arc::new(Mutex::new(Status::Waiting));
        let (stop, mut stopped) = tokio::sync::watch::channel(false);
        let (finished, mut done) = tokio::sync::watch::channel(false);
        let state = UploadState {
            token,
            origin,
            staging: staging.clone(),
            status: status.clone(),
            gate: Arc::default(),
            wake: wake.clone(),
            finished,
        };
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let thread_status = status.clone();
        let cleanup_status = status.clone();
        let cleanup = staging.clone();
        let thread = std::thread::Builder::new().name("lan-upload".into()).spawn(move || {
            runtime.block_on(async move {
                let app = router(state);
                let handle = axum_server::Handle::new();
                let tls = axum_server::tls_rustls::RustlsAcceptor::new(
                    axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(config)))
                    .handshake_timeout(Duration::from_secs(5))
                    .acceptor(ConnectionLimit(Arc::new(tokio::sync::Semaphore::new(16))));
                let mut server = match axum_server::from_tcp(listener) {
                    Ok(server) => server.acceptor(tls).handle(handle.clone()),
                    Err(error) => {
                        *thread_status.lock().unwrap() = Status::Closed(error.to_string());
                        wake();
                        return;
                    }
                };
                server.http_builder().http1().max_headers(32).max_buf_size(16 * 1024)
                    .timer(hyper_util::rt::TokioTimer::new()).header_read_timeout(Duration::from_secs(10));
                let server = server.serve(app.into_make_service());
                tokio::pin!(server);
                let reason = tokio::select! {
                    result = &mut server => Some(format!("Upload server stopped: {}", result.err().map(|e| e.to_string()).unwrap_or_default())),
                    _ = stopped.changed() => Some("Transfer cancelled".into()),
                    _ = done.changed() => None,
                    _ = tokio::time::sleep(lifetime) => Some("QR code expired. Open another-device import to try again.".into()),
                };
                // Hard shutdown also terminates partial bodies and TLS connections.
                handle.shutdown();
                if let Some(reason) = reason {
                    let mut status = thread_status.lock().unwrap();
                    // A finished batch may already be owned by the import worker.
                    if !matches!(*status, Status::Ready(_)) {
                        *status = Status::Closed(reason);
                    }
                }
                wake();
            });
            // Abort in-flight handlers before removing files, including on Windows.
            drop(runtime);
            if matches!(*cleanup_status.lock().unwrap(), Status::Closed(_)) {
                let _ = std::fs::remove_dir_all(cleanup.path());
            }
        }).map_err(|e| e.to_string())?;
        Ok(Arc::new(Self {
            address: address.to_string(),
            fingerprint,
            qr,
            staging,
            status,
            stop,
            thread: Mutex::new(Some(thread)),
            #[cfg(test)]
            url,
            #[cfg(test)]
            certificate: cert.cert.der().clone(),
        }))
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }
}

// The permit lives with the socket, so even idle connections and incomplete TLS
// handshakes count toward the limit. Reject rather than queue excess connections.
#[derive(Clone)]
struct ConnectionLimit(Arc<tokio::sync::Semaphore>);

struct LimitedStream {
    inner: tokio::net::TcpStream,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl<S> axum_server::accept::Accept<tokio::net::TcpStream, S> for ConnectionLimit {
    type Stream = LimitedStream;
    type Service = S;
    type Future = std::future::Ready<std::io::Result<(LimitedStream, S)>>;
    fn accept(&self, stream: tokio::net::TcpStream, service: S) -> Self::Future {
        if !stream
            .peer_addr()
            .is_ok_and(|peer| private_v4(peer.ip()) || peer.ip().is_loopback())
        {
            return std::future::ready(Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "LAN clients only",
            )));
        }
        let result = self
            .0
            .clone()
            .try_acquire_owned()
            .map(|permit| {
                (
                    LimitedStream {
                        inner: stream,
                        _permit: permit,
                    },
                    service,
                )
            })
            .map_err(|_| std::io::Error::other("Too many upload connections"));
        std::future::ready(result)
    }
}

impl tokio::io::AsyncRead for LimitedStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}
impl tokio::io::AsyncWrite for LimitedStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

fn router(state: UploadState) -> Router {
    Router::new()
        .route(
            "/",
            get(|| async { Html(include_str!("remote_import.html")) }),
        )
        .route("/upload", put(upload))
        .route("/finish", post(finish))
        .layer(middleware::from_fn_with_state(state.clone(), security))
        .with_state(state)
}

async fn security(State(state): State<UploadState>, request: Request, next: Next) -> Response {
    let headers = request.headers();
    // Exact Host and Origin checks block DNS rebinding and browser cross-origin writes.
    if headers.get("host").and_then(|v| v.to_str().ok()) != state.origin.strip_prefix("https://") {
        return StatusCode::FORBIDDEN.into_response();
    }
    if let Some(origin) = headers.get("origin")
        && origin.to_str().ok() != Some(&state.origin)
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    if request.uri().path() != "/" {
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let supplied = auth.strip_prefix("Bearer ").unwrap_or("");
        if !bool::from(supplied.as_bytes().ct_eq(state.token.as_bytes())) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }
    // Requests cannot outlive the session. Body reads also have an idle timeout.
    let mut response = match tokio::time::timeout(LIFETIME, next.run(request)).await {
        Ok(response) => response,
        Err(_) => StatusCode::REQUEST_TIMEOUT.into_response(),
    };
    for (name, value) in [
        ("cache-control", "no-store"),
        ("referrer-policy", "no-referrer"),
        ("x-content-type-options", "nosniff"),
        ("x-frame-options", "DENY"),
        (
            "content-security-policy",
            "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
        ),
    ] {
        response
            .headers_mut()
            .insert(name, HeaderValue::from_static(value));
    }
    response
}

/// Makes a phone's file name safe to create on Windows. Android storage allows
/// characters Windows forbids, and music titles use them often (`?`, `:`), so
/// those become underscores instead of failing the upload. Separators are
/// replaced too, so the name can never leave its own staging directory.
fn safe_name(encoded: &str) -> Option<String> {
    let decoded = percent_encoding::percent_decode_str(encoded)
        .decode_utf8()
        .ok()?;
    let name: String = decoded
        .chars()
        .map(|c| {
            if c.is_control() || "<>:\"/\\|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    // A leading dot hides the file or names a parent; Windows drops trailing dots and spaces.
    let name = name
        .trim_start_matches(['.', ' '])
        .trim_end_matches(['.', ' ']);
    if name.is_empty() {
        return None;
    }
    // Keep the extension whole and cut the title to fit the length limit.
    let (stem, ext) = match name.rfind('.') {
        Some(dot) if dot > 0 && name.len() - dot <= 16 => name.split_at(dot),
        _ => (name, ""),
    };
    let mut stem = truncate_bytes(stem, 180 - ext.len()).to_string();
    let upper = stem.split('.').next().unwrap_or("").to_ascii_uppercase();
    if [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ]
    .contains(&upper.as_str())
    {
        stem.insert(0, '_');
    }
    Some(format!("{stem}{ext}"))
}

/// The longest prefix of `text` that fits in `max` bytes, on a character boundary.
fn truncate_bytes(text: &str, max: usize) -> &str {
    let mut end = 0;
    for (index, c) in text.char_indices() {
        if index + c.len_utf8() > max {
            break;
        }
        end = index + c.len_utf8();
    }
    &text[..end]
}

async fn upload(State(state): State<UploadState>, request: Request) -> StatusCode {
    let Ok(mut batch) = state.gate.try_lock() else {
        return StatusCode::CONFLICT;
    };
    if batch.finished || batch.paths.len() >= FILE_COUNT {
        return StatusCode::CONFLICT;
    }
    let Some(name) = request
        .headers()
        .get("x-file-name")
        .and_then(|v| v.to_str().ok())
        .and_then(safe_name)
    else {
        return StatusCode::BAD_REQUEST;
    };
    // One private directory per file keeps original names without collisions.
    let Ok(dir) = tempfile::Builder::new()
        .prefix("file-")
        .tempdir_in(state.staging.path())
    else {
        return StatusCode::INSUFFICIENT_STORAGE;
    };
    let path = dir.path().join(name);
    let Ok(mut file) = tokio::fs::File::create(&path).await else {
        return StatusCode::INSUFFICIENT_STORAGE;
    };
    *state.status.lock().unwrap() = Status::Receiving;
    (state.wake)();
    let result = receive(request.into_body(), &mut file, TOTAL_LIMIT - batch.bytes).await;
    // Close before deleting on Windows. The TempDir removes partial writes on any error.
    drop(file);
    match result {
        Ok(bytes) => {
            batch.bytes += bytes;
            batch.paths.push(path);
            let _ = dir.keep();
            StatusCode::CREATED
        }
        Err(code) => code,
    }
}

async fn receive(
    mut body: Body,
    file: &mut tokio::fs::File,
    remaining: u64,
) -> Result<u64, StatusCode> {
    use http_body_util::BodyExt;
    let mut bytes = 0u64;
    while let Some(frame) = tokio::time::timeout(Duration::from_secs(30), body.frame())
        .await
        .map_err(|_| StatusCode::REQUEST_TIMEOUT)?
    {
        let frame = frame.map_err(|_| StatusCode::BAD_REQUEST)?;
        if let Ok(chunk) = frame.into_data() {
            bytes += chunk.len() as u64;
            if bytes > FILE_LIMIT || bytes > remaining {
                return Err(StatusCode::PAYLOAD_TOO_LARGE);
            }
            file.write_all(&chunk)
                .await
                .map_err(|_| StatusCode::INSUFFICIENT_STORAGE)?;
        }
    }
    file.flush()
        .await
        .map_err(|_| StatusCode::INSUFFICIENT_STORAGE)?;
    if bytes == 0 {
        return Err(StatusCode::BAD_REQUEST);
    }
    Ok(bytes)
}

async fn finish(State(state): State<UploadState>) -> StatusCode {
    let Ok(mut batch) = state.gate.try_lock() else {
        return StatusCode::CONFLICT;
    };
    if batch.finished || batch.paths.is_empty() {
        return StatusCode::BAD_REQUEST;
    }
    batch.finished = true;
    *state.status.lock().unwrap() = Status::Ready(batch.paths.clone());
    (state.wake)();
    // Let the response leave before closing all connections.
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let _ = state.finished.send(true);
    });
    StatusCode::OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn session(lifetime: Duration) -> Arc<Session> {
        Session::bind("127.0.0.1".parse().unwrap(), lifetime, Arc::new(|| {})).unwrap()
    }

    fn client(
        session: &Session,
    ) -> rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream> {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(session.certificate.clone()).unwrap();
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        let connection = rustls::ClientConnection::new(
            Arc::new(config),
            rustls::pki_types::ServerName::IpAddress("127.0.0.1".parse::<IpAddr>().unwrap().into()),
        )
        .unwrap();
        let socket = std::net::TcpStream::connect(&session.address).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        rustls::StreamOwned::new(connection, socket)
    }

    fn request(session: &Session, method: &str, path: &str, headers: &str, body: &[u8]) -> String {
        let mut stream = client(session);
        write!(stream, "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n",session.address,body.len()).unwrap();
        stream.write_all(body).unwrap();
        stream.flush().unwrap();
        let mut bytes = Vec::new();
        // Servers may close TCP without TLS close_notify after an HTTP response.
        let _ = stream.read_to_end(&mut bytes);
        String::from_utf8(bytes).unwrap()
    }

    fn auth(session: &Session) -> String {
        format!(
            "Authorization: Bearer {}\r\n",
            session.url.split('#').nth(1).unwrap()
        )
    }

    #[test]
    fn tls_upload_is_authenticated_isolated_and_preserves_original_names() {
        let session = session(LIFETIME);
        let html = request(&session, "GET", "/", "", b"");
        assert!(html.starts_with("HTTP/1.1 200"), "{html}");
        assert!(html.contains("cache-control: no-store"));
        assert!(!html.contains(session.url.split('#').nth(1).unwrap()));
        assert!(
            request(
                &session,
                "PUT",
                "/upload",
                "X-File-Name: photo.jpg\r\n",
                b"data"
            )
            .starts_with("HTTP/1.1 401")
        );
        let auth = auth(&session);
        assert!(
            request(
                &session,
                "PUT",
                "/upload",
                &format!("{auth}Origin: https://evil.example\r\nX-File-Name: photo.jpg\r\n"),
                b"data"
            )
            .starts_with("HTTP/1.1 403")
        );
        assert!(
            request(
                &session,
                "PUT",
                "/upload",
                &format!("{auth}X-File-Name: %2E%2E\r\n"),
                b"data"
            )
            .starts_with("HTTP/1.1 400")
        );
        for data in [b"first".as_slice(), b"second".as_slice()] {
            assert!(
                request(
                    &session,
                    "PUT",
                    "/upload",
                    &format!("{auth}X-File-Name: photo.jpg\r\n"),
                    data
                )
                .starts_with("HTTP/1.1 201")
            );
        }
        assert!(request(&session, "GET", "/photo.jpg", &auth, b"").starts_with("HTTP/1.1 404"));
        assert!(request(&session, "POST", "/finish", &auth, b"").starts_with("HTTP/1.1 200"));
        let Status::Ready(paths) = session.status() else {
            panic!("batch not ready");
        };
        assert_eq!(paths.len(), 2);
        assert_ne!(paths[0], paths[1]);
        assert_eq!(paths[0].file_name().unwrap(), "photo.jpg");
        assert_eq!(std::fs::read(&paths[0]).unwrap(), b"first");
        let guard = session.staging.clone();
        let address = session.address.clone();
        drop(session); // Immediate import/cancel must close the port, even before /finish's delay.
        assert!(std::net::TcpStream::connect(&address).is_err());
        assert!(
            paths[0].exists(),
            "the import worker's guard must keep received files alive"
        );
        drop(guard);
        assert!(!paths[0].exists());
    }

    #[test]
    fn finishing_shuts_down_without_dismissing_dialog_and_new_sessions_reject_old_secrets() {
        let first = session(LIFETIME);
        let first_auth = auth(&first);
        assert!(
            request(
                &first,
                "PUT",
                "/upload",
                &format!("{first_auth}X-File-Name: clip.mp4\r\n"),
                b"clip"
            )
            .starts_with("HTTP/1.1 201")
        );
        assert!(request(&first, "POST", "/finish", &first_auth, b"").starts_with("HTTP/1.1 200"));
        first.thread.lock().unwrap().take().unwrap().join().unwrap();
        assert!(matches!(first.status(), Status::Ready(_)));
        assert!(std::net::TcpStream::connect(&first.address).is_err());
        let second = session(LIFETIME);
        assert_ne!(first.fingerprint, second.fingerprint);
        assert!(request(&second, "POST", "/finish", &first_auth, b"").starts_with("HTTP/1.1 401"));
    }

    #[test]
    fn cancellation_closes_live_connections_and_removes_partial_uploads() {
        let session = session(LIFETIME);
        let address = session.address.clone();
        let staging = session.staging.path().to_path_buf();
        let mut stream = client(&session);
        write!(stream,"PUT /upload HTTP/1.1\r\nHost: {address}\r\n{}X-File-Name: video.mp4\r\nContent-Length: 1000000\r\n\r\npartial",auth(&session)).unwrap();
        stream.flush().unwrap();
        drop(session);
        assert!(!staging.exists());
        assert!(std::net::TcpStream::connect(&address).is_err());
        let mut bytes = Vec::new();
        assert!(stream.read_to_end(&mut bytes).is_err() || bytes.is_empty());
    }

    #[test]
    fn expiry_closes_listener_and_cleans_staging_without_closing_dialog() {
        let (tx, rx) = std::sync::mpsc::channel();
        let session = Session::bind(
            "127.0.0.1".parse().unwrap(),
            Duration::from_millis(60),
            Arc::new(move || {
                let _ = tx.send(());
            }),
        )
        .unwrap();
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // Join guarantees runtime and cleanup have both completed.
        session
            .thread
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .join()
            .unwrap();
        assert!(matches!(session.status(), Status::Closed(_)));
        assert!(!session.staging.path().exists());
        assert!(std::net::TcpStream::connect(&session.address).is_err());
    }

    #[test]
    fn lan_selection_prefers_hardware_over_the_vpn_default_route() {
        let vpn: IpAddr = "10.8.0.2".parse().unwrap();
        let wifi: IpAddr = "192.168.1.7".parse().unwrap();
        let ethernet: IpAddr = "192.168.1.8".parse().unwrap();
        let interfaces = vec![
            ("VPN".into(), vpn),
            ("Wi-Fi".into(), wifi),
            ("Ethernet".into(), ethernet),
        ];
        assert_eq!(
            select_lan_address(Some(vpn), interfaces.clone(), &["Wi-Fi".into()]),
            Some(wifi)
        );
        assert_eq!(
            select_lan_address(
                Some(ethernet),
                interfaces.clone(),
                &["Wi-Fi".into(), "Ethernet".into()]
            ),
            Some(ethernet)
        );
        assert_eq!(select_lan_address(Some(vpn), interfaces, &[]), Some(wifi));
        assert_eq!(
            select_lan_address(
                None,
                vec![("Ethernet".into(), "8.8.8.8".parse().unwrap())],
                &[]
            ),
            None
        );
        // A customized hardware alias should still beat an adapter named Wi-Fi.
        assert_eq!(
            select_lan_address(
                Some(vpn),
                vec![("Office".into(), ethernet), ("Wi-Fi VPN".into(), vpn)],
                &["Office".into()]
            ),
            Some(ethernet)
        );
    }

    #[test]
    fn unsafe_names_are_made_safe_and_nameless_ones_rejected() {
        for name in ["", ".", "..", "%FF"] {
            assert!(safe_name(name).is_none(), "accepted {name}");
        }
        for (name, safe) in [
            ("..%2Ffile", "_file"),
            ("%5Cfile", "_file"),
            ("a%3Ab.jpg", "a_b.jpg"),
            ("Who%3F%20Why.mp3", "Who_ Why.mp3"),
            ("NUL.jpg", "_NUL.jpg"),
            ("COM1", "_COM1"),
            ("file.", "file"),
            ("file%20", "file"),
            ("%00", "_"),
        ] {
            assert_eq!(safe_name(name).as_deref(), Some(safe), "{name}");
        }
        let long = format!("{}.flac", "é".repeat(120));
        let kept = safe_name(
            &percent_encoding::utf8_percent_encode(&long, percent_encoding::NON_ALPHANUMERIC)
                .to_string(),
        )
        .unwrap();
        assert!(kept.len() <= 180 && kept.ends_with(".flac"), "{kept}");
        assert_eq!(
            safe_name("family%20photo.jpg").as_deref(),
            Some("family photo.jpg")
        );
        assert!(!private_v4("8.8.8.8".parse().unwrap()));
        assert!(!private_v4("127.0.0.1".parse().unwrap()));
        assert!(private_v4("192.168.1.2".parse().unwrap()));
    }

    #[tokio::test]
    async fn body_limit_is_enforced_while_streaming() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("file");
        let mut file = tokio::fs::File::create(path).await.unwrap();
        assert_eq!(
            receive(Body::from("12345"), &mut file, 4).await,
            Err(StatusCode::PAYLOAD_TOO_LARGE)
        );
        assert_eq!(
            receive(Body::empty(), &mut file, 4).await,
            Err(StatusCode::BAD_REQUEST)
        );
        assert_eq!(receive(Body::from("1234"), &mut file, 4).await, Ok(4));
    }
}
