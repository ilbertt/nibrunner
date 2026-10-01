use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use guest_contract::cron_registration::{self, RegistrationRequest, HEADER_BYTES, STATUS_OK};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Operation {
    Replace(Option<PathBuf>),
    List,
    Remove,
}

fn operation(arguments: &[OsString]) -> Option<Operation> {
    let [argument] = arguments else {
        return None;
    };
    match argument.to_str() {
        Some("-l") => Some(Operation::List),
        Some("-r") => Some(Operation::Remove),
        Some("-") => Some(Operation::Replace(None)),
        Some(flag) if flag.starts_with('-') => None,
        _ => Some(Operation::Replace(Some(PathBuf::from(argument)))),
    }
}

pub(crate) fn invoked(arguments: &[OsString]) -> Option<&[OsString]> {
    let (program, rest) = arguments.split_first()?;
    if Path::new(program)
        .file_name()
        .is_some_and(|name| name == "crontab")
    {
        return Some(rest);
    }
    if rest.first().is_some_and(|argument| argument == "crontab") {
        return Some(&rest[1..]);
    }
    None
}

fn read_source(source: &mut impl Read) -> io::Result<protocol::Crontab> {
    let mut bytes = Vec::new();
    source
        .take(protocol::MAX_CRONTAB_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    let text = String::from_utf8(bytes).map_err(io::Error::other)?;
    protocol::Crontab::parse(&text).map_err(io::Error::other)
}

fn request(operation: &Operation) -> io::Result<RegistrationRequest> {
    Ok(match operation {
        Operation::List => RegistrationRequest::List,
        Operation::Remove => {
            RegistrationRequest::Replace(protocol::Crontab::parse("").map_err(io::Error::other)?)
        }
        Operation::Replace(Some(path)) => {
            RegistrationRequest::Replace(read_source(&mut std::fs::File::open(path)?)?)
        }
        Operation::Replace(None) => RegistrationRequest::Replace(read_source(&mut io::stdin().lock())?),
    })
}

fn exchange(
    connection: &mut (impl Read + Write),
    request: &RegistrationRequest,
) -> io::Result<cron_registration::RegistrationReply> {
    connection.write_all(&cron_registration::encode_request(request))?;
    let mut header = [0u8; HEADER_BYTES];
    connection.read_exact(&mut header)?;
    let header = cron_registration::decode_reply_header(&header).map_err(io::Error::other)?;
    if matches!(request, RegistrationRequest::Replace(_))
        && header.status == STATUS_OK
        && header.body_length != 0
    {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut body = vec![0u8; header.body_length];
    connection.read_exact(&mut body)?;
    cron_registration::decode_reply(header, &body).map_err(io::Error::other)
}

#[cfg(target_os = "linux")]
fn connect() -> io::Result<std::fs::File> {
    use nix::sys::socket::{
        connect, setsockopt, socket, sockopt, AddressFamily, SockFlag, SockType, VsockAddr,
    };
    use std::os::fd::AsRawFd;
    let socket = socket(
        AddressFamily::Vsock,
        SockType::Stream,
        SockFlag::SOCK_CLOEXEC,
        None,
    )?;
    let timeout = nix::sys::time::TimeVal::new(5, 0);
    setsockopt(&socket, sockopt::ReceiveTimeout, &timeout)?;
    setsockopt(&socket, sockopt::SendTimeout, &timeout)?;
    connect(socket.as_raw_fd(), &VsockAddr::new(2, cron_registration::PORT))?;
    Ok(std::fs::File::from(socket))
}

#[cfg(not(target_os = "linux"))]
fn connect() -> io::Result<std::fs::File> {
    Err(io::ErrorKind::Unsupported.into())
}

pub(crate) fn run(arguments: &[OsString]) -> ExitCode {
    let Some(operation) = operation(arguments) else {
        eprintln!("Usage: crontab FILE | crontab - | crontab -l | crontab -r");
        return ExitCode::FAILURE;
    };
    let Ok(request) = request(&operation) else {
        eprintln!(
            "crontab: cannot read input or table exceeds {} bytes",
            protocol::MAX_CRONTAB_BYTES
        );
        return ExitCode::FAILURE;
    };
    let Ok(mut connection) = connect() else {
        eprintln!("crontab: cannot connect to the host");
        return ExitCode::FAILURE;
    };
    let reply = match exchange(&mut connection, &request) {
        Ok(reply) => reply,
        Err(_) => {
            eprintln!("crontab: host connection failed; replacement was not confirmed");
            return ExitCode::FAILURE;
        }
    };
    if reply.status == STATUS_OK {
        if io::stdout().write_all(reply.text.as_bytes()).is_err() {
            return ExitCode::FAILURE;
        }
        ExitCode::SUCCESS
    } else {
        eprintln!("crontab: {}", reply.text);
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_supported_argument_selects_a_crontab_operation() {
        for (argument, expected) in [
            ("-l", Operation::List),
            ("-r", Operation::Remove),
            ("-", Operation::Replace(None)),
            ("jobs.txt", Operation::Replace(Some("jobs.txt".into()))),
        ] {
            assert_eq!(operation(&[argument.into()]), Some(expected));
        }
        assert_eq!(operation(&[]), None);
        assert_eq!(operation(&["-e".into()]), None);
        assert_eq!(operation(&["-l".into(), "jobs.txt".into()]), None);
    }

    #[test]
    fn crontab_invocation_works_through_a_symlink_or_an_init_subcommand() {
        let symlink = [OsString::from("/usr/bin/crontab"), "-l".into()];
        let subcommand = [OsString::from("/init"), "crontab".into(), "-r".into()];
        assert_eq!(invoked(&symlink), Some(&symlink[1..]));
        assert_eq!(invoked(&subcommand), Some(&subcommand[2..]));
        assert_eq!(invoked(&["/init".into()]), None);
    }

    #[test]
    fn a_source_is_bounded_in_bytes_and_never_silently_truncated() {
        let boundary = vec![b'a'; protocol::MAX_CRONTAB_BYTES];
        assert_eq!(
            read_source(&mut boundary.as_slice()).unwrap().expose().len(),
            boundary.len()
        );
        let oversized = vec![b'a'; protocol::MAX_CRONTAB_BYTES + 1];
        assert!(read_source(&mut oversized.as_slice()).is_err());
        assert!(read_source(&mut b"\xff".as_slice()).is_err());
        assert!(read_source(&mut b"\0".as_slice()).is_err());
    }

    struct Connection {
        response: io::Cursor<Vec<u8>>,
        sent: Vec<u8>,
    }
    impl Read for Connection {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            self.response.read(bytes)
        }
    }
    impl Write for Connection {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.sent.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn list_preserves_the_installed_text_and_replace_needs_an_empty_confirmation() {
        let mut connection = Connection {
            response: io::Cursor::new(cron_registration::encode_reply(STATUS_OK, "* * * * * echo ok\n")),
            sent: vec![],
        };
        let reply = exchange(&mut connection, &RegistrationRequest::List).unwrap();
        assert_eq!(reply.text, "* * * * * echo ok\n");
        assert_eq!(connection.sent, b"NBC1\x02\x00\x00\x00\x00");
        let request = RegistrationRequest::Replace(protocol::Crontab::parse("").unwrap());
        connection.response.set_position(0);
        assert!(exchange(&mut connection, &request).is_err());
    }
}
