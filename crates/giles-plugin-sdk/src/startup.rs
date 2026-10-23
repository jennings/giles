//! The plugin side of the launch handshake (PLAN section 7.4).
//!
//! In order: the magic cookie, the protocol choice, the data directory, fd 3, and the token on
//! stdin. Once the server runs, the handshake line goes to stdout. Stdin then stays open until the
//! core goes away; its end-of-file is the plugin's only reason to exit on its own.

use std::ffi::OsString;
use std::io::{self, BufRead, Read as _, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use giles_proto::handshake::{
    DATA_ENV, EXIT_NO_SHARED_PROTOCOL, HandshakeLine, MAGIC_COOKIE, MAGIC_COOKIE_ENV,
    PROTOCOL_MAJOR, PROTOCOLS_ENV, parse_major,
};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

/// The plugin protocol majors this SDK speaks.
pub(crate) const SUPPORTED_PROTOCOLS: &[u32] = &[PROTOCOL_MAJOR];

/// The longest token accepted, in bytes. The core's tokens are far shorter.
const MAX_TOKEN_LEN: usize = 1024;

/// Why the plugin couldn't start.
#[derive(Debug, thiserror::Error)]
pub(crate) enum StartupError {
    /// [`MAGIC_COOKIE_ENV`] is missing or wrong: someone ran the binary by hand.
    #[error("not launched by Giles ({MAGIC_COOKIE_ENV} is not set)")]
    NotLaunchedByGiles,
    /// [`PROTOCOLS_ENV`] is missing or isn't a list of protocol majors.
    #[error("{PROTOCOLS_ENV} must list protocol majors, e.g. \"1\", got {0:?}")]
    MalformedProtocols(String),
    /// The core and the plugin share no protocol major.
    #[error(
        "no shared plugin protocol: Giles speaks {offered}, this plugin speaks {}",
        join(SUPPORTED_PROTOCOLS)
    )]
    NoSharedProtocol {
        /// What the core offered, as it wrote it.
        offered: String,
    },
    /// [`DATA_ENV`] is missing or isn't an absolute path to a directory.
    #[error("{DATA_ENV} must name the plugin's data directory, got {0:?}")]
    DataDir(OsString),
    /// Fd 3 isn't an open Unix socket.
    #[error("fd 3 is not the plugin's socket: {0}")]
    PluginFd(io::Error),
    /// Stdin didn't start with a valid token line.
    #[error("reading the token from stdin: {0}")]
    Token(String),
    /// The SDK's state in the data directory couldn't be opened.
    #[error("opening the plugin's state in its data directory: {0}")]
    State(io::Error),
    /// Writing the handshake line failed.
    #[error("writing the handshake line: {0}")]
    Handshake(io::Error),
    /// The gRPC server failed.
    #[error("serving gRPC on fd 3: {0}")]
    Serve(String),
}

impl StartupError {
    /// The process exit status for this error.
    pub(crate) fn exit_code(&self) -> u8 {
        match self {
            Self::NoSharedProtocol { .. } => {
                u8::try_from(EXIT_NO_SHARED_PROTOCOL).expect("exit statuses fit in u8")
            }
            _ => 1,
        }
    }
}

fn join(majors: &[u32]) -> String {
    majors
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// Checks the magic cookie.
pub(crate) fn check_magic(env: &dyn Fn(&str) -> Option<OsString>) -> Result<(), StartupError> {
    match env(MAGIC_COOKIE_ENV) {
        Some(value) if value == MAGIC_COOKIE => Ok(()),
        _ => Err(StartupError::NotLaunchedByGiles),
    }
}

/// Picks the highest protocol major in both `offered` (the value of [`PROTOCOLS_ENV`]) and
/// `supported`.
pub(crate) fn choose_protocol(
    offered: Option<OsString>,
    supported: &[u32],
) -> Result<u32, StartupError> {
    let text = offered.unwrap_or_default();
    let malformed = || StartupError::MalformedProtocols(text.to_string_lossy().into_owned());
    let list = text.to_str().ok_or_else(malformed)?;
    let offered = list
        .split(',')
        .map(|major| parse_major(major.trim()))
        .collect::<Option<Vec<u32>>>()
        .ok_or_else(malformed)?;
    offered
        .iter()
        .filter(|major| supported.contains(major))
        .max()
        .copied()
        .ok_or_else(|| StartupError::NoSharedProtocol {
            offered: list.to_owned(),
        })
}

/// The data directory from [`DATA_ENV`]: an absolute path to an existing directory.
pub(crate) fn data_dir(env: &dyn Fn(&str) -> Option<OsString>) -> Result<PathBuf, StartupError> {
    let value = env(DATA_ENV).unwrap_or_default();
    let path = PathBuf::from(&value);
    if path.is_absolute() && path.is_dir() {
        Ok(path)
    } else {
        Err(StartupError::DataDir(value))
    }
}

/// Takes ownership of the inherited socket on `fd` (fd 3 in a launched plugin) and readies it for
/// tokio: close-on-exec, non-blocking.
///
/// Fails without touching `fd` unless it's an open socket. Must run before anything else in the
/// process could have closed `fd` or opened another file on it.
#[allow(unsafe_code)]
pub(crate) fn adopt_socket(fd: RawFd) -> io::Result<UnixStream> {
    // SAFETY: `F_GETFD` only reads the descriptor flags; it fails with `EBADF` if `fd` isn't open.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    let mut stat = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `fd` is open (checked above) and `stat` is valid for writes of a `struct stat`.
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fstat` succeeded, so it initialized `stat`.
    let mode = unsafe { stat.assume_init() }.st_mode;
    if mode & libc::S_IFMT != libc::S_IFSOCK {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a socket"));
    }
    // SAFETY: `fd` is an open socket (checked above) that the launcher passed to this process for
    // the SDK alone; nothing else in the process uses or closes it, so the `OwnedFd` becomes its
    // only owner.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: `F_SETFD` on a descriptor we own; it only changes the descriptor flags. Without
    // close-on-exec, a child the plugin spawns would inherit the gRPC socket and keep the
    // connection open after the plugin died.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let stream = UnixStream::from(fd);
    // A socket of another family fails here rather than in the server.
    stream.peer_addr()?;
    stream.set_nonblocking(true)?;
    Ok(stream)
}

/// Reads the token line: visible ASCII (so `Bearer <token>` is a valid header value), at most
/// [`MAX_TOKEN_LEN`] bytes, ended by `\n` (or `\r\n`).
pub(crate) fn read_token(reader: &mut impl BufRead) -> Result<String, StartupError> {
    let mut line = Vec::new();
    let limit = u64::try_from(MAX_TOKEN_LEN + 2).expect("fits in u64");
    reader
        .by_ref()
        .take(limit)
        .read_until(b'\n', &mut line)
        .map_err(|e| StartupError::Token(e.to_string()))?;
    if line.pop() != Some(b'\n') {
        return Err(StartupError::Token(if line.len() > MAX_TOKEN_LEN {
            "the token line is too long".into()
        } else {
            "stdin ended before the token line did".into()
        }));
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    if line.is_empty() || line.len() > MAX_TOKEN_LEN || !line.iter().all(u8::is_ascii_graphic) {
        return Err(StartupError::Token(
            "the token must be 1 to 1024 visible ASCII characters".into(),
        ));
    }
    Ok(String::from_utf8(line).expect("ASCII is UTF-8"))
}

/// Reads the token from `reader` and then watches it for end-of-file, on a thread of its own.
///
/// Returns the token (or why there's none) and a token cancelled at end-of-file or a read error.
/// A plain thread rather than tokio's blocking pool: the read can't be cancelled, and a pending
/// blocking task would hold up the runtime's shutdown.
pub(crate) fn watch_stdin(
    reader: impl BufRead + Send + 'static,
) -> (
    oneshot::Receiver<Result<String, StartupError>>,
    CancellationToken,
) {
    let (token_tx, token_rx) = oneshot::channel();
    let closed = CancellationToken::new();
    let on_close = closed.clone();
    std::thread::Builder::new()
        .name("giles-plugin-stdin".into())
        .spawn(move || {
            let mut reader = reader;
            let token = read_token(&mut reader);
            let ok = token.is_ok();
            let _ = token_tx.send(token);
            if ok {
                drain(&mut reader);
            }
            on_close.cancel();
        })
        .expect("spawning the stdin thread");
    (token_rx, closed)
}

/// Discards input until end-of-file or a read error.
fn drain(reader: &mut impl BufRead) {
    loop {
        match reader.fill_buf() {
            Ok([]) => return,
            Ok(buf) => {
                let len = buf.len();
                reader.consume(len);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

/// Writes the handshake line and flushes it.
pub(crate) fn write_handshake(out: &mut impl Write, protocol: u32) -> Result<(), StartupError> {
    writeln!(out, "{}", HandshakeLine { protocol })
        .and_then(|()| out.flush())
        .map_err(StartupError::Handshake)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::Cursor;
    use std::os::fd::IntoRawFd;
    use std::time::Duration;

    use super::*;

    fn env_of(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let vars: HashMap<String, OsString> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), OsString::from(v)))
            .collect();
        move |name| vars.get(name).cloned()
    }

    #[test]
    fn magic_cookie_must_match_exactly() {
        check_magic(&env_of(&[(MAGIC_COOKIE_ENV, MAGIC_COOKIE)])).unwrap();
        for env in [env_of(&[]), env_of(&[(MAGIC_COOKIE_ENV, "1")])] {
            assert!(matches!(
                check_magic(&env),
                Err(StartupError::NotLaunchedByGiles)
            ));
        }
    }

    #[test]
    fn highest_shared_protocol_wins() {
        let choose =
            |offered: &str, supported: &[u32]| choose_protocol(Some(offered.into()), supported);
        assert_eq!(choose("1", &[1]).unwrap(), 1);
        assert_eq!(choose("3,1,2", &[1, 2]).unwrap(), 2);
        assert_eq!(choose(" 1 , 2 ", &[1, 2, 3]).unwrap(), 2);
        let err = choose("2,3", &[1]).unwrap_err();
        assert!(matches!(&err, StartupError::NoSharedProtocol { offered } if offered == "2,3"));
        assert_eq!(err.exit_code(), 76);
        assert_eq!(
            err.to_string(),
            "no shared plugin protocol: Giles speaks 2,3, this plugin speaks 1"
        );
    }

    #[test]
    fn malformed_protocol_lists_are_refused() {
        for offered in ["", "1,", ",1", "v1", "1;2", "-1"] {
            let err = choose_protocol(Some(offered.into()), &[1]).unwrap_err();
            assert!(
                matches!(&err, StartupError::MalformedProtocols(text) if text == offered),
                "{offered:?}: {err}"
            );
            assert_eq!(err.exit_code(), 1);
        }
        assert!(matches!(
            choose_protocol(None, &[1]),
            Err(StartupError::MalformedProtocols(_))
        ));
    }

    #[test]
    fn data_dir_must_be_an_absolute_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap();
        assert_eq!(data_dir(&env_of(&[(DATA_ENV, path)])).unwrap(), dir.path());
        let file = dir.path().join("file");
        std::fs::write(&file, "").unwrap();
        for value in ["", "relative", file.to_str().unwrap(), "/nonexistent/giles"] {
            assert!(
                matches!(
                    data_dir(&env_of(&[(DATA_ENV, value)])),
                    Err(StartupError::DataDir(_))
                ),
                "{value:?}"
            );
        }
        assert!(matches!(
            data_dir(&env_of(&[])),
            Err(StartupError::DataDir(_))
        ));
    }

    #[test]
    fn token_is_one_visible_ascii_line() {
        let read = |input: &[u8]| read_token(&mut Cursor::new(input.to_vec()));
        assert_eq!(read(b"tok-1\n").unwrap(), "tok-1");
        assert_eq!(read(b"tok-1\r\nignored").unwrap(), "tok-1");
        let max = format!("{}\n", "a".repeat(MAX_TOKEN_LEN));
        assert_eq!(read(max.as_bytes()).unwrap().len(), MAX_TOKEN_LEN);
        let long = format!("{}\n", "a".repeat(MAX_TOKEN_LEN + 1));
        for input in [
            &b""[..],
            b"tok",
            b"\n",
            b"to k\n",
            b"tok\x7f\n",
            "tök\n".as_bytes(),
            long.as_bytes(),
        ] {
            assert!(
                matches!(read(input), Err(StartupError::Token(_))),
                "{input:?}"
            );
        }
    }

    #[tokio::test]
    async fn stdin_watch_yields_the_token_then_waits_for_end_of_file() {
        let (reader, mut writer) = io::pipe().unwrap();
        let (token, closed) = watch_stdin(io::BufReader::new(reader));
        writer
            .write_all(b"secret\nmore input is ignored\n")
            .unwrap();
        assert_eq!(token.await.unwrap().unwrap(), "secret");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!closed.is_cancelled(), "stdin is still open");
        drop(writer);
        tokio::time::timeout(Duration::from_secs(5), closed.cancelled())
            .await
            .expect("end-of-file cancels the token");
    }

    #[tokio::test]
    async fn stdin_watch_reports_a_missing_token() {
        let (token, closed) = watch_stdin(Cursor::new(b"no newline".to_vec()));
        assert!(matches!(token.await.unwrap(), Err(StartupError::Token(_))));
        closed.cancelled().await;
    }

    #[test]
    fn adopting_a_socket_sets_close_on_exec_and_non_blocking() {
        let (_ours, theirs) = UnixStream::pair().unwrap();
        let fd = theirs.into_raw_fd();
        // Inherited descriptors arrive without close-on-exec.
        clear_cloexec(fd);
        let adopted = adopt_socket(fd).unwrap();
        assert_eq!(adopted.as_raw_fd(), fd);
        assert_ne!(fcntl_getfd(fd) & libc::FD_CLOEXEC, 0);
        let mut buf = [0; 1];
        let err = (&adopted).read(&mut buf).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn adopting_something_else_fails_and_leaves_it_open() {
        let (reader, writer) = io::pipe().unwrap();
        let fd = reader.into_raw_fd();
        let err = adopt_socket(fd).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_ne!(fcntl_getfd(fd), -1, "the pipe is still open");
        // SAFETY: test-only: `fd` came from `into_raw_fd` above and nothing else owns it.
        #[allow(unsafe_code)]
        drop(unsafe { OwnedFd::from_raw_fd(fd) });
        drop(writer);
        // (A closed descriptor fails `F_GETFD`; testing it here would race with other tests
        // reusing the number.)
    }

    #[test]
    fn handshake_line_is_flushed_with_a_newline() {
        let mut out = Vec::new();
        write_handshake(&mut out, 1).unwrap();
        assert_eq!(out, b"1|1|fd|3|grpc\n");
    }

    #[allow(unsafe_code)]
    fn fcntl_getfd(fd: RawFd) -> i32 {
        // SAFETY: `F_GETFD` only reads the descriptor flags.
        unsafe { libc::fcntl(fd, libc::F_GETFD) }
    }

    #[allow(unsafe_code)]
    fn clear_cloexec(fd: RawFd) {
        let flags = fcntl_getfd(fd) & !libc::FD_CLOEXEC;
        // SAFETY: `F_SETFD` only changes the descriptor flags of a descriptor the test owns.
        assert_ne!(unsafe { libc::fcntl(fd, libc::F_SETFD, flags) }, -1);
    }
}
