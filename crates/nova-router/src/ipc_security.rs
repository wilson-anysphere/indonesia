use crate::Result;
use anyhow::{anyhow, Context};
use std::path::Path;

pub(crate) fn generate_auth_token() -> Result<String> {
    // 256-bit random token (hex encoded) used as a shared secret between the router and workers.
    // This is primarily a defense-in-depth measure for local IPC transports.
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|err| anyhow!("generate auth token: {err}"))?;
    Ok(hex_encode(&bytes))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(unix)]
pub(crate) fn ensure_unix_socket_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    let Some(parent) = path.parent() else {
        return Ok(());
    };
    // `Path::parent()` can return an empty component for relative paths like `router.sock`.
    if parent.as_os_str().is_empty() {
        return Ok(());
    }

    if parent.exists() {
        let meta = std::fs::metadata(parent).with_context(|| format!("metadata {parent:?}"))?;
        if !meta.is_dir() {
            return Err(anyhow!("unix socket parent {parent:?} is not a directory"));
        }
        return Ok(());
    }

    // Create the directory with an explicit 0700 mode (still subject to umask, but umask can only
    // remove permissions and 0700 already grants none to group/other).
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    builder.mode(0o700);
    builder
        .create(parent)
        .with_context(|| format!("create socket dir {parent:?}"))?;

    // Explicitly chmod to 0700 so other users cannot traverse the directory to reach the socket
    // path (and to correct permissions if the FS/umask created something more permissive).
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod socket dir {parent:?} to 0700"))?;
    Ok(())
}

#[cfg(unix)]
pub(crate) fn restrict_unix_socket_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    // `UnixListener::bind` creates the socket path with permissions derived from the process umask
    // (typically 0777 & !umask). We immediately chmod to 0600 to restrict access to the owning
    // user.
    //
    // Note: there is an unavoidable race window between `bind()` creating the path and this chmod.
    // For the strongest protection in shared environments, place the socket in a private (0700)
    // directory.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod unix socket {path:?} to 0600"))?;
    Ok(())
}

#[cfg(windows)]
pub(crate) fn create_secure_named_pipe_server(
    name: &str,
    first_pipe_instance: bool,
) -> Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{FromRawHandle, RawHandle};

    use tokio::net::windows::named_pipe::NamedPipeServer;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED,
    };
    use windows_sys::Win32::System::Memory::LocalFree;
    use windows_sys::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_ACCESS_DUPLEX, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
        PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };

    // Restrict access to LocalSystem + the pipe owner (the user running the router).
    //
    // `OW` ("OWNER RIGHTS") grants access to the object's owner without embedding a specific SID.
    // This keeps the implementation small while still ensuring other local users cannot connect.
    //
    // Note: this is about local multi-tenant safety, not full authentication. If workers are
    // started externally, use `DistributedRouterConfig::auth_token` as an application-layer guard.
    let sddl = OsStr::new("D:P(A;;GA;;;SY)(A;;GA;;;OW)");
    let mut sddl_w: Vec<u16> = sddl.encode_wide().collect();
    sddl_w.push(0);

    let mut sd = std::ptr::null_mut();
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl_w.as_ptr(),
            SDDL_REVISION_1 as u32,
            &mut sd,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error()).context("convert SDDL security descriptor");
    }

    let mut sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        bInheritHandle: 0,
    };

    let mut name_w: Vec<u16> = OsStr::new(name).encode_wide().collect();
    name_w.push(0);

    let mut open_mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
    if first_pipe_instance {
        open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    let pipe_mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;

    let handle = unsafe {
        CreateNamedPipeW(
            name_w.as_ptr(),
            open_mode,
            pipe_mode,
            PIPE_UNLIMITED_INSTANCES,
            64 * 1024,
            64 * 1024,
            0,
            &mut sa,
        )
    };

    unsafe {
        // `CreateNamedPipeW` makes its own copy of the security descriptor; we can free ours.
        LocalFree(sd as isize);
    }

    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("CreateNamedPipeW({name}) failed"));
    }

    // SAFETY: `handle` is a newly-created pipe handle which we transfer to Tokio for async IO.
    let server = unsafe { NamedPipeServer::from_raw_handle(handle as RawHandle) };
    Ok(server)
}
