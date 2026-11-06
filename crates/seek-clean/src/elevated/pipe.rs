//! The named pipe the job and its result travel on. No file holds either, so the elevated process
//! never opens a path a user-owned folder could redirect.
//!
//! The window makes the pipe (one instance, `FILE_FLAG_FIRST_PIPE_INSTANCE`, so no one else can
//! have made it first) and the elevated copy connects to it as a client that cannot be
//! impersonated, then checks which process is serving.
//!
//! A same-user process that gets in the way can do only two things. It can connect to the pipe
//! before the elevated copy does, and then it is served a job the rules still judge item by item
//! (or reads the result). Or it can answer in the window's place for the elevated copy, which
//! refuses, because the server's process id is not the window the job names. Neither lets it remove
//! anything the rules would not.

use std::fs::File;
use std::io::{Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::sync::mpsc;
use std::time::Duration;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeServerProcessId, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};

const PREFIX: &str = r"\\.\pipe\seek-clean-";

/// A fresh pipe name: `\\.\pipe\seek-clean-` and 128 random bits as 32 lowercase hex digits.
pub fn new_name() -> String {
    format!("{PREFIX}{:032x}", rand::random::<u128>())
}

/// True only for exactly that shape.
pub fn name_ok(name: &str) -> bool {
    name.strip_prefix(PREFIX).is_some_and(|h| h.len() == 32 && h.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')))
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The window's end: one instance, both directions.
pub struct Server {
    file: File,
}

impl Server {
    /// Fails if a pipe of this name already exists.
    pub fn create(name: &str) -> std::io::Result<Server> {
        let w = wide(name);
        // SAFETY: `w` is NUL-terminated and outlives the call.
        let h = unsafe {
            CreateNamedPipeW(
                PCWSTR(w.as_ptr()),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                65536,
                65536,
                0,
                None,
            )
        };
        if h == INVALID_HANDLE_VALUE || h.is_invalid() {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: a fresh handle we own; the File closes it.
        Ok(Server { file: unsafe { File::from_raw_handle(h.0) } })
    }

    /// Waits for one client, sends it `job` (length first), then reads what it answers until it
    /// closes the pipe, up to `max_reply` bytes. Blocks until a client comes (see `release`).
    pub fn serve(mut self, job: &[u8], max_reply: u64) -> Result<Vec<u8>, String> {
        let h = HANDLE(self.file.as_raw_handle());
        // SAFETY: a valid pipe handle; ERROR_PIPE_CONNECTED means the client was already there.
        if let Err(e) = unsafe { ConnectNamedPipe(h, None) } {
            if e.code() != ERROR_PIPE_CONNECTED.to_hresult() {
                return Err(e.to_string());
            }
        }
        let len = u32::try_from(job.len()).map_err(|_| "the job is too big".to_string())?;
        // A client that refuses the job (too big, say) answers and closes before it has read all of
        // it. What it wrote stays readable, so a failed write is only an error if nothing came back.
        let sent = self
            .file
            .write_all(&len.to_le_bytes())
            .and_then(|_| self.file.write_all(job))
            .and_then(|_| self.file.flush());
        let mut reply = Vec::new();
        let read = (&mut self.file).take(max_reply + 1).read_to_end(&mut reply);
        if reply.len() as u64 > max_reply {
            return Err("the reply is too big".into());
        }
        match (sent, read) {
            (_, Ok(_)) if !reply.is_empty() => Ok(reply),
            (Err(e), _) | (_, Err(e)) => Err(e.to_string()),
            (Ok(()), Ok(_)) => Ok(reply),
        }
    }
}

/// Ends a `serve` that is still waiting for a client that never came, by connecting to it.
/// Harmless when a client is already connected (the pipe is busy then).
pub fn release(name: &str) {
    let _ = std::fs::OpenOptions::new().read(true).write(true).open(name);
}

/// The elevated copy's end.
pub struct Client {
    file: File,
    /// The process that made the pipe.
    pub server_pid: u32,
}

impl Client {
    /// Connects without letting the server impersonate this process, and asks which process serves.
    pub fn connect(name: &str) -> Result<Client, String> {
        let mut last = String::new();
        for _ in 0..50 {
            let opened = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .security_qos_flags(SECURITY_SQOS_PRESENT.0 | SECURITY_IDENTIFICATION.0)
                .open(name);
            match opened {
                Ok(file) => {
                    let mut pid = 0u32;
                    // SAFETY: a valid pipe handle and out pointer.
                    unsafe { GetNamedPipeServerProcessId(HANDLE(file.as_raw_handle()), &mut pid) }.map_err(|e| e.to_string())?;
                    return Ok(Client { file, server_pid: pid });
                }
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32) => {
                    last = e.to_string();
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => return Err(e.to_string()),
            }
        }
        Err(last)
    }

    /// Reads the job: its length, then that many bytes. A length over `max`, or no answer within
    /// `timeout`, is an error.
    pub fn read_job(&self, max: u64, timeout: Duration) -> Result<Vec<u8>, String> {
        let mut file = self.file.try_clone().map_err(|e| e.to_string())?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut len = [0u8; 4];
            let got = file.read_exact(&mut len).map_err(|e| e.to_string()).and_then(|_| {
                let n = u64::from(u32::from_le_bytes(len));
                if n > max {
                    return Err("The job is too big, so nothing was removed.".to_string());
                }
                let mut bytes = vec![0u8; n as usize];
                file.read_exact(&mut bytes).map_err(|e| e.to_string())?;
                Ok(bytes)
            });
            let _ = tx.send(got);
        });
        rx.recv_timeout(timeout).map_err(|_| "No job arrived in time, so nothing was removed.".to_string())?
    }

    /// Writes the answer and closes the pipe.
    pub fn reply(mut self, bytes: &[u8]) -> Result<(), String> {
        self.file.write_all(bytes).and_then(|_| self.file.flush()).map_err(|e| e.to_string())
    }
}
