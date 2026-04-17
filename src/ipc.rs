//! IPC via Unix domain socket.
//!
//! The daemon listens on `$XDG_RUNTIME_DIR/niri-wallpaper.sock`.
//! The same binary can act as a client via subcommands (`next`, `set`, `current`).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};

use crate::IpcCommand;

/// Resolve the socket path.
pub fn socket_path() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return Ok(PathBuf::from(dir).join("niri-wallpaper.sock"));
    }
    let uid = unsafe { libc::getuid() };
    Ok(PathBuf::from(format!("/tmp/niri-wallpaper-{}.sock", uid)))
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Run the IPC client: connect to the daemon, send a command, print the response.
pub fn run_client(cmd: IpcCommand) -> Result<()> {
    let path = socket_path()?;
    let mut stream = UnixStream::connect(&path)
        .with_context(|| format!("connect to {} — is the daemon running?", path.display()))?;

    let line = match cmd {
        IpcCommand::Next => "next\n".to_string(),
        IpcCommand::Set(p) => format!("set {}\n", p.display()),
        IpcCommand::Current => "current\n".to_string(),
    };
    stream.write_all(line.as_bytes()).context("send command")?;
    stream.flush()?;

    // Shut down the write half so the daemon knows we're done sending.
    stream.shutdown(std::net::Shutdown::Write)?;

    let mut response = String::new();
    BufReader::new(&stream).read_line(&mut response).context("read response")?;
    let response = response.trim_end();

    if response.starts_with("error ") {
        return Err(anyhow!("{}", &response["error ".len()..]));
    }

    if !response.is_empty() {
        println!("{}", response);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// Create the Unix listener, removing any stale socket file first.
pub fn create_listener() -> Result<UnixListener> {
    let path = socket_path()?;

    // Remove stale socket from a previous run.
    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("remove stale socket {}", path.display()))?;
    }

    let listener = UnixListener::bind(&path)
        .with_context(|| format!("bind {}", path.display()))?;
    listener.set_nonblocking(true).context("set_nonblocking")?;
    log::info!("IPC listening on {}", path.display());
    Ok(listener)
}

/// Best-effort cleanup of the socket file on shutdown.
pub fn cleanup_socket() {
    if let Ok(path) = socket_path() {
        let _ = std::fs::remove_file(&path);
    }
}

/// Parsed IPC request from a client.
pub enum IpcRequest {
    Next,
    Set(PathBuf),
    Current,
}

/// Read a single command from an accepted client connection.
pub fn read_request(stream: &UnixStream) -> Result<IpcRequest> {
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).context("read IPC command")?;
    let line = line.trim();

    if line == "next" {
        Ok(IpcRequest::Next)
    } else if line == "current" {
        Ok(IpcRequest::Current)
    } else if let Some(rest) = line.strip_prefix("set ") {
        let path = rest.trim();
        if path.is_empty() {
            anyhow::bail!("set command requires a path");
        }
        Ok(IpcRequest::Set(PathBuf::from(path)))
    } else {
        anyhow::bail!("unknown IPC command: {:?}", line);
    }
}

/// Write a response line to the client.
pub fn write_response(stream: &mut UnixStream, response: &str) {
    let msg = format!("{}\n", response);
    if let Err(e) = stream.write_all(msg.as_bytes()) {
        log::debug!("IPC write response failed: {}", e);
    }
}

/// Write an error response to the client.
pub fn write_error(stream: &mut UnixStream, err: &str) {
    write_response(stream, &format!("error {}", err));
}
