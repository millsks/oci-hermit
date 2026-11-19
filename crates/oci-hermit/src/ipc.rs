//! The local channel between a container's monitor and the commands that talk to it (#22).
//!
//! On Linux and macOS it is a Unix socket in `<data>/sockets/`, a folder only its owner can open. On Windows it is a
//! named pipe, `\\.\pipe\oci-hermit-<id>`, that refuses remote clients and that only the monitor may create: its
//! first instance is created with `FILE_FLAG_FIRST_PIPE_INSTANCE`, so a monitor that finds the name taken fails rather
//! than sharing it. Either way a connection is a pair of blocking halves that two threads can use at once.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// One connection, split so one thread can read while another writes.
pub struct Conn {
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
}

/// Where a container's socket lives: `<data>/sockets/` and the first 24 digits of its ID, which keeps the path short
/// enough for a Unix socket (at most 103 bytes on macOS) under the usual data folders.
#[cfg(any(unix, test))]
pub fn socket_path(data_dir: &Path, id: &str) -> PathBuf {
    data_dir.join("sockets").join(id.get(..24).unwrap_or(id))
}

/// The named pipe a container's monitor listens on.
#[cfg(any(windows, test))]
pub fn pipe_name(id: &str) -> String {
    format!(r"\\.\pipe\oci-hermit-{id}")
}

#[cfg(unix)]
mod imp {
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};

    /// The longest socket path every Unix takes: macOS's `sun_path` has 104 bytes, the last one a NUL.
    const MAX_PATH: usize = 103;

    pub struct Listener {
        inner: UnixListener,
        path: PathBuf,
    }

    fn checked(path: PathBuf) -> io::Result<PathBuf> {
        if path.as_os_str().len() > MAX_PATH {
            return Err(io::Error::other(format!(
                "the container's control socket {} would be longer than the {MAX_PATH} bytes a socket path may \
                 have; set {} to a shorter folder",
                path.display(),
                crate::store::DIR_ENV
            )));
        }
        Ok(path)
    }

    fn conn(stream: UnixStream) -> io::Result<Conn> {
        Ok(Conn {
            reader: Box::new(stream.try_clone()?),
            writer: Box::new(stream),
        })
    }

    impl Listener {
        /// Listens for `id`'s commands. A socket left by a monitor that died is replaced: only the process holding
        /// the container's running lock binds it.
        pub fn bind(data_dir: &Path, id: &str) -> io::Result<Listener> {
            let path = checked(socket_path(data_dir, id))?;
            let dir = path.parent().expect("the socket is in a folder");
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            let inner = UnixListener::bind(&path)?;
            Ok(Listener { inner, path })
        }

        pub fn accept(&mut self) -> io::Result<Conn> {
            let (stream, _) = self.inner.accept()?;
            conn(stream)
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    pub fn connect(data_dir: &Path, id: &str) -> io::Result<Conn> {
        conn(UnixStream::connect(checked(socket_path(data_dir, id))?)?)
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::sync::OnceLock;
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};

    /// All the pipes in this process share one runtime with one worker; the halves block on it.
    fn runtime() -> &'static tokio::runtime::Runtime {
        static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
        RUNTIME.get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_io()
                .build()
                .expect("a tokio runtime for named pipes")
        })
    }

    struct Reader<T>(ReadHalf<T>);
    struct Writer<T>(WriteHalf<T>);

    impl<T: AsyncRead + Send> Read for Reader<T> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match runtime().block_on(self.0.read(buf)) {
                // A client or server that went away ends the stream, as a Unix socket does.
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(0),
                other => other,
            }
        }
    }

    impl<T: AsyncWrite + Send> Write for Writer<T> {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            runtime().block_on(self.0.write(buf))
        }
        fn flush(&mut self) -> io::Result<()> {
            runtime().block_on(self.0.flush())
        }
    }

    fn conn<T: AsyncRead + AsyncWrite + Send + 'static>(pipe: T) -> Conn {
        let (reader, writer) = tokio::io::split(pipe);
        Conn {
            reader: Box::new(Reader(reader)),
            writer: Box::new(Writer(writer)),
        }
    }

    pub struct Listener {
        next: Option<NamedPipeServer>,
        name: String,
    }

    impl Listener {
        pub fn bind(_data_dir: &Path, id: &str) -> io::Result<Listener> {
            let name = pipe_name(id);
            let _guard = runtime().enter();
            let first = ServerOptions::new()
                .first_pipe_instance(true)
                .reject_remote_clients(true)
                .create(&name)?;
            Ok(Listener {
                next: Some(first),
                name,
            })
        }

        pub fn accept(&mut self) -> io::Result<Conn> {
            let server = match self.next.take() {
                Some(server) => server,
                None => {
                    let _guard = runtime().enter();
                    ServerOptions::new().reject_remote_clients(true).create(&self.name)?
                }
            };
            runtime().block_on(server.connect())?;
            // The next instance, so a client never finds the name gone between two connections.
            let _guard = runtime().enter();
            self.next = ServerOptions::new().reject_remote_clients(true).create(&self.name).ok();
            Ok(conn(server))
        }
    }

    pub fn connect(_data_dir: &Path, id: &str) -> io::Result<Conn> {
        const ERROR_PIPE_BUSY: i32 = 231;
        let name = pipe_name(id);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let _guard = runtime().enter();
            match ClientOptions::new().open(&name) {
                Ok(client) => return Ok(conn(client)),
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(e) => return Err(e),
            }
        }
    }
}

pub use imp::{Listener, connect};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qemu::testing::scratch;
    use std::io::{BufRead, BufReader};

    #[test]
    fn sockets_are_named_by_the_start_of_the_id() {
        let id = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert_eq!(
            socket_path(Path::new("/d"), id),
            Path::new("/d/sockets/0123456789abcdef01234567")
        );
        assert_eq!(socket_path(Path::new("/d"), "abc"), Path::new("/d/sockets/abc"));
        assert_eq!(pipe_name("abc"), r"\\.\pipe\oci-hermit-abc");
    }

    #[test]
    fn a_client_and_the_monitor_talk_both_ways() {
        let dir = scratch("ipc-talk");
        let id = format!("{:064x}", u128::from(std::process::id()) << 8 | 1);
        let mut listener = Listener::bind(&dir, &id).unwrap();
        let server = std::thread::spawn(move || {
            let mut conn = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&mut conn.reader).read_line(&mut line).unwrap();
            conn.writer.write_all(format!("echo {line}").as_bytes()).unwrap();
            conn.writer.flush().unwrap();
            // The client hanging up ends the stream.
            let mut rest = Vec::new();
            conn.reader.read_to_end(&mut rest).unwrap();
            rest
        });
        let mut client = connect(&dir, &id).unwrap();
        client.writer.write_all(b"hello\n").unwrap();
        client.writer.flush().unwrap();
        let mut line = String::new();
        BufReader::new(&mut client.reader).read_line(&mut line).unwrap();
        assert_eq!(line, "echo hello\n");
        drop(client);
        assert!(server.join().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn only_the_owner_can_reach_the_sockets_and_long_paths_are_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("ipc-owner");
        let listener = Listener::bind(&dir, "feedface").unwrap();
        let mode = std::fs::metadata(dir.join("sockets")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        // A second monitor for the same container replaces a socket left behind.
        std::mem::forget(listener);
        let again = Listener::bind(&dir, "feedface").unwrap();
        drop(again);
        assert!(!socket_path(&dir, "feedface").exists());
        assert!(connect(&dir, "feedface").is_err());

        let long = dir.join("x".repeat(120));
        let err = Listener::bind(&long, "feedface").err().unwrap();
        assert!(err.to_string().contains("set OCI_HERMIT_DATA_DIR"), "{err}");
        assert!(connect(&long, "feedface").is_err());
    }
}
