//! A pseudo-terminal for an agent's own interactive UI, for the owner's
//! takeover of a conversation ([`super::takeover`]).
//!
//! [`spawn`] opens a terminal and starts the agent on it as the leader of a
//! session of its own, so the terminal is its controlling terminal: a
//! resize reaches it as `SIGWINCH`, and when the master side closes (SCV
//! stops it, or the SCV process that holds it dies) the kernel hangs the
//! terminal up and sends it `SIGHUP`. The leader's process group is its
//! session's, so stopping the group stops what it started in the terminal.
//! Flow control (`Ctrl+S`) starts off, so a key cannot freeze the agent's
//! output, and terminal stop signals (`Ctrl+Z`) never reach it: its group
//! has no parent in its session, which makes the kernel discard them.
//!
//! One thread per terminal ([`Pty::run_io`]) does all of its I/O, with
//! `select(2)`, which works on pseudo-terminals everywhere (macOS's kqueue
//! and poll do not): it reads whatever the agent writes, so the agent never
//! blocks on its output, and writes typed input as the agent takes it, so
//! a query's answer never lands inside a half-written paste. Callers queue
//! input ([`Pty::type_input`]) and wait for it up to a deadline; what the
//! agent has not taken by then is dropped.

use std::{
    collections::VecDeque,
    ffi::OsString,
    io,
    os::{
        fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd},
        unix::process::CommandExt as _,
    },
    path::Path,
    process::Stdio,
    sync::Mutex as StdMutex,
    time::Duration,
};

use tokio::{process::Command, sync::Notify, time::Instant};

use crate::{process::apply_agent_environment, sync::lock};

/// How long the I/O thread waits before it checks whether to stop.
const IO_WAIT: Duration = Duration::from_millis(500);
/// The most output one read takes.
const READ_CHUNK: usize = 16 * 1024;

/// The master side of a pseudo-terminal, and its I/O.
pub(crate) struct Pty {
    master: OwnedFd,
    /// A byte here wakes the I/O thread: input waits, or it should stop.
    wake_read: OwnedFd,
    wake_write: OwnedFd,
    io: StdMutex<Io>,
    /// Woken when input is written, or the terminal's I/O ends.
    progressed: Notify,
}

impl std::fmt::Debug for Pty {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Pty")
            .field("master", &self.master.as_raw_fd())
            .finish_non_exhaustive()
    }
}

/// What waits to be written to the terminal.
#[derive(Debug, Default)]
struct Io {
    /// Chunks in the order they are written; the first may be part written.
    queue: VecDeque<Chunk>,
    /// How much of the first chunk is written.
    offset: usize,
    /// The input chunks written whole, by number: they complete in order.
    done: u64,
    next: u64,
    /// The I/O thread ended: nothing more is read or written.
    closed: bool,
    stop: bool,
}

#[derive(Debug)]
struct Chunk {
    /// Typed input's number; `None` for SCV's answer to a query.
    input: Option<u64>,
    bytes: Vec<u8>,
}

/// A process started on its own terminal.
pub(crate) struct Spawned {
    pub(crate) child: tokio::process::Child,
    pub(crate) pid: u32,
    pub(crate) pty: Pty,
}

/// What to start on a terminal.
pub(crate) struct PtySpec<'a> {
    pub(crate) executable: &'a Path,
    pub(crate) args: &'a [String],
    pub(crate) cwd: &'a Path,
    /// Set after inherited agent credentials and state locations are
    /// removed ([`apply_agent_environment`]).
    pub(crate) environment: &'a [(OsString, OsString)],
    pub(crate) cols: u16,
    pub(crate) rows: u16,
}

/// `TIOCSCTTY` as `ioctl(2)` takes it here: Apple's headers type it
/// narrower than the request argument.
#[cfg(target_os = "linux")]
fn set_controlling_terminal() -> libc::Ioctl {
    libc::TIOCSCTTY
}

#[cfg(not(target_os = "linux"))]
fn set_controlling_terminal() -> libc::c_ulong {
    libc::c_ulong::from(libc::TIOCSCTTY)
}

/// Open a terminal of `spec`'s size and start `spec`'s process on it.
pub(crate) fn spawn(spec: &PtySpec<'_>) -> io::Result<Spawned> {
    let (master, slave) = open(spec.cols, spec.rows)?;
    let (wake_read, wake_write) = wake_pipe()?;
    for fd in [&master, &wake_read] {
        if usize::try_from(fd.as_raw_fd()).is_ok_and(|fd| fd >= libc::FD_SETSIZE) {
            return Err(io::Error::other(
                "SCV holds too many open files to watch another terminal",
            ));
        }
    }
    let request = set_controlling_terminal();
    let mut command = Command::new(spec.executable);
    apply_agent_environment(command.as_std_mut(), spec.environment);
    command
        .args(spec.args)
        .current_dir(spec.cwd)
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave))
        .kill_on_drop(true);
    // SAFETY: the closure runs in the forked child before exec and calls
    // only setsid(2) and ioctl(2), which are async-signal-safe, on the
    // child's own standard input (the terminal's slave side, dup'ed there
    // by `Stdio`); it touches no memory shared with the parent.
    unsafe {
        command.as_std_mut().pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(0, request, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    // The command, which holds the slave's descriptors, drops after the
    // spawn: only the child keeps the slave side open, so reads end once it
    // and its descendants have closed the terminal.
    let child = command.spawn()?;
    drop(command);
    let pid = child
        .id()
        .ok_or_else(|| io::Error::other("the terminal's process has no pid"))?;
    Ok(Spawned {
        child,
        pid,
        pty: Pty {
            master,
            wake_read,
            wake_write,
            io: StdMutex::default(),
            progressed: Notify::new(),
        },
    })
}

/// Open a terminal pair: the master non-blocking, both closed on exec. On
/// Linux each descriptor is created closed on exec, so a process SCV starts
/// on another thread meanwhile never inherits one (holding the master would
/// let it read the agent's screen and type into it); elsewhere the flag is
/// set right after `openpty(3)`.
#[cfg(target_os = "linux")]
fn open(cols: u16, rows: u16) -> io::Result<(OwnedFd, OwnedFd)> {
    let flags = libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC;
    // SAFETY: open(2) of a NUL-terminated path with integer flags.
    let master = unsafe { libc::open(c"/dev/ptmx".as_ptr(), flags | libc::O_NONBLOCK) };
    if master < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: open succeeded, so it is an open descriptor nothing else owns.
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    // SAFETY: grantpt(3) takes our master's descriptor only.
    if unsafe { libc::grantpt(master.as_raw_fd()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: unlockpt(3) takes our master's descriptor only.
    if unsafe { libc::unlockpt(master.as_raw_fd()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let slave = open_slave(&master, flags)?;
    let size = winsize(cols, rows);
    // SAFETY: ioctl(2) TIOCSWINSZ reads the struct we own, for our
    // descriptor.
    if unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &raw const size) } < 0 {
        return Err(io::Error::last_os_error());
    }
    no_flow_control(&slave)?;
    Ok((master, slave))
}

/// The slave side of `master`, opened with `flags`: by the master itself
/// (`TIOCGPTPEER`, Linux 4.13 and later), else by its name.
#[cfg(target_os = "linux")]
fn open_slave(master: &OwnedFd, flags: libc::c_int) -> io::Result<OwnedFd> {
    // SAFETY: ioctl(2) TIOCGPTPEER takes integer flags and returns a new
    // descriptor, or -1.
    let slave = unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCGPTPEER, flags) };
    if slave < 0 {
        let mut name = [0 as libc::c_char; 128];
        // SAFETY: ptsname_r(3) writes a NUL-terminated name of at most the
        // buffer's length into the buffer we own.
        let failed = unsafe { libc::ptsname_r(master.as_raw_fd(), name.as_mut_ptr(), name.len()) };
        if failed != 0 {
            return Err(io::Error::from_raw_os_error(failed));
        }
        // SAFETY: open(2) of the NUL-terminated name ptsname_r wrote.
        let slave = unsafe { libc::open(name.as_ptr(), flags) };
        if slave < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: open succeeded, so it is an open descriptor nothing else
        // owns.
        return Ok(unsafe { OwnedFd::from_raw_fd(slave) });
    }
    // SAFETY: the ioctl succeeded, so it is an open descriptor nothing else
    // owns.
    Ok(unsafe { OwnedFd::from_raw_fd(slave) })
}

#[cfg(not(target_os = "linux"))]
fn open(cols: u16, rows: u16) -> io::Result<(OwnedFd, OwnedFd)> {
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let mut size = winsize(cols, rows);
    // SAFETY: openpty(3) writes one descriptor into each integer we pass
    // and reads the window size; the name and terminal attributes are null,
    // which asks for the system's defaults.
    let opened = unsafe {
        libc::openpty(
            &raw mut master,
            &raw mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut size,
        )
    };
    if opened != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openpty succeeded, so both are open descriptors owned by
    // nothing else; each `OwnedFd` closes its own.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    for fd in [&master, &slave] {
        set_flag(fd, libc::F_GETFD, libc::F_SETFD, libc::FD_CLOEXEC)?;
    }
    set_flag(&master, libc::F_GETFL, libc::F_SETFL, libc::O_NONBLOCK)?;
    no_flow_control(&slave)?;
    Ok((master, slave))
}

/// A non-blocking pipe, both ends closed on exec (created so on Linux).
fn wake_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut ends: [libc::c_int; 2] = [-1, -1];
    #[cfg(target_os = "linux")]
    // SAFETY: pipe2(2) writes two descriptors into the array we own.
    let made = unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
    #[cfg(not(target_os = "linux"))]
    // SAFETY: pipe(2) writes two descriptors into the array we own.
    let made = unsafe { libc::pipe(ends.as_mut_ptr()) };
    if made != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the pipe was made, so both are open descriptors owned by
    // nothing else; each `OwnedFd` closes its own.
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1])) };
    #[cfg(not(target_os = "linux"))]
    for fd in [&read, &write] {
        set_flag(fd, libc::F_GETFD, libc::F_SETFD, libc::FD_CLOEXEC)?;
        set_flag(fd, libc::F_GETFL, libc::F_SETFL, libc::O_NONBLOCK)?;
    }
    Ok((read, write))
}

/// Add `flag` to the descriptor flags `get` reads and `set` writes.
#[cfg(not(target_os = "linux"))]
fn set_flag(fd: &OwnedFd, get: libc::c_int, set: libc::c_int, flag: libc::c_int) -> io::Result<()> {
    // SAFETY: fcntl(2) on a descriptor we own, with integer arguments only.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), get) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { libc::fcntl(fd.as_raw_fd(), set, flags | flag) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Turn off XON/XOFF flow control, so `Ctrl+S` typed into the terminal is
/// a key and never stops the agent's output; and mark input UTF-8.
fn no_flow_control(slave: &OwnedFd) -> io::Result<()> {
    // SAFETY: `termios` is plain data; tcgetattr(3) fills it in.
    let mut attributes: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: tcgetattr writes into the struct we own, for our descriptor.
    if unsafe { libc::tcgetattr(slave.as_raw_fd(), &raw mut attributes) } != 0 {
        return Err(io::Error::last_os_error());
    }
    attributes.c_iflag &= !(libc::IXON | libc::IXOFF);
    attributes.c_iflag |= libc::IUTF8;
    // SAFETY: tcsetattr reads the struct we own, for our descriptor.
    if unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &raw const attributes) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn winsize(cols: u16, rows: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

/// Marks the terminal closed when its I/O ends, however it ends.
struct Closing<'a>(&'a Pty);

impl Drop for Closing<'_> {
    fn drop(&mut self) {
        let mut io = lock(&self.0.io);
        io.closed = true;
        io.queue.clear();
        io.offset = 0;
        drop(io);
        self.0.progressed.notify_waiters();
    }
}

/// What `select(2)` found ready.
struct Ready {
    readable: bool,
    writable: bool,
    woken: bool,
}

/// Read into `buffer` without blocking: `Some(0)` once the terminal is
/// closed, `None` when nothing waits.
fn read_now(fd: RawFd, buffer: &mut [u8]) -> io::Result<Option<usize>> {
    // SAFETY: read(2) into a buffer we own, at most its length.
    let read = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
    if read >= 0 {
        return Ok(Some(read.cast_unsigned()));
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        // Linux reports a terminal whose every slave descriptor is closed
        // as an I/O error: the agent is gone.
        Some(libc::EIO) => Ok(Some(0)),
        Some(code) if code == libc::EAGAIN || code == libc::EINTR => Ok(None),
        _ => Err(error),
    }
}

/// Write from `bytes` without blocking: `None` when nothing can go now.
fn write_now(fd: RawFd, bytes: &[u8]) -> io::Result<Option<usize>> {
    // SAFETY: write(2) from a buffer we own, at most its length.
    let wrote = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
    if wrote >= 0 {
        return Ok(Some(wrote.cast_unsigned()));
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(code) if code == libc::EAGAIN || code == libc::EINTR => Ok(None),
        _ => Err(error),
    }
}

impl Pty {
    /// Do the terminal's I/O until it closes or [`Pty::stop`] is called,
    /// on the calling thread, which it blocks: `output` takes everything
    /// the agent writes and returns what to answer it with (a query's
    /// reply), which goes before typed input not yet begun. Returns the
    /// error that ended it, if one did. However it ends, even by a panic,
    /// the terminal counts as closed: input waits no more.
    pub(crate) fn run_io(&self, mut output: impl FnMut(&[u8]) -> Vec<u8>) -> io::Result<()> {
        let _closing = Closing(self);
        let mut buffer = vec![0u8; READ_CHUNK];
        loop {
            let pending = {
                let io = lock(&self.io);
                if io.stop {
                    return Ok(());
                }
                !io.queue.is_empty()
            };
            let ready = self.wait(pending)?;
            if ready.woken {
                let mut drained = [0u8; 64];
                while matches!(
                    read_now(self.wake_read.as_raw_fd(), &mut drained),
                    Ok(Some(1..))
                ) {}
            }
            if ready.readable {
                match read_now(self.master.as_raw_fd(), &mut buffer)? {
                    Some(0) => return Ok(()),
                    Some(read) => {
                        let reply = output(&buffer[..read]);
                        if !reply.is_empty() {
                            let mut io = lock(&self.io);
                            // After a chunk part written, before the rest.
                            let at = usize::from(io.offset > 0).min(io.queue.len());
                            io.queue.insert(
                                at,
                                Chunk {
                                    input: None,
                                    bytes: reply,
                                },
                            );
                        }
                    }
                    None => {}
                }
            }
            if ready.writable {
                self.write_some()?;
            }
        }
    }

    /// Wait until the terminal has output, can take input (when `pending`),
    /// or the I/O thread is woken; at most [`IO_WAIT`].
    fn wait(&self, pending: bool) -> io::Result<Ready> {
        let master = self.master.as_raw_fd();
        let wake = self.wake_read.as_raw_fd();
        // SAFETY: `fd_set` is plain data, cleared and filled with the two
        // descriptors `spawn` checked are below `FD_SETSIZE`; select(2)
        // reads and writes only the sets and the timeout we own.
        unsafe {
            let mut read: libc::fd_set = std::mem::zeroed();
            let mut write: libc::fd_set = std::mem::zeroed();
            libc::FD_ZERO(&raw mut read);
            libc::FD_ZERO(&raw mut write);
            libc::FD_SET(master, &raw mut read);
            libc::FD_SET(wake, &raw mut read);
            if pending {
                libc::FD_SET(master, &raw mut write);
            }
            let mut timeout = libc::timeval {
                tv_sec: 0,
                tv_usec: libc::suseconds_t::try_from(IO_WAIT.as_micros()).unwrap_or(500_000),
            };
            let ready = libc::select(
                master.max(wake) + 1,
                &raw mut read,
                &raw mut write,
                std::ptr::null_mut(),
                &raw mut timeout,
            );
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    return Ok(Ready {
                        readable: false,
                        writable: false,
                        woken: false,
                    });
                }
                return Err(error);
            }
            Ok(Ready {
                readable: libc::FD_ISSET(master, &raw const read),
                writable: pending && libc::FD_ISSET(master, &raw const write),
                woken: libc::FD_ISSET(wake, &raw const read),
            })
        }
    }

    /// Write what the terminal takes of the first waiting chunk.
    fn write_some(&self) -> io::Result<()> {
        let mut io = lock(&self.io);
        let offset = io.offset;
        let Some(chunk) = io.queue.front() else {
            return Ok(());
        };
        let Some(wrote) = write_now(self.master.as_raw_fd(), &chunk.bytes[offset..])? else {
            return Ok(());
        };
        let complete = offset + wrote >= chunk.bytes.len();
        if !complete {
            io.offset = offset + wrote;
            return Ok(());
        }
        io.offset = 0;
        if let Some(Chunk {
            input: Some(number),
            ..
        }) = io.queue.pop_front()
        {
            io.done = number;
            drop(io);
            self.progressed.notify_waiters();
        }
        Ok(())
    }

    fn wake(&self) {
        // A full pipe already wakes the thread.
        let _ = write_now(self.wake_write.as_raw_fd(), b"!");
    }

    /// Ask the I/O thread to end.
    pub(crate) fn stop(&self) {
        lock(&self.io).stop = true;
        self.wake();
    }

    /// Type `bytes` into the terminal, waiting until `deadline` for the
    /// agent to take them. Returns how many it took; the rest is dropped,
    /// so keys typed into an agent that stopped reading never arrive late.
    pub(crate) async fn type_input(&self, bytes: &[u8], deadline: Instant) -> usize {
        if bytes.is_empty() {
            return 0;
        }
        let number = {
            let mut io = lock(&self.io);
            if io.closed || io.stop {
                return 0;
            }
            io.next += 1;
            let number = io.next;
            io.queue.push_back(Chunk {
                input: Some(number),
                bytes: bytes.to_vec(),
            });
            number
        };
        self.wake();
        loop {
            let progressed = self.progressed.notified();
            {
                let io = lock(&self.io);
                if io.done >= number {
                    return bytes.len();
                }
                if io.closed {
                    break;
                }
            }
            if tokio::time::timeout_at(deadline, progressed).await.is_err() {
                break;
            }
        }
        // Not taken in time: what was not written goes.
        let mut io = lock(&self.io);
        if io.done >= number {
            return bytes.len();
        }
        let Some(index) = io
            .queue
            .iter()
            .position(|chunk| chunk.input == Some(number))
        else {
            return 0;
        };
        let taken = if index == 0 {
            std::mem::take(&mut io.offset)
        } else {
            0
        };
        io.queue.remove(index);
        taken
    }

    /// Tell the terminal, and so the agent, its new size.
    pub(crate) fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        let size = winsize(cols, rows);
        // SAFETY: ioctl(2) TIOCSWINSZ reads the struct we own, for our
        // descriptor.
        if unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &raw const size) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
