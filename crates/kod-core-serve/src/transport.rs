//! IPC transport: a Unix domain socket on Unix, a named pipe on
//! Windows.
//!
//! `serve.rs` speaks NDJSON over any `AsyncRead + AsyncWrite`; only
//! the *transport* was ever Unix-specific. This module is the seam:
//! `Listener` accepts connections, `Stream` is one connection (either
//! role), and both platforms present the same shape so `serve.rs`
//! stops naming `tokio::net::UnixStream`, which does not exist on
//! Windows.
//!
//! # Why boxed halves
//!
//! On Windows the server side is a `NamedPipeServer` and the client
//! side is a `NamedPipeClient` — distinct types with no common
//! concrete supertype. `Stream` therefore holds boxed read/write
//! halves from the start, and `into_split` just returns them. On Unix
//! the same shape wraps `UnixStream::into_split`'s owned halves. One
//! signature, both platforms, both roles.
//!
//! # Security model
//!
//! The daemon's boundary is "only the user who started it may
//! connect". On Unix that is `SO_PEERCRED`/`getpeereid` plus the
//! socket file's `0600` mode. On Windows a named pipe created with a
//! default DACL grants access to the creating user, and
//! `first_pipe_instance(true)` refuses a second server — the same
//! per-user guarantee, expressed the way Windows expresses it.
//! `check_peer` is a no-op on Windows because the kernel enforced the
//! ACL at connect time.

use tokio::io::{AsyncRead, AsyncWrite};

/// One connection's read/write halves, already split and boxed.
pub struct Stream {
    read: Box<dyn AsyncRead + Unpin + Send>,
    write: Box<dyn AsyncWrite + Unpin + Send>,
}

impl Stream {
    /// Consume the stream, returning its owned halves.
    pub fn into_split(
        self,
    ) -> (
        Box<dyn AsyncRead + Unpin + Send>,
        Box<dyn AsyncWrite + Unpin + Send>,
    ) {
        (self.read, self.write)
    }
}

// ---------------------------------------------------------------------------
// Unix
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod imp {
    use super::*;
    use kod_error::{KodError, Result};
    use std::path::Path;
    use tokio::net::{UnixListener, UnixStream};

    #[derive(Debug)]
    pub struct Listener {
        inner: UnixListener,
    }

    /// Bind the socket, refusing if a live daemon already owns it.
    ///
    /// The check-then-bind runs under an exclusive `flock` on a
    /// sibling `<socket>.lock` file so two concurrent `kod serve`
    /// invocations serialize. Without the lock, a connect-probe that
    /// saw a stale file could unlink a socket a sibling had just
    /// bound (check-then-act with no atomicity).
    pub async fn bind(path: &Path) -> Result<Listener> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(KodError::Io)?;
        }
        let _lock = SocketLock::acquire(path)?;
        match UnixStream::connect(path).await {
            Ok(_) => Err(KodError::InvalidState(format!(
                "another kod server is already listening on {}",
                path.display()
            ))),
            Err(_) => {
                // No file, or a stale one from a crashed daemon.
                // Removing is safe *under the lock*.
                let _ = std::fs::remove_file(path);
                let inner = UnixListener::bind(path).map_err(|e| {
                    KodError::Internal(format!("could not bind {}: {e}", path.display()))
                })?;
                Ok(Listener { inner })
            }
        }
    }

    /// `flock`-based exclusive lock on `<socket>.lock`. Held for the
    /// duration of `bind`; releases when the handle drops.
    struct SocketLock {
        _file: std::fs::File,
    }

    impl SocketLock {
        fn acquire(socket_path: &Path) -> Result<Self> {
            let lock_path = socket_path.with_extension("lock");
            let file = std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(&lock_path)
                .map_err(KodError::Io)?;
            use std::os::unix::io::AsRawFd;
            // SAFETY: valid fd; flock is the standard advisory lock.
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if rc != 0 {
                return Err(KodError::Io(std::io::Error::last_os_error()));
            }
            Ok(Self { _file: file })
        }
    }

    pub async fn connect(path: &Path) -> Result<Stream> {
        let s = UnixStream::connect(path).await.map_err(|e| {
            KodError::Internal(format!("could not connect to {}: {e}", path.display()))
        })?;
        let (r, w) = s.into_split();
        Ok(Stream {
            read: Box::new(r),
            write: Box::new(w),
        })
    }

    impl Listener {
        pub async fn accept(&self) -> Result<Stream> {
            let (s, _addr) = self
                .inner
                .accept()
                .await
                .map_err(|e| KodError::Internal(format!("accept failed: {e}")))?;
            // Peer check runs while the socket is whole; after split
            // the credentials are no longer reachable.
            check_stream_peer(&s)?;
            let (r, w) = s.into_split();
            Ok(Stream {
                read: Box::new(r),
                write: Box::new(w),
            })
        }
    }

    /// Reject a peer whose UID is not ours. Called by `accept` while
    /// the `UnixStream` is still whole (credentials are unreachable
    /// after a split).
    fn check_stream_peer(stream: &UnixStream) -> Result<()> {
        let cred = stream
            .peer_cred()
            .map_err(|e| KodError::Internal(format!("could not read peer credentials: {e}")))?;
        let our_uid = current_uid();
        if cred.uid() != our_uid {
            return Err(KodError::PermissionDenied {
                action: "connect".to_string(),
                reason: format!(
                    "peer uid {} does not match server uid {}",
                    cred.uid(),
                    our_uid
                ),
            });
        }
        Ok(())
    }

    fn current_uid() -> u32 {
        // SAFETY: `getuid` has no preconditions and cannot fail.
        unsafe { getuid() }
    }

    unsafe extern "C" {
        fn getuid() -> u32;
    }
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod imp {
    use super::*;
    use kod_error::{KodError, Result};
    use std::path::Path;
    use std::sync::Arc;
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};
    use tokio::sync::Mutex;

    pub struct Listener {
        name: String,
        #[allow(dead_code)] // Debug for tests; the handle is used via accept
        pending: Arc<Mutex<Option<NamedPipeServer>>>,
    }

    impl std::fmt::Debug for Listener {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Listener")
                .field("name", &self.name)
                .finish()
        }
    }

    /// Map the cross-platform path to a pipe name. The path keeps
    /// `default_socket_path()` uniform; the real identity is
    /// `\\.\pipe\kod-<hash>`.
    fn pipe_name(path: &Path) -> String {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        path.to_string_lossy().hash(&mut h);
        format!(r"\\.\pipe\kod-{:016x}", h.finish())
    }

    pub async fn bind(path: &Path) -> Result<Listener> {
        let name = pipe_name(path);
        let first = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&name)
            .map_err(|e| {
                KodError::InvalidState(format!(
                    "another kod server is already listening on {name}: {e}"
                ))
            })?;
        Ok(Listener {
            name,
            pending: Arc::new(Mutex::new(Some(first))),
        })
    }

    pub async fn connect(path: &Path) -> Result<Stream> {
        let name = pipe_name(path);
        let client = ClientOptions::new()
            .open(&name)
            .map_err(|e| KodError::Internal(format!("could not connect to {name}: {e}")))?;
        let (r, w) = tokio::io::split(client);
        Ok(Stream {
            read: Box::new(r),
            write: Box::new(w),
        })
    }

    impl Listener {
        pub async fn accept(&self) -> Result<Stream> {
            let server = {
                let mut g = self.pending.lock().await;
                match g.take() {
                    Some(s) => s,
                    None => ServerOptions::new()
                        .create(&self.name)
                        .map_err(|e| KodError::Internal(format!("create pipe: {e}")))?,
                }
            };
            server
                .connect()
                .await
                .map_err(|e| KodError::Internal(format!("pipe connect: {e}")))?;
            // Queue the next instance so a second client can wait
            // while this connection is served.
            let next = ServerOptions::new()
                .create(&self.name)
                .map_err(|e| KodError::Internal(format!("create pipe: {e}")))?;
            *self.pending.lock().await = Some(next);

            let (r, w) = tokio::io::split(server);
            Ok(Stream {
                read: Box::new(r),
                write: Box::new(w),
            })
        }
    }
}

pub use imp::{Listener, bind, connect};
