//! Server-side pipe creation with an explicit security descriptor.

use std::ffi::c_void;
use std::io;

use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};

/// SYSTEM and Administrators: full access. Authenticated users: read/write.
/// The service runs as SYSTEM, so without this the app (a normal user) could not write requests.
const PIPE_SDDL: &str = "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;AU)";

/// Creates one pipe instance. Pass `first = true` for the very first instance so that a second
/// service process fails instead of silently sharing the name.
pub fn create_server(name: &str, first: bool) -> io::Result<NamedPipeServer> {
    let sddl: Vec<u16> = PIPE_SDDL.encode_utf16().chain(std::iter::once(0)).collect();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: `sddl` is NUL-terminated; `sd` receives a LocalAlloc'd descriptor freed below.
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        bInheritHandle: 0,
    };
    // SAFETY: `sa` and the descriptor it points to are valid for the duration of the call.
    let result = unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(name, &mut sa as *mut _ as *mut c_void)
    };
    // SAFETY: `sd` was allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
    unsafe { LocalFree(sd as _) };
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Client;
    use crate::frame::{read_frame, write_frame};
    use crate::protocol::{Envelope, Request, Response};

    #[tokio::test]
    async fn client_server_roundtrip() {
        let name = format!(r"\\.\pipe\seek-ipc-test-{}", uuid::Uuid::new_v4());
        let server = create_server(&name, true).unwrap();
        let srv = tokio::spawn(async move {
            let mut server = server;
            server.connect().await.unwrap();
            let req: Envelope<Request> = read_frame(&mut server).await.unwrap().unwrap();
            assert_eq!(req.body, Request::Status);
            write_frame(&mut server, &Envelope { id: req.id, body: Response::Ok }).await.unwrap();
        });
        let mut c = Client::connect_to(&name).await.unwrap();
        assert_eq!(c.request(Request::Status).await.unwrap(), Response::Ok);
        srv.await.unwrap();
    }
}
