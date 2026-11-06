use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::net::{IpAddr, Ipv6Addr, SocketAddr, SocketAddrV6};
use std::os::{fd::OwnedFd, unix::net::UnixStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use super::{remaining, MAX_ADDRESSES};

const GETENT: &str = "/usr/bin/getent";
const PRLIMIT: &str = "/usr/bin/prlimit";
const MAX_OUTPUT_BYTES: usize = 32768;

#[derive(Debug, PartialEq, Eq)]
enum Mode {
    Auto,
    Process,
    Native,
}

impl Mode {
    fn parse(value: Option<&OsStr>) -> io::Result<Self> {
        match value.and_then(OsStr::to_str) {
            None if value.is_none() => Ok(Self::Auto),
            Some("auto") => Ok(Self::Auto),
            Some("process") => Ok(Self::Process),
            Some("native") => Ok(Self::Native),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid DFOR77_DNS_MODE",
            )),
        }
    }

    fn use_process(&self, available: bool) -> io::Result<bool> {
        match self {
            Self::Native => Ok(false),
            Self::Auto => Ok(available),
            Self::Process if available => Ok(true),
            Self::Process => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "DNS process helpers unavailable",
            )),
        }
    }
}

struct ResolverChild(Child);

impl Drop for ResolverChild {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

pub(super) fn try_resolve(
    host: &str,
    port: u16,
    deadline: Instant,
) -> Option<io::Result<Vec<SocketAddr>>> {
    let mode = match Mode::parse(std::env::var_os("DFOR77_DNS_MODE").as_deref()) {
        Ok(mode) => mode,
        Err(error) => return Some(Err(error)),
    };
    let available = || Path::new(GETENT).is_file() && Path::new(PRLIMIT).is_file();
    match mode.use_process(mode != Mode::Native && available()) {
        Ok(false) => return None,
        Ok(true) => {}
        Err(error) => return Some(Err(error)),
    }
    Some(run(
        command(
            Path::new(PRLIMIT),
            Path::new(GETENT),
            host,
            std::env::vars_os(),
        ),
        port,
        deadline,
    ))
}

fn command(
    prlimit: &Path,
    getent: &Path,
    host: &str,
    environment: impl IntoIterator<Item = (OsString, OsString)>,
) -> Command {
    let mut command = Command::new(prlimit);
    command
        .args(["--core=0", "--"])
        .arg(getent)
        .args(["--no-addrconfig", "--no-idn", "--", "ahosts", host])
        .env_clear()
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    for (key, value) in environment {
        if matches!(
            key.to_str(),
            Some(
                "LOCALDOMAIN"
                    | "RES_OPTIONS"
                    | "HOSTALIASES"
                    | "HOME"
                    | "SSL_CERT_FILE"
                    | "SSL_CERT_DIR"
                    | "KRB5_CONFIG"
                    | "KRB5CCNAME"
                    | "KRB5_KTNAME"
                    | "LDAPCONF"
                    | "LDAPRC"
                    | "LDAPNOINIT"
            )
        ) {
            command.env(key, value);
        }
    }
    command
}

fn run(mut command: Command, port: u16, deadline: Instant) -> io::Result<Vec<SocketAddr>> {
    remaining(deadline)?;
    let (mut output, peer) = UnixStream::pair()?;
    command.stdout(Stdio::from(OwnedFd::from(peer)));
    let spawned = command.spawn();
    drop(command);
    let mut child = ResolverChild(spawned?);
    let mut addresses = Vec::new();
    let mut pending = Vec::new();
    let mut received = 0_usize;
    let mut tcp_addresses = 0_usize;
    let mut buffer = [0_u8; 1024];
    loop {
        output.set_read_timeout(Some(remaining(deadline)?))?;
        let read = match output.read(&mut buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if read == 0 {
            break;
        }
        received += read;
        if received > MAX_OUTPUT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "DNS output limit exceeded",
            ));
        }
        pending.extend_from_slice(&buffer[..read]);
        let mut consumed = 0;
        while let Some(end) = pending[consumed..].iter().position(|byte| *byte == b'\n') {
            let end = consumed + end;
            if let Some(address) = address(&pending[consumed..end], port)? {
                tcp_addresses += 1;
                if !addresses.contains(&address) {
                    addresses.push(address);
                }
                if tcp_addresses == MAX_ADDRESSES {
                    return Ok(addresses);
                }
            }
            consumed = end + 1;
        }
        pending.drain(..consumed);
    }
    if !pending.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "incomplete DNS output",
        ));
    }
    let status = loop {
        if let Some(status) = child.0.try_wait()? {
            break status;
        }
        let wait = remaining(deadline)?;
        std::thread::sleep(wait.min(Duration::from_millis(5)));
    };
    if !status.success() || addresses.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "system resolver failed",
        ));
    }
    Ok(addresses)
}

fn address(line: &[u8], port: u16) -> io::Result<Option<SocketAddr>> {
    let mut fields = line
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|part| !part.is_empty());
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid DNS output");
    let text = std::str::from_utf8(fields.next().ok_or_else(invalid)?).map_err(|_| invalid())?;
    let kind = fields.next().ok_or_else(invalid)?;
    if kind != b"STREAM" {
        return Ok(None);
    }
    if let Ok(ip) = text.parse::<IpAddr>() {
        return Ok(Some(SocketAddr::new(ip, port)));
    }
    let (ip, scope) = text.split_once('%').ok_or_else(invalid)?;
    let ip = ip.parse::<Ipv6Addr>().map_err(|_| invalid())?;
    let scope = scope.parse::<u32>().map_err(|_| invalid())?;
    Ok(Some(SocketAddr::V6(SocketAddrV6::new(ip, port, 0, scope))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    #[test]
    fn resolver_mode_preserves_native_and_can_require_process_helpers() {
        for (value, expected) in [
            (None, Mode::Auto),
            (Some("auto"), Mode::Auto),
            (Some("process"), Mode::Process),
            (Some("native"), Mode::Native),
        ] {
            assert_eq!(Mode::parse(value.map(OsStr::new)).unwrap(), expected);
        }
        for available in [false, true] {
            assert!(!Mode::Native.use_process(available).unwrap());
            assert_eq!(Mode::Auto.use_process(available).unwrap(), available);
        }
        assert!(Mode::Process.use_process(true).unwrap());
        assert_eq!(
            Mode::Process.use_process(false).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn resolver_mode_rejects_unknown_and_non_unicode_values() {
        use std::os::unix::ffi::OsStrExt;

        for value in [
            OsStr::new(""),
            OsStr::new("PROCESS"),
            OsStr::from_bytes(b"\xff"),
        ] {
            assert_eq!(
                Mode::parse(Some(value)).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    struct Helper(PathBuf);

    impl Helper {
        fn new(script: &str) -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir()
                .join(format!("dfor77-resolver-{}-{nonce}", std::process::id()));
            std::fs::create_dir(&directory).unwrap();
            let executable = directory.join("helper");
            std::fs::write(
                &executable,
                format!("#!/bin/sh\nprintf '%s' \"$$\" > child.pid\n{script}\n"),
            )
            .unwrap();
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
            Self(directory)
        }

        fn command(&self) -> Command {
            let mut command = Command::new(self.0.join("helper"));
            command
                .current_dir(&self.0)
                .stdin(Stdio::null())
                .stderr(Stdio::null());
            command
        }

        fn assert_reaped(&self) {
            let pid = std::fs::read_to_string(self.0.join("child.pid")).unwrap();
            let pid = pid.parse::<u32>().unwrap();
            assert!(!Path::new("/proc").join(pid.to_string()).exists());
        }
    }

    impl Drop for Helper {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn resolver_output_preserves_ipv4_ipv6_and_deduplicates_tcp_addresses() {
        let helper = Helper::new(concat!(
            "printf '127.0.0.1 STREAM local\\n127.0.0.1 DGRAM\\n127.0.0.1 RAW\\n'\n",
            "printf '192.0.2.1 SEQPACKET\\n192.0.2.2 99\\n192.0.2.3 FUTURE\\n'\n",
            "printf '::1 STREAM local\\n127.0.0.1 STREAM alias\\n'\n",
        ));
        let result = run(
            helper.command(),
            443,
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(
            result,
            [
                "127.0.0.1:443".parse().unwrap(),
                "[::1]:443".parse().unwrap()
            ]
        );
        helper.assert_reaped();
    }

    #[test]
    fn resolver_failures_and_native_faults_cannot_return_partial_success() {
        for ending in ["exit 7", "kill -ABRT $$"] {
            let helper = Helper::new(&format!("printf '127.0.0.1 STREAM\\n'\n{ending}"));
            let mut command = helper.command();
            if ending.contains("ABRT") {
                if !Path::new(PRLIMIT).is_file() {
                    continue;
                }
                command =
                    super::command(Path::new(PRLIMIT), &helper.0.join("helper"), "example", []);
                command.current_dir(&helper.0);
            }
            assert_eq!(
                run(command, 443, Instant::now() + Duration::from_secs(2))
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::NotFound
            );
            helper.assert_reaped();
        }
    }

    #[test]
    fn slow_or_detached_outputs_are_killed_and_reaped_at_the_deadline() {
        for script in ["exec /bin/sleep 10", "exec 1>&-\nexec /bin/sleep 10"] {
            let helper = Helper::new(script);
            let started = Instant::now();
            let error =
                run(helper.command(), 443, started + Duration::from_millis(250)).unwrap_err();
            assert!(matches!(
                error.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ));
            assert!(started.elapsed() < Duration::from_secs(2));
            helper.assert_reaped();
        }
    }

    #[test]
    fn a_full_address_prefix_cancels_the_helper_without_waiting_for_more_results() {
        let helper = Helper::new(concat!(
            "n=1\nwhile test \"$n\" -le 20; do\n",
            "printf '192.0.2.%s STREAM\\n' \"$n\"\nn=$((n+1))\ndone\n",
            "exec /bin/sleep 10\n",
        ));
        let started = Instant::now();
        let result = run(helper.command(), 80, started + Duration::from_secs(2)).unwrap();
        assert_eq!(result.len(), MAX_ADDRESSES);
        assert_eq!(result[0], "192.0.2.1:80".parse().unwrap());
        assert_eq!(result[MAX_ADDRESSES - 1], "192.0.2.16:80".parse().unwrap());
        assert!(started.elapsed() < Duration::from_secs(1));
        helper.assert_reaped();
    }

    #[test]
    fn repeated_tcp_results_obey_the_same_address_limit() {
        let helper = Helper::new(concat!(
            "n=0\nwhile test \"$n\" -lt 16; do\n",
            "printf '127.0.0.1 STREAM\\n'\nn=$((n+1))\ndone\n",
            "exec /bin/sleep 10\n",
        ));
        let result = run(
            helper.command(),
            80,
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(result, ["127.0.0.1:80".parse().unwrap()]);
        helper.assert_reaped();
    }

    #[test]
    fn malformed_and_oversized_resolver_output_is_bounded() {
        for script in [
            "printf 'not-an-address STREAM\\n'",
            "printf '127.0.0.1\\n'",
            "printf '127.0.0.1 STREAM'",
            "n=0; while test \"$n\" -lt 8192; do printf 'abcdefgh'; n=$((n+1)); done",
        ] {
            let helper = Helper::new(script);
            assert_eq!(
                run(
                    helper.command(),
                    80,
                    Instant::now() + Duration::from_secs(2)
                )
                .unwrap_err()
                .kind(),
                io::ErrorKind::InvalidData
            );
            helper.assert_reaped();
        }
    }

    #[test]
    fn scoped_ipv6_output_preserves_numeric_interface_ids() {
        assert_eq!(
            address(b"fe80::1%42 STREAM", 443).unwrap(),
            Some(SocketAddr::V6(SocketAddrV6::new(
                "fe80::1".parse().unwrap(),
                443,
                0,
                42
            )))
        );
        for value in [
            b"fe80::1%../lo STREAM".as_slice(),
            b"fe80::1%.. STREAM",
            b"fe80::1% STREAM",
            b"fe80::1%4294967296 STREAM",
            b"fe80::1%lo STREAM",
        ] {
            assert!(address(value, 80).is_err());
        }
    }

    #[test]
    fn resolver_command_keeps_dns_settings_without_credentials_or_argument_interpretation() {
        if !Path::new(PRLIMIT).is_file() {
            return;
        }
        let helper = Helper::new(concat!(
            "test \"$1\" = --no-addrconfig || exit 1\n",
            "test \"$2\" = --no-idn || exit 2\n",
            "test \"$3\" = -- || exit 3\ntest \"$4\" = ahosts || exit 4\n",
            "test \"$5\" = '-option$(false)' || exit 5\n",
            "test \"$LOCALDOMAIN\" = 'example.test other.test' || exit 6\n",
            "test \"$LC_ALL\" = C || exit 7\n",
            "test -z \"${DISCORD_BOT_TOKEN+x}\" || exit 8\n",
            "test -z \"${PRIVATE_TOKEN_ENV+x}\" || exit 9\n",
            "test -z \"${LD_PRELOAD+x}\" || exit 10\n",
            "test \"$(ulimit -c)\" = 0 || exit 11\n",
            "printf '127.0.0.1 STREAM\\n'\n",
        ));
        let environment = [
            ("LOCALDOMAIN", "example.test other.test"),
            ("DISCORD_BOT_TOKEN", "not-forwarded"),
            ("PRIVATE_TOKEN_ENV", "not-forwarded"),
            ("LD_PRELOAD", "/not-forwarded"),
            ("LC_ALL", "not-forwarded"),
        ]
        .map(|(key, value)| (OsString::from(key), OsString::from(value)));
        let mut command = super::command(
            Path::new(PRLIMIT),
            &helper.0.join("helper"),
            "-option$(false)",
            environment,
        );
        command.current_dir(&helper.0);
        assert_eq!(
            run(command, 80, Instant::now() + Duration::from_secs(2)).unwrap(),
            ["127.0.0.1:80".parse().unwrap()]
        );
        helper.assert_reaped();
    }

    #[test]
    fn installed_system_helpers_resolve_local_addresses() {
        if let Some(result) = try_resolve("localhost", 443, Instant::now() + Duration::from_secs(2))
        {
            let addresses = result.unwrap();
            assert!(!addresses.is_empty());
            assert!(addresses
                .iter()
                .all(|address| address.ip().is_loopback() && address.port() == 443));
        }
    }
}
