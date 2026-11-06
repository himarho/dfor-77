use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[cfg(all(target_os = "linux", target_env = "gnu"))]
mod resolver;

static RESOLVERS: AtomicUsize = AtomicUsize::new(0);
const MAX_RESOLVERS: usize = 4;
const MAX_ADDRESSES: usize = 16;

struct ResolverSlot;

impl Drop for ResolverSlot {
    fn drop(&mut self) {
        RESOLVERS.fetch_sub(1, Ordering::AcqRel);
    }
}

fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "network deadline exceeded"))
}

fn resolve(host: &str, port: u16, deadline: Instant) -> io::Result<Vec<SocketAddr>> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    if host.eq_ignore_ascii_case("localhost") {
        return Ok(vec![
            SocketAddr::from(([127, 0, 0, 1], port)),
            SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], port)),
        ]);
    }
    let mut count = RESOLVERS.load(Ordering::Acquire);
    loop {
        if count >= MAX_RESOLVERS {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "resolver capacity exceeded",
            ));
        }
        match RESOLVERS.compare_exchange_weak(count, count + 1, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => break,
            Err(current) => count = current,
        }
    }
    let slot = ResolverSlot;
    let host = host.to_string();
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("dfor77-resolver".into())
        .spawn(move || {
            let _slot = slot;
            let result = system_resolve(&host, port, deadline);
            let _ = tx.send(result);
        })?;
    rx.recv_timeout(remaining(deadline)?)
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "DNS deadline exceeded"))?
}

fn system_resolve(host: &str, port: u16, deadline: Instant) -> io::Result<Vec<SocketAddr>> {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    if let Some(result) = resolver::try_resolve(host, port, deadline) {
        return result;
    }
    let _ = deadline;
    (host, port)
        .to_socket_addrs()
        .map(|addresses| addresses.take(MAX_ADDRESSES).collect())
}

#[derive(Debug)]
pub(crate) struct NetworkStream {
    tcp: TcpStream,
    deadline: Instant,
}

impl NetworkStream {
    pub(crate) fn connect(
        host: &str,
        port: u16,
        timeout: Duration,
        loopback_only: bool,
    ) -> io::Result<Self> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid deadline"))?;
        let addresses = resolve(host, port, deadline)?;
        for address in addresses {
            if loopback_only && !address.ip().is_loopback() {
                continue;
            }
            if let Ok(tcp) = TcpStream::connect_timeout(&address, remaining(deadline)?) {
                return Ok(Self { tcp, deadline });
            }
        }
        Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "connection failed",
        ))
    }

    pub(crate) fn renew_deadline(&mut self, timeout: Duration) {
        let now = Instant::now();
        self.deadline = now.checked_add(timeout).unwrap_or(now);
    }
}

impl Read for NetworkStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.tcp.set_read_timeout(Some(remaining(self.deadline)?))?;
        self.tcp.read(buffer)
    }
}

impl Write for NetworkStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.tcp
            .set_write_timeout(Some(remaining(self.deadline)?))?;
        self.tcp.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        remaining(self.deadline)?;
        self.tcp.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn slow_drip_cannot_extend_total_deadline() {
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = server.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = server.accept().unwrap();
            for _ in 0..100 {
                if stream.write_all(b"x").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        let start = Instant::now();
        let mut stream = NetworkStream::connect(
            "127.0.0.1",
            address.port(),
            Duration::from_millis(100),
            true,
        )
        .unwrap();
        let mut data = Vec::new();
        let error = stream.read_to_end(&mut data).unwrap_err();
        assert!(matches!(
            error.kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        ));
        assert!(start.elapsed() < Duration::from_secs(1));
        drop(stream);
        handle.join().unwrap();
    }

    #[test]
    fn loopback_connections_do_not_use_dns_and_expired_deadlines_fail() {
        assert!(resolve("localhost", 80, Instant::now())
            .unwrap()
            .iter()
            .all(|addr| addr.ip().is_loopback()));
        assert!(remaining(Instant::now()).is_err());
    }
}
