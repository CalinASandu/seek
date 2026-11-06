//! The named pipe the job and its result travel on. No file holds either, so the elevated process
//! never opens a path a user-owned folder could redirect.
//!
//! The window makes the pipe (one instance, `FILE_FLAG_FIRST_PIPE_INSTANCE`, so no one else can
//! have made it first; a DACL that grants the current user alone) and the elevated copy connects
//! to it as a client that cannot be impersonated, then checks which process is serving. The
//! window in turn checks that the client is the very process it started, and writes nothing to
//! any other.
//!
//! A same-user process that gets in the way can do very little. It can connect first, and is
//! then disconnected without being sent anything (it is not the process the window started). It
//! can make the elevated copy talk to it only by being the server, and the elevated copy refuses,
//! because the server's process id is not the window the job names. Neither lets it remove
//! anything the rules would not. Every wait is bounded.

use std::fs::File;
use std::io::{Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, HANDLE, HLOCAL, INVALID_HANDLE_VALUE, LocalFree};
use windows::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId, GetNamedPipeServerProcessId, WaitNamedPipeW,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::IO::CancelSynchronousIo;

const PREFIX: &str = r"\\.\pipe\seek-clean-";
/// The most a client that is connected before the window knows which process it started waits for
/// that to be known (the prompt may be open that long: the process does not exist until it is
/// answered). It is then judged by process id like any other. Never longer than this in total.
const PID_WAIT: Duration = Duration::from_secs(15 * 60);
/// Clients turned away before the server gives up.
const MAX_CONNECTS: u32 = 32;
/// How long the elevated copy tries to connect, waiting for a free instance.
const CONNECT_WINDOW: Duration = Duration::from_secs(30);
/// How long the elevated copy waits to write its result.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

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

/// A security descriptor from the system, freed once.
struct Descriptor(PSECURITY_DESCRIPTOR);

impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW, freed only here.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0 .0)));
        }
    }
}

/// `D:P(A;;GA;;;<current user>)`: full control for the current user, a protected DACL (nothing
/// inherited), and no entry for Everyone, Anonymous or anyone else.
fn user_only_descriptor() -> std::io::Result<Descriptor> {
    let sid = super::account::current_user_sid().map_err(std::io::Error::other)?;
    let sddl = wide(&format!("D:P(A;;GA;;;{sid})"));
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: `sddl` is NUL-terminated; revision 1 is SDDL_REVISION_1.
    unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(PCWSTR(sddl.as_ptr()), 1, &mut sd, None) }.map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(Descriptor(sd))
}

/// The window's end: one instance, both directions.
pub struct Server {
    file: File,
}

impl Server {
    /// Fails if a pipe of this name already exists.
    pub fn create(name: &str) -> std::io::Result<Server> {
        let w = wide(name);
        let sd = user_only_descriptor()?;
        let attrs = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd.0 .0, bInheritHandle: false.into() };
        // SAFETY: `w` is NUL-terminated and `attrs` points at a descriptor that both outlive the call.
        let h = unsafe {
            CreateNamedPipeW(
                PCWSTR(w.as_ptr()),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                65536,
                65536,
                0,
                Some(&attrs),
            )
        };
        if h == INVALID_HANDLE_VALUE || h.is_invalid() {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: a fresh handle we own; the File closes it.
        Ok(Server { file: unsafe { File::from_raw_handle(h.0) } })
    }

    pub(crate) fn handle(&self) -> HANDLE {
        HANDLE(self.file.as_raw_handle())
    }

    /// Waits for the client that is the process the window started, whose id arrives on
    /// `started`; a client that is anyone else is disconnected without being sent anything.
    /// Then sends it `job` (length first) and reads what it answers until it closes the pipe, up
    /// to `max_reply` bytes. Blocks until the right client comes (see `finish`).
    pub fn serve(mut self, job: &[u8], max_reply: u64, started: mpsc::Receiver<u32>) -> Result<Vec<u8>, String> {
        let h = self.handle();
        let mut want = None;
        let mut connected = false;
        for _ in 0..MAX_CONNECTS {
            // SAFETY: a valid pipe handle; ERROR_PIPE_CONNECTED means the client was already there.
            if let Err(e) = unsafe { ConnectNamedPipe(h, None) } {
                if e.code() != ERROR_PIPE_CONNECTED.to_hresult() {
                    return Err(e.to_string());
                }
            }
            let mut client = 0u32;
            // SAFETY: a valid pipe handle and out pointer.
            unsafe { GetNamedPipeClientProcessId(h, &mut client) }.map_err(|e| e.to_string())?;
            let pid = match want {
                Some(p) => Some(p),
                None => match started.recv_timeout(PID_WAIT) {
                    Ok(p) => {
                        want = Some(p);
                        want
                    }
                    // The launch ended without a process (or the process has exited): nobody is the one.
                    Err(mpsc::RecvTimeoutError::Disconnected) => return Err("no process was started".into()),
                    Err(mpsc::RecvTimeoutError::Timeout) => return Err("the process that was started never came".into()),
                },
            };
            if pid == Some(client) {
                connected = true;
                break;
            }
            // Someone else: nothing is sent. Back to waiting for the right one.
            // SAFETY: a valid, connected pipe handle.
            let _ = unsafe { DisconnectNamedPipe(h) };
        }
        if !connected {
            return Err("the process that was started never connected".into());
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

    /// The ACCESS_ALLOWED entries of the pipe's DACL as SID text, and the number of entries of
    /// any kind. For tests.
    #[cfg(test)]
    pub(crate) fn dacl(&self) -> Result<(Vec<String>, u32), String> {
        use std::ffi::c_void;
        use windows::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
        use windows::Win32::Security::{GetAce, GetAclInformation, AclSizeInformation, ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, DACL_SECURITY_INFORMATION, PSID};
        let mut acl: *mut ACL = std::ptr::null_mut();
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: valid handle and out pointers; the descriptor is freed below.
        let err = unsafe { GetSecurityInfo(self.handle(), SE_KERNEL_OBJECT, DACL_SECURITY_INFORMATION, None, None, Some(&mut acl), None, Some(&mut sd)) };
        if err.0 != 0 {
            return Err(format!("GetSecurityInfo failed ({})", err.0));
        }
        let _free = Descriptor(sd);
        let mut info = ACL_SIZE_INFORMATION::default();
        // SAFETY: `acl` came from the call above; `info` is the size the class wants.
        unsafe { GetAclInformation(acl, (&mut info as *mut ACL_SIZE_INFORMATION).cast::<c_void>(), std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32, AclSizeInformation) }
            .map_err(|e| e.to_string())?;
        let mut sids = Vec::new();
        for i in 0..info.AceCount {
            let mut ace: *mut c_void = std::ptr::null_mut();
            // SAFETY: an index below AceCount of a valid ACL.
            unsafe { GetAce(acl, i, &mut ace) }.map_err(|e| e.to_string())?;
            // SAFETY: `ace` points at an ACE, which starts with its header.
            if unsafe { (*ace.cast::<ACE_HEADER>()).AceType } == 0 {
                // SAFETY: type 0 is ACCESS_ALLOWED_ACE; its SID starts at SidStart.
                let sid = unsafe { PSID(std::ptr::addr_of_mut!((*ace.cast::<ACCESS_ALLOWED_ACE>()).SidStart).cast()) };
                sids.push(super::account::sid_text(sid)?);
            }
        }
        Ok((sids, info.AceCount))
    }
}

/// Waits for a serving thread to end. Once the process it serves has exited it should end at
/// once; after `grace` its blocking call (waiting for a client, reading, writing) is cancelled
/// every few milliseconds, and after `deadline` the thread is let go. `None` means it did not
/// end: it never blocks the caller for longer than `deadline`.
pub fn finish<T>(handle: JoinHandle<T>, grace: Duration, deadline: Duration) -> Option<T> {
    let start = Instant::now();
    loop {
        if handle.is_finished() {
            return handle.join().ok();
        }
        let waited = start.elapsed();
        if waited >= deadline {
            return None;
        }
        if waited >= grace {
            // SAFETY: the thread handle stays valid while `handle` is held; this only cancels a
            // synchronous call the thread is blocked in, and does nothing if there is none.
            let _ = unsafe { CancelSynchronousIo(HANDLE(handle.as_raw_handle())) };
        }
        std::thread::sleep(Duration::from_millis(10));
    }
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
        let end = Instant::now() + CONNECT_WINDOW;
        let w = wide(name);
        let mut last = String::new();
        while Instant::now() < end {
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
                // Another client holds the instance (one that will be turned away, or a first try
                // before the server is listening again): wait for a free one, up to a second at a time.
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32) => {
                    last = e.to_string();
                    // SAFETY: `w` is NUL-terminated.
                    let _ = unsafe { WaitNamedPipeW(PCWSTR(w.as_ptr()), 1000) };
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

    /// Writes the answer and closes the pipe. More than `max` bytes is refused; a server that
    /// doesn't read within `timeout` is given up on, so this never blocks for good.
    pub fn reply_within(self, bytes: &[u8], max: u64, timeout: Duration) -> Result<(), String> {
        if bytes.len() as u64 > max {
            return Err("The answer is too big to send.".into());
        }
        let mut file = self.file.try_clone().map_err(|e| e.to_string())?;
        let bytes = bytes.to_vec();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(file.write_all(&bytes).and_then(|_| file.flush()).map_err(|e| e.to_string()));
        });
        rx.recv_timeout(timeout).map_err(|_| "The window did not read the answer in time.".to_string())?
    }
}
