use std::io::{self, BufRead, BufReader, Read, Write};

use guest_contract::control;

const MAX_REQUEST_BYTES: u64 = 256;

pub(crate) fn request(wire: &mut impl BufRead) -> io::Result<String> {
    let mut line = String::new();
    wire.take(MAX_REQUEST_BYTES).read_line(&mut line)?;
    if !line.ends_with('\n') {
        return Err(io::Error::other(
            "the host control request is incomplete or too long",
        ));
    }
    Ok(line.trim_end().to_string())
}

pub(crate) fn confirm_freeze(wire: &mut BufReader<impl Read + Write>) -> io::Result<()> {
    writeln!(wire.get_mut(), "{}", control::TENANT_FREEZE_READY)?;
    if request(wire)? != control::TENANT_FREEZE_COMMIT {
        return Err(io::Error::other("the host did not confirm the tenant freeze"));
    }
    Ok(())
}

pub(crate) fn host_time(wire: &mut BufReader<impl Read + Write>) -> io::Result<u128> {
    writeln!(wire.get_mut(), "{}", control::TENANT_CLOCK_READY)?;
    request(wire)?
        .strip_prefix(control::TENANT_CLOCK_RELEASE)
        .ok_or_else(|| io::Error::other("the host did not release the guest clock"))?
        .parse()
        .map_err(|_| io::Error::other("invalid host time"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    fn pair() -> (BufReader<UnixStream>, BufReader<UnixStream>) {
        let (host, guest) = UnixStream::pair().unwrap();
        host.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        guest.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        (BufReader::new(host), BufReader::new(guest))
    }

    #[test]
    fn an_abandoned_wake_never_supplies_a_clock_value() {
        let (mut host, mut guest) = pair();
        let answer = std::thread::spawn(move || host_time(&mut guest));
        assert_eq!(request(&mut host).unwrap(), "READY");
        host.get_mut().shutdown(Shutdown::Write).unwrap();
        assert!(answer.join().unwrap().is_err());
    }

    #[test]
    fn the_clock_value_arrives_only_with_the_release() {
        let (mut host, mut guest) = pair();
        let answer = std::thread::spawn(move || host_time(&mut guest));
        assert_eq!(request(&mut host).unwrap(), "READY");
        writeln!(host.get_mut(), "GO 123456789").unwrap();
        assert_eq!(answer.join().unwrap().unwrap(), 123456789);
    }

    #[test]
    fn an_abandoned_sleep_never_commits_the_freeze() {
        let (mut host, mut guest) = pair();
        let answer = std::thread::spawn(move || confirm_freeze(&mut guest));
        assert_eq!(request(&mut host).unwrap(), "READY");
        host.get_mut().shutdown(Shutdown::Write).unwrap();
        assert!(answer.join().unwrap().is_err());
    }

    #[test]
    fn an_unterminated_or_oversized_release_is_refused() {
        for release in ["GO 123".to_string(), format!("GO {}\n", "1".repeat(256))] {
            assert!(request(&mut std::io::Cursor::new(release)).is_err());
        }
    }
}
