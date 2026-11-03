//! Linux pty plumbing: one master file, one child shell.
//!
//! Port of `io/terminal/pty.go`. The ioctls are Linux's because that is where
//! the engine runs.

use std::fs::File;
use std::io::{Error as IoError, ErrorKind, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

/// A shell running on a pseudo-terminal, holding the master side.
pub struct Pty {
    master: Arc<File>,
    pid: i32,
    child: Arc<Mutex<Option<Child>>>,
    exit: Arc<AtomicI32>,
    dead: Arc<AtomicBool>,
}

impl Pty {
    /// Opens a pty and starts `program` on it, sized to `cols` x `rows`.
    pub fn start(
        program: &str,
        args: &[String],
        cols: u16,
        rows: u16,
        dir: Option<&str>,
    ) -> std::io::Result<Pty> {
        let (master, slave) = open_pty()?;

        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        if let Some(dir) = dir {
            command.current_dir(dir);
        }
        command.env("TERM", "xterm-256color");
        unsafe {
            use std::os::unix::process::CommandExt;
            command.pre_exec(|| {
                // A new session makes the shell a process-group leader, and
                // TIOCSCTTY on its own stdin gives it the controlling terminal
                // that job control and Ctrl-C need.
                if libc::setsid() < 0 {
                    return Err(IoError::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(IoError::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn()?;
        let pid = child.id() as i32;
        let child = Arc::new(Mutex::new(Some(child)));
        let exit = Arc::new(AtomicI32::new(0));
        let dead = Arc::new(AtomicBool::new(false));

        let pty = Pty {
            master: Arc::new(master),
            pid,
            child: child.clone(),
            exit: exit.clone(),
            dead: dead.clone(),
        };
        pty.resize(cols, rows)?;

        // A separate waiter reaps the shell even if nobody is reading the pty.
        let waited = exit.clone();
        let is_dead = dead.clone();
        std::thread::spawn(move || {
            let status = {
                let mut slot = child.lock().unwrap_or_else(|e| e.into_inner());
                match slot.as_mut() {
                    Some(child) => child.wait().ok(),
                    None => None,
                }
            };
            let code = match status {
                Some(status) => status.code().unwrap_or(-1),
                None => -1,
            };
            waited.store(code, Ordering::SeqCst);
            is_dead.store(true, Ordering::SeqCst);
        });

        Ok(pty)
    }

    pub fn pid(&self) -> i32 {
        self.pid
    }

    pub fn master(&self) -> Arc<File> {
        self.master.clone()
    }

    pub fn dead(&self) -> bool {
        self.dead.load(Ordering::SeqCst)
    }

    pub fn exit_code(&self) -> i32 {
        self.exit.load(Ordering::SeqCst)
    }

    /// Reads the master side. The fd is non-blocking, so this reports
    /// `WouldBlock` when there is nothing to read yet.
    pub fn read(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut file = &*self.master;
        file.read(buf)
    }

    pub fn write(&self, data: &[u8]) -> std::io::Result<()> {
        let mut file = &*self.master;
        match file.write_all(data) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == ErrorKind::WouldBlock => Ok(()),
            Err(err) => Err(err),
        }
    }

    pub fn resize(&self, cols: u16, rows: u16) -> std::io::Result<()> {
        let size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let rc = unsafe {
            libc::ioctl(
                self.master.as_raw_fd(),
                libc::TIOCSWINSZ as _,
                &size as *const libc::winsize,
            )
        };
        if rc < 0 {
            return Err(IoError::last_os_error());
        }
        Ok(())
    }

    /// Sends SIGWINCH to the foreground process group so it repaints.
    pub fn kick(&self) -> std::io::Result<()> {
        let mut pgrp: libc::c_int = 0;
        let rc = unsafe {
            libc::ioctl(
                self.master.as_raw_fd(),
                libc::TIOCGPGRP as _,
                &mut pgrp as *mut libc::c_int,
            )
        };
        if rc < 0 {
            return Err(IoError::last_os_error());
        }
        if pgrp <= 0 {
            return Ok(());
        }
        unsafe {
            libc::kill(-pgrp, libc::SIGWINCH);
        }
        Ok(())
    }

    /// Hangs the pty up and kills the shell's process group. The master file is
    /// closed when the last handle drops, not here: the reader still holds an
    /// fd, and closing it twice could close another file's number.
    pub fn close(&self) {
        unsafe {
            libc::kill(-self.pid, libc::SIGKILL);
        }
        if let Some(child) = self
            .child
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
        {
            let _ = child.kill();
        }
    }
}

fn open_pty() -> std::io::Result<(File, File)> {
    let master = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open("/dev/ptmx")?;
    unlock_pty(master.as_raw_fd())?;
    let number = pty_number(master.as_raw_fd())?;
    let slave = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(format!("/dev/pts/{number}"))?;
    set_nonblocking(master.as_raw_fd())?;
    Ok((master, slave))
}

fn unlock_pty(fd: i32) -> std::io::Result<()> {
    let unlock: libc::c_int = 0;
    let rc = unsafe { libc::ioctl(fd, libc::TIOCSPTLCK as _, &unlock as *const libc::c_int) };
    if rc < 0 {
        return Err(IoError::last_os_error());
    }
    Ok(())
}

fn pty_number(fd: i32) -> std::io::Result<u32> {
    let mut number: libc::c_uint = 0;
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGPTN as _, &mut number as *mut libc::c_uint) };
    if rc < 0 {
        return Err(IoError::last_os_error());
    }
    Ok(number)
}

fn set_nonblocking(fd: i32) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(IoError::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(IoError::last_os_error());
    }
    Ok(())
}

/// The start time of a process from `/proc/<pid>/stat`, used with the pid to
/// tell a live shell from a reused pid.
pub fn process_start(pid: i32) -> Option<u64> {
    if pid <= 0 {
        return None;
    }
    let raw = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let cut = raw.rfind(')')?;
    let fields: Vec<&str> = raw[cut + 1..].split_whitespace().collect();
    if fields.len() < 20 {
        return None;
    }
    if matches!(fields[0], "Z" | "X") {
        return None;
    }
    let start: u64 = fields[19].parse().ok()?;
    (start != 0).then_some(start)
}

/// Whether `pid` is still the shell this session started.
pub fn process_alive(pid: i32, start: u64) -> bool {
    match process_start(pid) {
        Some(now) => start == 0 || now == start,
        None => false,
    }
}

/// The master file as a raw fd, for the reader's poll loop.
pub fn raw_fd(file: &File) -> i32 {
    file.as_raw_fd()
}
