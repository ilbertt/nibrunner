use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use std::path::Path;

use guest_contract::paths;
use nix::sys::socket::{setsockopt, sockopt};
use nix::sys::time::{TimeVal, TimeValLike};
use protocol::{GuestPath, HranaError, HranaPipelineRequest, SQLITE_MAX_BODY_BYTES};
use sqlite::{Database, QueryError, Session};

use crate::guest::{memory, service};

const MAX_HEADER_BYTES: usize = 16_384;
const WORKER_ADDRESS_BYTES: libc::rlim_t = 128 * 1024 * 1024;
const QUERY_WATCHDOG_SECONDS: u32 = 6;

struct QueryDeadline;

impl QueryDeadline {
    fn start() -> Self {
        // SQLite scalar functions can run without invoking the VM progress callback.
        unsafe {
            libc::alarm(QUERY_WATCHDOG_SECONDS);
        }
        Self
    }
}

impl Drop for QueryDeadline {
    fn drop(&mut self) {
        unsafe {
            libc::alarm(0);
        }
    }
}

fn prepare_deadline() -> io::Result<()> {
    use nix::sys::signal::{SigAction, SigHandler, SigSet, Signal};
    let action = SigAction::new(
        SigHandler::SigDfl,
        nix::sys::signal::SaFlags::empty(),
        SigSet::empty(),
    );
    unsafe { nix::sys::signal::sigaction(Signal::SIGALRM, &action) }.map_err(io::Error::from)?;
    let mut unblocked = SigSet::empty();
    unblocked.add(Signal::SIGALRM);
    unblocked.thread_unblock().map_err(io::Error::from)
}

struct RequestHead {
    method: String,
    target: String,
    length: usize,
    close: bool,
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "the SQLite HTTP request is malformed")
}

fn read_head(reader: &mut BufReader<File>) -> io::Result<RequestHead> {
    let mut bytes = Vec::new();
    loop {
        let remaining = MAX_HEADER_BYTES.checked_sub(bytes.len()).ok_or_else(invalid)?;
        if remaining == 0 {
            return Err(invalid());
        }
        let count = reader
            .by_ref()
            .take(remaining as u64)
            .read_until(b'\n', &mut bytes)?;
        if count == 0 || !bytes.ends_with(b"\n") {
            return Err(invalid());
        }
        if bytes.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut request = httparse::Request::new(&mut headers);
    if !request.parse(&bytes).map_err(|_| invalid())?.is_complete() || request.version != Some(1) {
        return Err(invalid());
    }
    let mut length = None;
    let mut close = false;
    for header in request.headers.iter() {
        if header.name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(invalid());
        }
        if header.name.eq_ignore_ascii_case("content-length") {
            if length.is_some() || header.value.is_empty() || !header.value.iter().all(u8::is_ascii_digit) {
                return Err(invalid());
            }
            length = Some(
                std::str::from_utf8(header.value)
                    .map_err(|_| invalid())?
                    .parse::<usize>()
                    .map_err(|_| invalid())?,
            );
        }
        if header.name.eq_ignore_ascii_case("connection") {
            close = std::str::from_utf8(header.value)
                .map_err(|_| invalid())?
                .split(',')
                .any(|value| value.trim().eq_ignore_ascii_case("close"));
        }
    }
    let method = request.method.ok_or_else(invalid)?.to_owned();
    let target = request.path.ok_or_else(invalid)?.to_owned();
    if method == "POST" && length.is_none() {
        return Err(invalid());
    }
    let length = length.unwrap_or(0);
    if length > SQLITE_MAX_BODY_BYTES || (method == "GET" && length != 0) {
        return Err(invalid());
    }
    Ok(RequestHead {
        method,
        target,
        length,
        close,
    })
}

fn write_json(file: &mut File, status: u16, body: &impl serde::Serialize, close: bool) -> io::Result<()> {
    let bytes = serde_json::to_vec(body).map_err(io::Error::other)?;
    if bytes.len() > SQLITE_MAX_BODY_BYTES {
        return Err(invalid());
    }
    write!(file, "HTTP/1.1 {status} SQLite\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: {}\r\n\r\n", bytes.len(), if close { "close" } else { "keep-alive" })?;
    file.write_all(&bytes)
}

fn opening(target: &str) -> io::Result<(GuestPath, GuestPath)> {
    guest_contract::sqlite::opening_route(target).map_err(|_| invalid())
}

fn jail(root: &GuestPath) -> io::Result<()> {
    use nix::fcntl::{open, openat, OFlag};
    use nix::sys::stat::Mode;
    use nix::unistd::{chdir, chroot, fchdir, setgid, setgroups, setuid, Gid, Uid};
    let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    let mut directory = open(paths::VOLUME_UPPER_DIR, flags, Mode::empty()).map_err(io::Error::from)?;
    for component in root.as_str().split('/').filter(|component| !component.is_empty()) {
        directory = openat(&directory, component, flags, Mode::empty()).map_err(io::Error::from)?;
    }
    fchdir(&directory).map_err(io::Error::from)?;
    chroot(".").map_err(io::Error::from)?;
    chdir("/").map_err(io::Error::from)?;
    setgroups(&[]).map_err(io::Error::from)?;
    setgid(Gid::from_raw(paths::TENANT_GID)).map_err(io::Error::from)?;
    setuid(Uid::from_raw(paths::TENANT_UID)).map_err(io::Error::from)?;
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn peer_closed(fd: RawFd) -> bool {
    if service::stopping() {
        return true;
    }
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN | libc::POLLRDHUP,
        revents: 0,
    };
    if unsafe { libc::poll(&mut poll, 1, 0) } < 0 {
        return true;
    }
    if poll.revents & (libc::POLLHUP | libc::POLLRDHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
        return true;
    }
    if poll.revents & libc::POLLIN == 0 {
        return false;
    }
    let mut byte = 0u8;
    unsafe { libc::recv(fd, (&raw mut byte).cast(), 1, libc::MSG_PEEK | libc::MSG_DONTWAIT) == 0 }
}

fn serve(reader: &mut BufReader<File>) -> io::Result<()> {
    let head = read_head(reader)?;
    if head.method != "GET" {
        return Err(invalid());
    }
    let (root, path) = opening(&head.target)?;
    jail(&root)?;
    let fd = reader.get_ref().as_raw_fd();
    let opened = {
        let _deadline = QueryDeadline::start();
        Database::open(Path::new(path.as_str()), move || peer_closed(fd))
    };
    let database = match opened {
        Ok(database) => database,
        Err(error) => {
            return write_json(reader.get_mut(), 400, &error.wire(), true);
        }
    };
    let mut session = Session::new(database);
    write_json(reader.get_mut(), 200, &serde_json::json!({}), head.close)?;
    if head.close {
        return Ok(());
    }
    let pipeline = format!("{}{}", head.target, guest_contract::sqlite::PIPELINE_SUFFIX);
    while !service::stopping() && !session.is_closed() {
        let head = read_head(reader)?;
        if head.method != "POST" || head.target != pipeline {
            return Err(invalid());
        }
        let mut body = vec![0; head.length];
        reader.read_exact(&mut body)?;
        let request: HranaPipelineRequest = serde_json::from_slice(&body).map_err(|_| invalid())?;
        let result = {
            let _deadline = QueryDeadline::start();
            session.pipeline(request)
        };
        match result {
            Ok(body) => write_json(reader.get_mut(), 200, &body, head.close || session.is_closed())?,
            Err(error) => {
                write_json(reader.get_mut(), 400, &error.wire(), true)?;
                break;
            }
        }
        if head.close {
            break;
        }
    }
    Ok(())
}

pub(super) fn refuse(fd: RawFd) {
    let Ok(mut file) = copy_stream(fd) else {
        return;
    };
    let _ = write_json(
        &mut file,
        503,
        &HranaError {
            code: Some("SQLITE_BUSY".into()),
            message: "The guest has no available database worker.".into(),
        },
        true,
    );
}

fn copy_stream(fd: RawFd) -> io::Result<File> {
    unsafe { BorrowedFd::borrow_raw(fd) }
        .try_clone_to_owned()
        .map(File::from)
}

pub(super) fn answer(fd: RawFd, ceiling: &memory::Ceiling) {
    if prepare_deadline().is_err() || memory::join_sqlite(ceiling).is_err() {
        refuse(fd);
        return;
    }
    let limits = libc::rlimit {
        rlim_cur: WORKER_ADDRESS_BYTES,
        rlim_max: WORKER_ADDRESS_BYTES,
    };
    let files = libc::rlimit {
        rlim_cur: 64,
        rlim_max: 64,
    };
    if unsafe { libc::setrlimit(libc::RLIMIT_AS, &limits) } != 0
        || unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &files) } != 0
        || unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 19) } != 0
    {
        refuse(fd);
        return;
    }
    let Ok(file) = copy_stream(fd) else {
        return;
    };
    let timeout = TimeVal::milliseconds(guest_contract::sqlite::SESSION_IDLE_TIMEOUT.as_millis() as i64);
    if setsockopt(&file, sockopt::ReceiveTimeout, &timeout).is_err()
        || setsockopt(&file, sockopt::SendTimeout, &timeout).is_err()
    {
        return;
    }
    let mut reader = BufReader::new(file);
    if let Err(error) = serve(&mut reader) {
        if error.kind() == io::ErrorKind::InvalidData {
            let _ = write_json(reader.get_mut(), 400, &QueryError::protocol().wire(), true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;

    #[test]
    fn a_sqlite_worker_is_terminated_even_when_a_query_never_calls_its_progress_handler() {
        use nix::sys::signal::Signal;
        use nix::sys::wait::{waitpid, WaitStatus};
        use nix::unistd::ForkResult;
        match unsafe { nix::unistd::fork() }.unwrap() {
            ForkResult::Child => {
                prepare_deadline().unwrap();
                let _deadline = QueryDeadline::start();
                loop {
                    nix::unistd::pause();
                }
            }
            ForkResult::Parent { child } => {
                assert_eq!(
                    waitpid(child, None).unwrap(),
                    WaitStatus::Signaled(child, Signal::SIGALRM, false)
                );
            }
        }
    }

    fn header(bytes: &[u8]) -> io::Result<RequestHead> {
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(bytes).unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        read_head(&mut BufReader::new(File::from(OwnedFd::from(server))))
    }

    #[test]
    fn sqlite_http_requires_one_bounded_content_length_and_no_chunked_body() {
        let valid =
            header(b"POST /sqlite/root/db/v2/pipeline HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}").unwrap();
        assert_eq!(valid.length, 2);
        for bytes in [
            b"POST / HTTP/1.1\r\n\r\n".as_slice(),
            b"POST / HTTP/1.1\r\nContent-Length: 0\r\nContent-Length: 0\r\n\r\n",
            b"POST / HTTP/1.1\r\nContent-Length: 0\r\nTransfer-Encoding: chunked\r\n\r\n",
            b"POST / HTTP/1.1\r\nContent-Length: 4194305\r\n\r\n",
            b"GET / HTTP/1.1\r\nContent-Length: 1\r\n\r\nx",
        ] {
            assert!(header(bytes).is_err());
        }
        assert!(header(&vec![b'x'; MAX_HEADER_BYTES + 1]).is_err());
    }

    #[test]
    fn sqlite_opening_keeps_the_jail_root_and_database_path_separate() {
        let (root, path) = opening("/sqlite/%2Fapp%2Fdata/%2Fnested%20folder%2Fdb.sqlite/v2").unwrap();
        assert_eq!(root.as_str(), "/app/data");
        assert_eq!(path.as_str(), "/nested folder/db.sqlite");
        for target in [
            "/sqlite/%2F/%2F..%2Fsecret/v2",
            "/sqlite/%2F/%00/v2",
            "/sqlite/%2F/unescaped/path/v2",
        ] {
            assert!(opening(target).is_err());
        }
    }
}
