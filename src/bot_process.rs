use std::io::{self, Read, Write};
use std::ops::{Deref, DerefMut};
use std::process::{Child, Command, ExitStatus, Stdio};
#[cfg(not(unix))]
use std::process::{ChildStdin, ChildStdout};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::{fd::OwnedFd, unix::net::UnixStream};

pub(super) struct ManagedChild(Child);

impl ManagedChild {
    pub(super) fn new(child: Child) -> Self {
        Self(child)
    }
}

impl Deref for ManagedChild {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.0
    }
}

impl DerefMut for ManagedChild {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

pub(super) enum BotInput {
    #[cfg(unix)]
    Socket {
        stream: UnixStream,
        timeout: Duration,
        deadline: Instant,
    },
    #[cfg(not(unix))]
    Pipe(ChildStdin),
}

pub(super) enum BotOutput {
    #[cfg(unix)]
    Socket(UnixStream),
    #[cfg(not(unix))]
    Pipe(ChildStdout),
}

pub(super) struct BotReadInterrupt {
    #[cfg(unix)]
    stream: UnixStream,
}

impl BotReadInterrupt {
    pub(super) fn stop(&self) {
        #[cfg(unix)]
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

impl BotOutput {
    pub(super) fn interrupt_handle(&self) -> io::Result<Option<BotReadInterrupt>> {
        #[cfg(unix)]
        {
            let Self::Socket(stream) = self;
            Ok(Some(BotReadInterrupt {
                stream: stream.try_clone()?,
            }))
        }
        #[cfg(not(unix))]
        Ok(None)
    }
}

impl Read for BotOutput {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        match self {
            #[cfg(unix)]
            Self::Socket(stream) => stream.read(bytes),
            #[cfg(not(unix))]
            Self::Pipe(pipe) => pipe.read(bytes),
        }
    }
}

pub(super) fn prepare_output(command: &mut Command) -> io::Result<Option<BotOutput>> {
    #[cfg(unix)]
    {
        let (stream, peer) = UnixStream::pair()?;
        command.stdout(Stdio::from(OwnedFd::from(peer)));
        Ok(Some(BotOutput::Socket(stream)))
    }
    #[cfg(not(unix))]
    {
        command.stdout(Stdio::piped());
        Ok(None)
    }
}

pub(super) fn take_output(prepared: Option<BotOutput>, child: &mut Child) -> io::Result<BotOutput> {
    #[cfg(unix)]
    {
        let _ = child;
        prepared.ok_or_else(|| io::Error::other("Bot IPC output missing"))
    }
    #[cfg(not(unix))]
    {
        let _ = prepared;
        child
            .stdout
            .take()
            .map(BotOutput::Pipe)
            .ok_or_else(|| io::Error::other("Bot IPC output missing"))
    }
}

impl BotInput {
    pub(super) fn begin_response(&mut self) -> io::Result<()> {
        #[cfg(unix)]
        {
            let Self::Socket {
                timeout, deadline, ..
            } = self;
            *deadline = response_deadline(*timeout)?;
        }
        Ok(())
    }
}

pub(super) fn prepare_input(
    command: &mut Command,
    timeout: Duration,
) -> io::Result<Option<BotInput>> {
    #[cfg(unix)]
    {
        let deadline = response_deadline(timeout)?;
        let (stream, peer) = UnixStream::pair()?;
        command.stdin(Stdio::from(OwnedFd::from(peer)));
        Ok(Some(BotInput::Socket {
            stream,
            timeout,
            deadline,
        }))
    }
    #[cfg(not(unix))]
    {
        let _ = timeout;
        command.stdin(Stdio::piped());
        Ok(None)
    }
}

pub(super) fn take_input(prepared: Option<BotInput>, child: &mut Child) -> io::Result<BotInput> {
    #[cfg(unix)]
    {
        let _ = child;
        prepared.ok_or_else(|| io::Error::other("Bot IPC input missing"))
    }
    #[cfg(not(unix))]
    {
        let _ = prepared;
        child
            .stdin
            .take()
            .map(BotInput::Pipe)
            .ok_or_else(|| io::Error::other("Bot IPC input missing"))
    }
}

impl Write for BotInput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            #[cfg(unix)]
            Self::Socket {
                stream, deadline, ..
            } => {
                let remaining = deadline
                    .checked_duration_since(Instant::now())
                    .filter(|remaining| !remaining.is_zero())
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::TimedOut, "Bot IPC response timed out")
                    })?;
                stream.set_write_timeout(Some(remaining))?;
                stream.write(bytes)
            }
            #[cfg(not(unix))]
            Self::Pipe(pipe) => pipe.write(bytes),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            Self::Socket { stream, .. } => stream.flush(),
            #[cfg(not(unix))]
            Self::Pipe(pipe) => pipe.flush(),
        }
    }
}

fn response_deadline(timeout: Duration) -> io::Result<Instant> {
    Instant::now()
        .checked_add(timeout)
        .filter(|_| !timeout.is_zero())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Bot IPC timeout is out of range",
            )
        })
}

pub(super) fn wait_for_exit(child: &mut Child, timeout: Duration) -> io::Result<ExitStatus> {
    let deadline = response_deadline(timeout)?;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            child.kill()?;
            child.wait()?;
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Bot did not exit after closing IPC",
            ));
        }
        std::thread::sleep(
            Duration::from_millis(10).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;

    fn sleeping_child() -> ManagedChild {
        ManagedChild::new(
            Command::new("sh")
                .args(["-c", "exec sleep 30"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        )
    }

    #[test]
    fn dropping_a_managed_child_stops_and_reaps_it() {
        let child = sleeping_child();
        let pid = child.id();
        drop(child);
        assert!(!Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success());
    }

    #[test]
    fn a_child_which_does_not_exit_is_killed_after_the_grace_period() {
        let mut child = sleeping_child();
        let started = Instant::now();
        assert_eq!(
            wait_for_exit(&mut child, Duration::from_millis(20))
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(child.try_wait().unwrap().is_some());
    }

    #[test]
    fn an_unread_response_cannot_block_past_its_deadline() {
        let mut command = Command::new("unused");
        let mut input = prepare_input(&mut command, Duration::from_millis(20))
            .unwrap()
            .unwrap();
        input.begin_response().unwrap();
        let started = Instant::now();
        let error = input.write_all(&vec![b'x'; 1024 * 1024]).unwrap_err();
        assert!(matches!(
            error.kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn one_deadline_covers_all_partial_response_writes() {
        let mut command = Command::new("unused");
        let mut input = prepare_input(&mut command, Duration::from_millis(10))
            .unwrap()
            .unwrap();
        input.begin_response().unwrap();
        input.write_all(b"partial").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            input.write_all(b"remaining").unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }
}
