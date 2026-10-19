//! The local OAuth callback server: one fresh listener per candidate port on the registered
//! redirect ports; the login flow races the browser callback against a manual paste. The served
//! pages keep the TS product's success/error wording.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use futures::stream::{FuturesUnordered, StreamExt};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};

const MAX_REQUEST_HEAD_BYTES: usize = 8192;
const MAX_CONCURRENT_REQUESTS: usize = 32;
const REQUEST_HEAD_TIMEOUT: Duration = Duration::from_secs(10);

/// `PI_OAUTH_CALLBACK_HOST` (default `127.0.0.1`).
fn callback_host() -> String {
    std::env::var("PI_OAUTH_CALLBACK_HOST").unwrap_or_else(|_| "127.0.0.1".to_string())
}

/// A range (not one port) so a leaked or concurrent login cannot wedge all logins with one occupied
/// socket. Distinct from the Anthropic callback port (53692).
const CALLBACK_PORT_BASE: u16 = 53_700;
const CALLBACK_PORT_COUNT: u16 = 10;
const CALLBACK_PATH: &str = "/callback";

fn redirect_uri_for(port: u16) -> String {
    format!("http://localhost:{port}{CALLBACK_PATH}")
}

pub(super) fn validate_callback_port(callback_port: Option<u16>) -> Result<()> {
    if callback_port == Some(0) {
        bail!("OAuth callbackPort must be between 1 and 65535");
    }
    Ok(())
}

fn candidate_ports(callback_port: Option<u16>) -> Result<Vec<u16>> {
    validate_callback_port(callback_port)?;
    Ok(match callback_port {
        Some(port) => vec![port],
        None => (0..CALLBACK_PORT_COUNT)
            .map(|offset| CALLBACK_PORT_BASE + offset)
            .collect(),
    })
}

/// Every redirect URI a login registers (dynamic client registration
/// offers them all).
pub fn redirect_uris(callback_port: Option<u16>) -> Result<Vec<String>> {
    Ok(candidate_ports(callback_port)?
        .into_iter()
        .map(redirect_uri_for)
        .collect())
}

/// The authorization code plus the echoed `state` from the browser
/// redirect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallbackCode {
    pub code: String,
    pub state: String,
}

/// What a login waits for: [`Some`] code from the browser redirect, or `None` when the callback
/// settled without one.
pub type CallbackResult = Option<CallbackCode>;

/// Settled once per login; the first settle wins.
#[derive(Default, Debug)]
struct CallbackShared {
    result: tokio::sync::Mutex<Option<CallbackResult>>,
    notify: tokio::sync::Notify,
}

impl CallbackShared {
    async fn settle(&self, result: CallbackResult) {
        let mut slot = self.result.lock().await;
        if slot.is_none() {
            *slot = Some(result);
            drop(slot);
            self.notify.notify_waiters();
        }
    }
}

/// All OS sockets stay here, never in a suspended task. Accept and registration share
/// one lock with shutdown, so even an accept racing Drop cannot escape ownership.
#[derive(Debug)]
struct CallbackSockets {
    listener: Option<TcpListener>,
    streams: BTreeMap<usize, TcpStream>,
    next_id: usize,
    #[cfg(test)]
    progress: tokio::sync::watch::Sender<(usize, usize)>,
}

impl CallbackSockets {
    fn close(&mut self) {
        self.listener.take();
        self.streams.clear();
    }
}

/// A request borrows its socket only for a synchronous I/O poll. Closing the owner
/// releases the socket immediately, even before the runtime polls task cancellation.
struct CallbackConnection {
    id: usize,
    sockets: Arc<Mutex<CallbackSockets>>,
}

impl Drop for CallbackConnection {
    fn drop(&mut self) {
        self.sockets.lock().unwrap().streams.remove(&self.id);
    }
}

impl AsyncRead for CallbackConnection {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let mut sockets = self.sockets.lock().unwrap();
        let Some(stream) = sockets.streams.get_mut(&self.id) else {
            return Poll::Ready(Ok(()));
        };
        #[cfg(test)]
        let before = buffer.filled().len();
        let result = Pin::new(stream).poll_read(cx, buffer);
        #[cfg(test)]
        if buffer.filled().len() > before {
            sockets.progress.send_modify(|(_, bytes)| {
                *bytes += buffer.filled().len() - before;
            });
        }
        result
    }
}

impl AsyncWrite for CallbackConnection {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let mut sockets = self.sockets.lock().unwrap();
        let Some(stream) = sockets.streams.get_mut(&self.id) else {
            return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
        };
        Pin::new(stream).poll_write(cx, buffer)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let mut sockets = self.sockets.lock().unwrap();
        let Some(stream) = sockets.streams.get_mut(&self.id) else {
            return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
        };
        Pin::new(stream).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let mut sockets = self.sockets.lock().unwrap();
        let Some(stream) = sockets.streams.get_mut(&self.id) else {
            return Poll::Ready(Ok(()));
        };
        Pin::new(stream).poll_shutdown(cx)
    }
}

/// A running callback server: its redirect URI plus the settled result.
#[derive(Debug)]
pub struct CallbackServer {
    shared: Arc<CallbackShared>,
    port: u16,
    sockets: Arc<Mutex<CallbackSockets>>,
    listener_task: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl CallbackServer {
    /// Bind a registered port exactly, or the first free DCR candidate. Never substitute
    /// an unregistered redirect port when the registered one is occupied.
    pub async fn start(label: &str, callback_port: Option<u16>) -> Result<Self> {
        Self::start_with_ports(label, callback_port, candidate_ports(callback_port)?).await
    }

    async fn start_with_ports(
        label: &str,
        callback_port: Option<u16>,
        ports: Vec<u16>,
    ) -> Result<Self> {
        let host = callback_host();
        let mut last_error: Option<String> = None;
        for port in ports {
            let listener = match TcpListener::bind((host.as_str(), port)).await {
                Ok(listener) => listener,
                Err(error) => {
                    last_error = Some(format!("port {port}: {error}"));
                    continue;
                }
            };
            let bound_port = listener.local_addr()?.port();
            return Ok(Self::from_listener(listener, bound_port));
        }
        if let Some(port) = callback_port {
            return Err(anyhow!(
                "Could not start the OAuth callback server on registered port {port}. \
                 Close other login attempts and retry. ({label}; {})",
                last_error.unwrap_or_default(),
            ));
        }
        Err(anyhow!(
            "Could not start the OAuth callback server: ports {CALLBACK_PORT_BASE}-{} are all in use. \
             Close other login attempts and retry. ({label}; {})",
            CALLBACK_PORT_BASE + CALLBACK_PORT_COUNT - 1,
            last_error.unwrap_or_else(|| "no candidate port bound".to_string()),
        ))
    }

    fn from_listener(listener: TcpListener, port: u16) -> Self {
        let shared = Arc::new(CallbackShared::default());
        let task_shared = Arc::clone(&shared);
        let sockets = Arc::new(Mutex::new(CallbackSockets {
            listener: Some(listener),
            streams: BTreeMap::new(),
            next_id: 0,
            #[cfg(test)]
            progress: tokio::sync::watch::channel((0, 0)).0,
        }));
        let task_sockets = Arc::clone(&sockets);
        let listener_task = tokio::spawn(async move {
            // Request futures are children of this task, not detached tasks. Joining
            // this task also drops every in-flight request future.
            let mut requests = FuturesUnordered::new();
            loop {
                tokio::select! {
                    accepted = futures::future::poll_fn(|cx| {
                        let mut sockets = task_sockets.lock().unwrap();
                        let Some(listener) = sockets.listener.as_ref() else {
                            return Poll::Ready(None);
                        };
                        match listener.poll_accept(cx) {
                            Poll::Ready(Ok((stream, _))) => {
                                let id = sockets.next_id;
                                sockets.next_id += 1;
                                sockets.streams.insert(id, stream);
                                #[cfg(test)]
                                sockets.progress.send_modify(|(accepted, _)| *accepted += 1);
                                Poll::Ready(Some(Ok(CallbackConnection {
                                    id,
                                    sockets: Arc::clone(&task_sockets),
                                })))
                            }
                            Poll::Ready(Err(error)) => Poll::Ready(Some(Err(error))),
                            Poll::Pending => Poll::Pending,
                        }
                    }), if requests.len() < MAX_CONCURRENT_REQUESTS => {
                        let Some(Ok(stream)) = accepted else { break };
                        requests.push(serve_callback(stream, &task_shared));
                    }
                    Some(()) = requests.next(), if !requests.is_empty() => {}
                }
            }
        });
        Self {
            shared,
            port,
            sockets,
            listener_task: tokio::sync::Mutex::new(Some(listener_task)),
        }
    }

    /// The local URL the authorization redirect must land on.
    pub fn redirect_uri(&self) -> String {
        redirect_uri_for(self.port)
    }

    /// Wait for the browser callback to settle.
    pub async fn wait_for_code(&self) -> CallbackResult {
        loop {
            let notified = self.shared.notify.notified();
            tokio::pin!(notified);
            // Register before checking the slot: notify_waiters does not retain a
            // permit when settle races the check and the first notification poll.
            notified.as_mut().enable();
            if let Some(result) = self.shared.result.lock().await.clone() {
                return result;
            }
            notified.await;
        }
    }

    /// Race a browser callback against manual input. The input future must settle the callback
    /// (through cancellation) before completing; its outcome remains available to the caller.
    pub async fn wait_for_code_or_input<F: Future + ?Sized>(
        &self,
        input: Pin<&mut F>,
    ) -> (CallbackResult, Option<F::Output>) {
        tokio::select! {
            code = self.wait_for_code() => (code, None),
            outcome = input => {
                // Input cancellation may have lost settle() to an earlier browser redirect.
                // Read that result even when fair select polls the input branch first.
                (self.wait_for_code().await, Some(outcome))
            },
        }
    }

    /// Cancel: settle the waiter with `None` so an in-flight redirect loses
    /// only because the caller chose to stop waiting.
    pub async fn cancel(&self) {
        self.sockets.lock().unwrap().close();
        self.shared.settle(None).await;
        let mut task = self.listener_task.lock().await;
        if let Some(handle) = task.as_mut() {
            handle.abort();
            // Keep the handle owned until joined, even if this cancel future is
            // itself dropped. Concurrent cancellations wait for the same cleanup.
            let _ = handle.await;
            task.take();
        }
    }
}

#[cfg(test)]
pub(super) mod test_listener {
    use super::*;

    type SocketOwner = Arc<Mutex<CallbackSockets>>;

    #[derive(Clone)]
    pub(crate) struct CallbackOwnership {
        // Keep the store alive: passing proves synchronous socket closure,
        // not task cancellation receiving a later runtime turn.
        sockets: Arc<Mutex<Option<SocketOwner>>>,
    }

    impl CallbackOwnership {
        pub(crate) fn is_listening(&self) -> bool {
            self.sockets
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|sockets| sockets.lock().unwrap().listener.is_some())
        }

        pub(crate) fn is_closed(&self) -> bool {
            self.sockets
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|sockets| {
                    let sockets = sockets.lock().unwrap();
                    sockets.listener.is_none() && sockets.streams.is_empty()
                })
        }
    }

    /// The socket stays owned until the actual login flow creates its callback server.
    pub(crate) struct ReservedListener {
        listener: std::net::TcpListener,
        port: u16,
        ownership: CallbackOwnership,
    }

    impl ReservedListener {
        pub(crate) fn port(&self) -> u16 {
            self.port
        }

        pub(crate) fn into_callback(self) -> Result<CallbackServer> {
            let listener = TcpListener::from_std(self.listener)?;
            let server = CallbackServer::from_listener(listener, self.port);
            *self.ownership.sockets.lock().unwrap() = Some(Arc::clone(&server.sockets));
            Ok(server)
        }
    }

    pub(crate) fn reserve_listener() -> (ReservedListener, CallbackOwnership) {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        reserve_existing(listener)
    }

    pub(crate) fn reserve_existing(
        listener: std::net::TcpListener,
    ) -> (ReservedListener, CallbackOwnership) {
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let ownership = CallbackOwnership {
            sockets: Arc::new(Mutex::new(None)),
        };
        (
            ReservedListener {
                listener,
                port,
                ownership: ownership.clone(),
            },
            ownership,
        )
    }
}

impl Drop for CallbackServer {
    fn drop(&mut self) {
        self.sockets.lock().unwrap().close();
        if let Some(task) = self.listener_task.get_mut().take() {
            task.abort();
        }
    }
}

/// One browser request: read it, answer it, settle the login's waiter.
async fn serve_callback(mut stream: CallbackConnection, shared: &CallbackShared) {
    let Some(request) = read_request_head(&mut stream).await else {
        let _ = write_response(
            &mut stream,
            "400 Bad Request",
            oauth_error_page("Prime Agent", "Callback route not found."),
        )
        .await;
        return;
    };
    // Upgrades are not callbacks. Reject them before parsing the query so they
    // cannot settle the login, even when they carry a valid code and state.
    if request.lines().skip(1).any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("upgrade")
                || (name.eq_ignore_ascii_case("connection")
                    && value
                        .split(',')
                        .any(|token| token.trim().eq_ignore_ascii_case("upgrade")))
        })
    }) {
        return;
    }
    let target = request
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_string();
    let path = target.split('?').next().unwrap_or_default();
    if path != CALLBACK_PATH {
        let _ = write_response(
            &mut stream,
            "404 Not Found",
            oauth_error_page("Prime Agent", "Callback route not found."),
        )
        .await;
        return;
    }
    let query = target.split_once('?').map_or("", |(_, query)| query);
    let code = query_param(query, "code");
    let state = query_param(query, "state");
    let error = query_param(query, "error");
    if let Some(error) = error {
        shared.settle(None).await;
        let _ = write_response(
            &mut stream,
            "400 Bad Request",
            oauth_error_page(
                "Prime Agent",
                &format!("Prime Agent authentication failed. Error: {error}"),
            ),
        )
        .await;
        return;
    }
    match (code, state) {
        (Some(code), Some(state)) if !code.is_empty() && !state.is_empty() => {
            shared.settle(Some(CallbackCode { code, state })).await;
            let _ = write_response(
                &mut stream,
                "200 OK",
                oauth_success_page(
                    "Prime Agent authentication completed. You can close this window.",
                ),
            )
            .await;
        }
        _ => {
            shared.settle(None).await;
            let _ = write_response(
                &mut stream,
                "400 Bad Request",
                oauth_error_page("Prime Agent", "Missing code or state parameter."),
            )
            .await;
        }
    }
}

/// Read through the complete header terminator, with one total deadline and a
/// hard byte bound. A partial request must never settle the callback.
async fn read_request_head(stream: &mut CallbackConnection) -> Option<String> {
    tokio::time::timeout(REQUEST_HEAD_TIMEOUT, async {
        let mut buffer = [0u8; MAX_REQUEST_HEAD_BYTES];
        let mut filled = 0;
        loop {
            let read = stream.read(&mut buffer[filled..]).await.ok()?;
            if read == 0 {
                return None;
            }
            filled += read;
            if let Some(end) = buffer[..filled]
                .windows(4)
                .position(|part| part == b"\r\n\r\n")
            {
                return String::from_utf8(buffer[..end + 4].to_vec()).ok();
            }
            if filled == MAX_REQUEST_HEAD_BYTES {
                return None;
            }
        }
    })
    .await
    .ok()
    .flatten()
}

async fn write_response(
    stream: &mut CallbackConnection,
    status: &str,
    body: String,
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await
}

/// One decoded query parameter (`+` as space, percent-decoded).
fn query_param(query: &str, name: &str) -> Option<String> {
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if percent_decode(&key.replace('+', " ")) == name {
            return Some(percent_decode(&value.replace('+', " ")));
        }
    }
    None
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Some(byte) = std::str::from_utf8(&bytes[index + 1..index + 3])
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
            {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn escape_html(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// The callback pages: a dark minimal page carrying the login's outcome.
fn render_page(title: &str, message: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>{}</title>
  <style>
    body {{ margin: 0; min-height: 100vh; display: flex; align-items: center;
           justify-content: center; padding: 24px; background: #000; color: #fff;
           font-family: ui-sans-serif, system-ui, sans-serif; text-align: center; }}
    main {{ width: 100%; max-width: 560px; }}
    h1 {{ margin: 0 0 10px; font-size: 28px; line-height: 1.15; font-weight: 650; }}
    p {{ margin: 0; line-height: 1.7; color: #a1a1aa; font-size: 15px; }}
  </style>
</head>
<body>
  <main>
    <h1>Authentication</h1>
    <p>{}</p>
  </main>
</body>
</html>"#,
        escape_html(title),
        escape_html(message),
    )
}

fn oauth_success_page(message: &str) -> String {
    render_page("Prime Agent authentication completed", message)
}

fn oauth_error_page(label: &str, message: &str) -> String {
    render_page(&format!("{label} authentication failed"), message)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn ephemeral_server() -> CallbackServer {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        CallbackServer::from_listener(listener, port)
    }

    async fn progress(
        events: &mut tokio::sync::watch::Receiver<(usize, usize)>,
        accepted: usize,
        bytes: usize,
    ) {
        tokio::time::timeout(
            Duration::from_secs(2),
            events.wait_for(|&(connections, read)| connections >= accepted && read >= bytes),
        )
        .await
        .expect("callback readiness event timed out")
        .unwrap();
    }

    async fn response(stream: &mut TcpStream) -> Vec<u8> {
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut response))
            .await
            .expect("callback did not close its request socket")
            .unwrap();
        response
    }

    #[tokio::test]
    async fn cancel_closes_idle_accepted_sockets_and_joins_all_requests() {
        let server = ephemeral_server().await;
        let mut events = server.sockets.lock().unwrap().progress.subscribe();
        let mut idle = TcpStream::connect(("127.0.0.1", server.port))
            .await
            .unwrap();
        progress(&mut events, 1, 0).await;
        server.cancel().await;
        assert!(server.sockets.lock().unwrap().streams.is_empty());
        assert!(server.listener_task.lock().await.is_none());
        // No listener task or request future retains the socket owner after the join.
        assert_eq!(Arc::strong_count(&server.sockets), 1);
        let rebound = std::net::TcpListener::bind(("127.0.0.1", server.port)).unwrap();
        assert!(response(&mut idle).await.is_empty());
        assert_eq!(server.wait_for_code().await, None);
        drop(rebound);
    }

    #[tokio::test]
    async fn drop_closes_idle_accepted_sockets_without_a_runtime_turn() {
        let server = ephemeral_server().await;
        let port = server.port;
        let sockets = Arc::clone(&server.sockets);
        let mut events = sockets.lock().unwrap().progress.subscribe();
        let idle = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        progress(&mut events, 1, 0).await;
        drop(server);
        // The aborted task cannot run on this current-thread runtime until we yield.
        assert!(sockets.lock().unwrap().streams.is_empty());
        assert!(sockets.lock().unwrap().listener.is_none());
        let rebound = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
        // Read the OS socket directly: Tokio's cached readiness cannot update
        // without a reactor turn, which would weaken the synchronous-Drop check.
        let mut idle = idle.into_std().unwrap();
        let mut byte = [0];
        assert!(matches!(std::io::Read::read(&mut idle, &mut byte), Ok(0)));
        drop(rebound);
    }

    #[tokio::test]
    async fn idle_connections_are_bounded_and_releasing_one_resumes_acceptance() {
        let server = ephemeral_server().await;
        let mut events = server.sockets.lock().unwrap().progress.subscribe();
        let mut idle = Vec::new();
        for _ in 0..MAX_CONCURRENT_REQUESTS {
            idle.push(
                TcpStream::connect(("127.0.0.1", server.port))
                    .await
                    .unwrap(),
            );
        }
        progress(&mut events, MAX_CONCURRENT_REQUESTS, 0).await;
        let mut queued = TcpStream::connect(("127.0.0.1", server.port))
            .await
            .unwrap();
        queued
            .write_all(b"GET /other HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        assert_eq!(
            server.sockets.lock().unwrap().streams.len(),
            MAX_CONCURRENT_REQUESTS
        );
        assert_eq!(events.borrow().0, MAX_CONCURRENT_REQUESTS);
        drop(idle.pop());
        progress(&mut events, MAX_CONCURRENT_REQUESTS + 1, 0).await;
        assert!(response(&mut queued)
            .await
            .starts_with(b"HTTP/1.1 404 Not Found"));
        server.cancel().await;
        assert!(server.sockets.lock().unwrap().streams.is_empty());
    }

    #[tokio::test]
    async fn fragmented_headers_do_not_settle_before_the_terminator() {
        let server = ephemeral_server().await;
        let mut events = server.sockets.lock().unwrap().progress.subscribe();
        let mut stream = TcpStream::connect(("127.0.0.1", server.port))
            .await
            .unwrap();
        let first = b"GET /callback?code=fragmented&state=state HTTP/1.1\r\nHost: localhost\r\n";
        stream.write_all(first).await.unwrap();
        progress(&mut events, 1, first.len()).await;
        assert!(server.shared.result.lock().await.is_none());
        stream.write_all(b"\r").await.unwrap();
        progress(&mut events, 1, first.len() + 1).await;
        assert!(server.shared.result.lock().await.is_none());
        stream.write_all(b"\n").await.unwrap();
        assert!(response(&mut stream).await.starts_with(b"HTTP/1.1 200 OK"));
        assert_eq!(
            server.wait_for_code().await,
            Some(CallbackCode {
                code: "fragmented".to_string(),
                state: "state".to_string(),
            })
        );
        server.cancel().await;
    }

    #[tokio::test]
    async fn upgrades_close_without_settling_even_with_valid_query_parameters() {
        let server = ephemeral_server().await;
        for upgrade in [
            "Connection: keep-alive, UpGrAdE\r\nUpgrade: websocket\r\n",
            "uPgRaDe: websocket\r\n",
            "Connection: Upgrade\r\n",
        ] {
            let mut stream = TcpStream::connect(("127.0.0.1", server.port))
                .await
                .unwrap();
            stream.write_all(format!(
                "GET /callback?code=ignored&state=state HTTP/1.1\r\nHost: localhost\r\n{upgrade}\r\n"
            ).as_bytes()).await.unwrap();
            assert!(response(&mut stream).await.is_empty());
            assert!(server.shared.result.lock().await.is_none());
        }
        server.cancel().await;
    }

    #[tokio::test]
    async fn oversized_request_head_is_rejected_without_settling() {
        let server = ephemeral_server().await;
        let mut stream = TcpStream::connect(("127.0.0.1", server.port))
            .await
            .unwrap();
        let prefix = b"GET /callback?code=ignored&state=state HTTP/1.1\r\nX-Padding: ";
        let mut head = prefix.to_vec();
        head.resize(MAX_REQUEST_HEAD_BYTES, b'x');
        stream.write_all(&head).await.unwrap();
        assert!(response(&mut stream)
            .await
            .starts_with(b"HTTP/1.1 400 Bad Request"));
        assert!(server.shared.result.lock().await.is_none());
        server.cancel().await;
    }

    #[tokio::test]
    async fn incomplete_head_has_one_total_deadline_not_a_per_read_deadline() {
        let server = ephemeral_server().await;
        let mut events = server.sockets.lock().unwrap().progress.subscribe();
        let mut stream = TcpStream::connect(("127.0.0.1", server.port))
            .await
            .unwrap();
        let first = b"GET /callback?code=ignored&state=state HTTP/1.1\r\n";
        stream.write_all(first).await.unwrap();
        progress(&mut events, 1, first.len()).await;
        tokio::time::pause();
        tokio::time::advance(REQUEST_HEAD_TIMEOUT / 2).await;
        stream.write_all(b"X").await.unwrap();
        // Let real socket readiness complete without Tokio auto-advancing the
        // paused clock to an unrelated timer while waiting for OS I/O.
        tokio::time::resume();
        progress(&mut events, 1, first.len() + 1).await;
        tokio::time::pause();
        tokio::time::advance(REQUEST_HEAD_TIMEOUT / 2).await;
        // The total header budget is spent. Observe real TCP I/O with a real
        // clock so paused-time auto-advance cannot outrun socket readiness.
        tokio::time::resume();
        assert!(response(&mut stream)
            .await
            .starts_with(b"HTTP/1.1 400 Bad Request"));
        assert!(
            server.sockets.lock().unwrap().streams.is_empty(),
            "the timed-out request releases its owned socket before EOF"
        );
        assert!(server.shared.result.lock().await.is_none());
        server.cancel().await;
    }

    #[tokio::test]
    async fn settlement_is_persistent_and_wakes_all_registered_waiters() {
        let server = ephemeral_server().await;
        let mut first = Box::pin(server.wait_for_code());
        let mut second = Box::pin(server.wait_for_code());
        assert!(futures::poll!(first.as_mut()).is_pending());
        assert!(futures::poll!(second.as_mut()).is_pending());
        let expected = Some(CallbackCode {
            code: "code".to_string(),
            state: "state".to_string(),
        });
        server.shared.settle(expected.clone()).await;
        server.shared.settle(None).await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), first)
                .await
                .unwrap(),
            expected
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), second)
                .await
                .unwrap(),
            expected
        );
        assert_eq!(server.wait_for_code().await, expected);
        server.cancel().await;
    }

    #[tokio::test]
    async fn settled_browser_code_survives_ready_input_after_listener_exit() {
        // An accept loop can exit while a previously accepted request is still completing.
        // Complete listener cleanup first so input cancellation will not yield on a join.
        for _ in 0..64 {
            let server = ephemeral_server().await;
            let task = server.listener_task.lock().await.take().unwrap();
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            let browser_code = CallbackCode {
                code: "browser-code".to_string(),
                state: "state".to_string(),
            };
            server.shared.settle(Some(browser_code.clone())).await;
            let mut input = Box::pin(async {
                server.cancel().await;
            });
            let (code, _) = server.wait_for_code_or_input(input.as_mut()).await;
            assert_eq!(
                code,
                Some(browser_code),
                "the browser settled before input cancellation"
            );
        }
    }

    #[tokio::test]
    async fn dropping_a_registered_callback_releases_the_port_without_yielding() {
        let (reservation, ownership) = test_listener::reserve_listener();
        let port = reservation.port();
        let server = reservation.into_callback().unwrap();
        assert_eq!(server.redirect_uri(), redirect_uri_for(port));
        assert!(ownership.is_listening());
        drop(server);
        assert!(
            ownership.is_closed(),
            "Drop closes actual owned sockets without a runtime turn"
        );
    }

    #[tokio::test]
    async fn registered_port_is_exact_and_released_after_cancel() {
        let blocker = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = blocker.local_addr().unwrap().port();
        let error = CallbackServer::start("Slack", Some(port))
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains(&format!("registered port {port}")));
        let (reservation, ownership) = test_listener::reserve_existing(blocker);
        let server = reservation.into_callback().unwrap();
        assert!(ownership.is_listening());
        assert_eq!(
            server.redirect_uri(),
            format!("http://localhost:{port}/callback")
        );
        let response = reqwest::Client::new()
            .get(format!(
                "http://127.0.0.1:{port}/callback?code=code&state=state"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            server.wait_for_code().await,
            Some(CallbackCode {
                code: "code".to_string(),
                state: "state".to_string(),
            })
        );
        server.cancel().await;
        assert!(
            ownership.is_closed(),
            "cancel closes actual owned callback sockets"
        );
    }

    #[tokio::test]
    async fn falls_back_to_the_next_free_port() {
        let blocker = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let blocked_port = blocker.local_addr().unwrap().port();
        let server = CallbackServer::start_with_ports("Linear", None, vec![blocked_port, 0])
            .await
            .unwrap();
        assert_ne!(server.port, blocked_port);
        assert_ne!(server.port, 0);
        assert_eq!(server.redirect_uri(), redirect_uri_for(server.port));
        server.cancel().await;
    }

    #[tokio::test]
    async fn all_candidates_bound_fails_clearly() {
        let blockers: Vec<_> = (0..2)
            .map(|_| std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap())
            .collect();
        let ports = blockers
            .iter()
            .map(|listener| listener.local_addr().unwrap().port())
            .collect();
        let error = CallbackServer::start_with_ports("Linear", None, ports)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("Could not start the OAuth callback server"));
    }

    #[tokio::test]
    async fn callback_flow_settles_code_and_state() {
        let server = ephemeral_server().await;
        let response = reqwest::Client::new()
            .get(format!(
                "http://127.0.0.1:{}/callback?code=the-code&state=the-state",
                server.port
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(
            server.wait_for_code().await,
            Some(CallbackCode {
                code: "the-code".to_string(),
                state: "the-state".to_string()
            })
        );
    }

    #[tokio::test]
    async fn error_callback_settles_none() {
        let server = ephemeral_server().await;
        let response = reqwest::Client::new()
            .get(format!(
                "http://127.0.0.1:{}/callback?error=access_denied",
                server.port
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 400);
        assert!(server.wait_for_code().await.is_none());
    }

    #[tokio::test]
    async fn unknown_route_is_404_without_settling() {
        let server = ephemeral_server().await;
        let response = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{}/other", server.port))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 404);
        // Unknown routes never settle the login; cancellation does.
        server.cancel().await;
        assert!(server.wait_for_code().await.is_none());
    }

    #[test]
    fn query_and_percent_decoding() {
        for (encoded, decoded) in [
            ("a%20b", "a b"),
            ("bad%2", "bad%2"),
            ("%💖", "%💖"),
            ("%é", "%é"),
            ("%a💖", "%a💖"),
            ("💖%💖%20", "💖%💖 "),
        ] {
            assert_eq!(percent_decode(encoded), decoded);
        }
        assert_eq!(
            query_param("code=1&state=2", "state"),
            Some("2".to_string())
        );
        assert_eq!(query_param("code=a+b", "code"), Some("a b".to_string()));
        assert_eq!(query_param("code", "state"), None);
    }
    #[tokio::test]
    async fn zero_callback_port_is_rejected_before_binding_or_registration() {
        assert!(redirect_uris(Some(0))
            .unwrap_err()
            .to_string()
            .contains("callbackPort"));
        let error = CallbackServer::start("fixture", Some(0)).await.unwrap_err();
        assert!(error.to_string().contains("callbackPort"));
    }

    #[tokio::test]
    async fn pinned_port_is_owned_until_drop() {
        let (reservation, ownership) = test_listener::reserve_listener();
        let port = reservation.port();
        let server = reservation.into_callback().unwrap();
        assert_eq!(server.redirect_uri(), redirect_uri_for(port));
        assert!(ownership.is_listening());
        drop(server);
        assert!(
            ownership.is_closed(),
            "Drop closes the pinned listener without a runtime turn"
        );
    }
}
