//! The thin TCP adapter over the sans-IO engine: a `std::net` listener, one
//! thread per connection, and a read -> feed -> write loop.
//!
//! Everything the protocol MEANS is in [`super::connection`]; this file owns
//! the transport and nothing else. It brings in no dependency: blocking
//! `std::net`, `std::thread::scope`, and no async runtime.
//!
//! ## Why `thread::scope` and not `Arc`
//!
//! A wire transaction is the service's single writer, HELD
//! (`docs/dist/OPS_CONTRACT.md` §1), and a `WriterGuard` borrows the
//! `ServiceDatabase` it took that writer from. A connection therefore
//! borrows the service rather than sharing an `Arc` of it, and
//! `std::thread::scope` is what lets that borrow cross into a connection
//! thread without leaking the service for the life of the process. It also
//! makes the shutdown a JOIN: [`serve`] returns once every connection thread
//! has finished, so a `close()` after it runs against a service no thread is
//! still inside.
//!
//! ## No TLS
//!
//! `OPS_CONTRACT` §9.4. An `SSLRequest` is declined with `N` and the session
//! continues in plaintext, so [`serve`] refuses to bind anything but a
//! loopback address unless the caller says `allow_remote` in as many words.

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::service::ServiceDatabase;

use super::connection::{BackendKey, CancelToken, Connection};

/// How long an accept loop sleeps between polls when nothing is waiting.
/// The listener is non-blocking so that [`Shutdown`] is noticed without a
/// second socket to wake it, and this is the latency that costs.
const ACCEPT_POLL: Duration = Duration::from_millis(20);
/// How long a connection socket waits for bytes before it re-checks the
/// shutdown flag and pushes any notification that arrived (`§9.3`).
const READ_POLL: Duration = Duration::from_millis(50);
/// The read buffer one connection uses.
const READ_BUFFER: usize = 16 << 10;

/// The live backends, so a `CancelRequest` arriving on a SECOND connection
/// can reach the one it names (`OPS_CONTRACT` §9.2).
#[derive(Clone, Default)]
pub struct Backends {
    inner: Arc<Mutex<Vec<(BackendKey, CancelToken)>>>,
}

impl Backends {
    pub fn new() -> Self {
        Self::default()
    }

    fn register(&self, key: BackendKey, token: CancelToken) {
        let mut live = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        live.push((key, token));
    }

    fn forget(&self, key: BackendKey) {
        let mut live = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        live.retain(|(live, _)| *live != key);
    }

    /// Fire the cancel of the backend that matches BOTH halves of the pair.
    /// Returns whether one did. A wrong secret cancels nothing, which is the
    /// whole reason the protocol carries one.
    pub fn cancel(&self, key: BackendKey) -> bool {
        let live = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        for (candidate, token) in live.iter() {
            if *candidate == key {
                token.cancel();
                return true;
            }
        }
        false
    }

    /// How many backends are live. For a caller that reports it.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The flag [`serve`] watches, so a caller can stop it from another thread.
#[derive(Clone, Default)]
pub struct Shutdown(Arc<AtomicBool>);

impl Shutdown {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask the accept loop to stop. Connections already open are allowed to
    /// finish the statement they are inside and then close.
    pub fn stop(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_stopped(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// What [`serve`] needs beyond the listener.
pub struct ServerOptions {
    /// Fired by the caller to stop the accept loop.
    pub shutdown: Shutdown,
    /// The `(pid, secret)` registry §9.2 routes cancels through.
    pub backends: Backends,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            shutdown: Shutdown::new(),
            backends: Backends::new(),
        }
    }
}

/// Bind `address`, refusing a non-loopback bind unless `allow_remote`.
///
/// §9.4: there is no TLS and no authentication beyond trust, so a port that
/// is reachable from another host is a database that is reachable from
/// another host. The refusal names that rather than leaving it implied.
pub fn bind(address: &str, allow_remote: bool) -> std::io::Result<TcpListener> {
    let listener = TcpListener::bind(address)?;
    let local = listener.local_addr()?;
    if !allow_remote && !is_loopback(&local) {
        return Err(std::io::Error::new(
            ErrorKind::PermissionDenied,
            format!(
                "refusing to serve {local}: the PostgreSQL surface has trust auth and no TLS \
                 (OPS_CONTRACT §9.4), so a non-loopback bind is an unauthenticated database on \
                 the network. Pass --allow-remote to say that is intended"
            ),
        ));
    }
    Ok(listener)
}

fn is_loopback(address: &SocketAddr) -> bool {
    match address {
        SocketAddr::V4(v4) => v4.ip().is_loopback(),
        SocketAddr::V6(v6) => v6.ip().is_loopback(),
    }
}

/// Serve `listener` against `service` until the shutdown flag is set.
///
/// Returns once every connection thread has finished, so the service is free
/// the instant this returns.
pub fn serve(
    listener: TcpListener,
    service: &ServiceDatabase,
    options: &ServerOptions,
) -> std::io::Result<()> {
    listener.set_nonblocking(true)?;
    // One counter per server, so the `(pid, secret)` pairs a client sees are
    // this server's and not the operating system's.
    let next_pid = AtomicI32::new(1);
    let secrets = SecretStream::new();

    std::thread::scope(|scope| {
        while !options.shutdown.is_stopped() {
            match listener.accept() {
                Ok((stream, _peer)) => {
                    let key = BackendKey {
                        pid: next_pid.fetch_add(1, Ordering::Relaxed),
                        secret: secrets.next(),
                    };
                    let backends = options.backends.clone();
                    let shutdown = options.shutdown.clone();
                    scope.spawn(move || {
                        let token = CancelToken::new();
                        backends.register(key, token.clone());
                        let result = handle(stream, service, key, token, &backends, &shutdown);
                        backends.forget(key);
                        if let Err(error) = result {
                            if !matches!(
                                error.kind(),
                                ErrorKind::UnexpectedEof
                                    | ErrorKind::ConnectionReset
                                    | ErrorKind::BrokenPipe
                            ) {
                                eprintln!("sekejap-pg: connection error: {error}");
                            }
                        }
                    });
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(ACCEPT_POLL);
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    })
}

/// One connection: read a chunk, feed it to the sans-IO engine, write back
/// whatever it produced.
fn handle(
    mut stream: TcpStream,
    service: &ServiceDatabase,
    key: BackendKey,
    token: CancelToken,
    backends: &Backends,
    shutdown: &Shutdown,
) -> std::io::Result<()> {
    // The LISTENER is non-blocking so the shutdown flag is noticed without a
    // second socket to wake it. On BSD and macOS an accepted socket INHERITS
    // that flag, and a non-blocking `write_all` of a large answer returns
    // EAGAIN rather than writing it -- which closes the connection in the
    // middle of a `DataRow`. So the connection socket is put back into
    // blocking mode explicitly, and its own read timeout is what lets this
    // loop notice a shutdown or a notification.
    stream.set_nonblocking(false)?;
    stream.set_nodelay(true).ok();
    stream.set_read_timeout(Some(READ_POLL))?;
    let mut connection = Connection::new(service, key, token);
    let mut buffer = vec![0u8; READ_BUFFER];
    loop {
        if shutdown.is_stopped() {
            return Ok(());
        }
        match stream.read(&mut buffer) {
            Ok(0) => return Ok(()), // the client closed
            Ok(n) => {
                let reply = connection.feed(&buffer[..n]);
                if !reply.is_empty() {
                    stream.write_all(&reply)?;
                }
            }
            Err(error)
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
            {
                // Idle. §9.3: a session that is LISTENing gets whatever the
                // change feed delivered while it was waiting.
                if connection.is_listening() {
                    let push = connection.poll_notify();
                    if !push.is_empty() {
                        stream.write_all(&push)?;
                    }
                }
            }
            Err(error) => return Err(error),
        }
        // §9.2: a `CancelRequest` arrives on its OWN connection, which
        // carries no startup and closes immediately. Route it, then close.
        if let Some(target) = connection.cancel_request() {
            backends.cancel(target);
            return Ok(());
        }
        if connection.is_closed() {
            return Ok(());
        }
    }
}

/// The secrets a `BackendKeyData` hands out.
///
/// A secret that a second connection can GUESS is not a secret, and guessing
/// one cancels another client's statement. There is no `getrandom` in this
/// crate's dependency set, so the stream is seeded from the clock and the
/// address of a heap allocation and stepped by SplitMix64 -- which is enough
/// that a secret is not the connection's ordinal, and is stated here rather
/// than claimed to be cryptographic.
struct SecretStream {
    state: std::sync::atomic::AtomicU64,
}

impl SecretStream {
    fn new() -> Self {
        let seeded = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0x9e37_79b9_7f4a_7c15, |d| d.as_nanos() as u64);
        let boxed = Box::new(0u8);
        let address = (&*boxed as *const u8) as u64;
        Self {
            state: std::sync::atomic::AtomicU64::new(seeded ^ address.rotate_left(17)),
        }
    }

    fn next(&self) -> i32 {
        let mut z = self
            .state
            .fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed)
            .wrapping_add(0x9e37_79b9_7f4a_7c15);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        // Positive, so the `Int32` a client echoes back is not a sign.
        ((z >> 1) as i32) | 1
    }
}
