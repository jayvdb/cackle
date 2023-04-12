//! This module handles wrapping and invocation of rustc, the linker and build.rs binaries.
//!
//! We always call through (proxy) to the real rustc and on the happy path, call the real linker.
//!
//! We wrap rustc for the following purposes:
//!
//! * So that we can add -Funsafe-code to all crates that aren't listed in cackle.toml as allowing
//!   unsafe code.
//! * So that we can override the linker with `-C linker=...`
//!
//! We wrap the linker so that:
//!
//! * We can get a list of all the objects and rlibs that are going to be linked and check that the
//!   rules in cackle.toml are satisfied.
//! * We can prevent the actual linker from being invoked if the rules aren't satisfied.
//! * We can put our binary in place of the output for build scripts so that we can proxy them.
//!
//! We wrap build.rs binaries so that:
//!
//! * We can run them inside a sandbox if the config says to do so.
//! * We can capture their output and check for any directives to cargo that haven't been permitted.

use crate::colour::Colour;
use anyhow::Context;
use anyhow::Result;
use std::fmt::Display;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::path::PathBuf;
use std::process;
use std::process::Command;
use std::thread::JoinHandle;
use std::time::Duration;

mod cargo;
mod errors;
pub(crate) mod rpc;
pub(crate) mod subprocess;

const SOCKET_ENV: &str = "CACKLE_SOCKET_PATH";
const CONFIG_PATH_ENV: &str = "CACKLE_CONFIG_PATH";
const ORIG_LINKER_ENV: &str = "CACKLE_ORIG_LINKER";

pub(crate) struct CargoBuildFailure {
    output: std::process::Output,
}

/// Invokes `cargo build` in the specified directory with us acting as proxy versions of rustc and
/// the linker. If calling this, you must call handle_wrapped_binaries from the start of main.
pub(crate) fn invoke_cargo_build(
    dir: &Path,
    config_path: &Path,
    colour: Colour,
    mut callback: impl FnMut(rpc::Request) -> rpc::CanContinueResponse,
) -> Result<Option<CargoBuildFailure>> {
    if !std::env::var(SOCKET_ENV).unwrap_or_default().is_empty() {
        panic!("{SOCKET_ENV} is already set. Missing call to handle_wrapped_binarie?");
    }
    let _ = std::fs::remove_file("/tmp/cackle.log");
    // For now, we always clean before we build. It might be possible to not do this, but we'd need
    // to carefully track changes to things we care about, like cackle.toml.
    run_command(&mut cargo::command("clean", dir, colour))?;

    let target_dir = dir.join("target");
    std::fs::create_dir_all(&target_dir)
        .with_context(|| format!("Failed to create directory `{}`", target_dir.display()))?;
    let ipc_path = target_dir.join("cackle.socket");
    let listener = UnixListener::bind(&ipc_path)
        .with_context(|| format!("Failed to create Unix socket `{}`", ipc_path.display()))?;

    let mut command = cargo::command("build", dir, colour);
    command
        .env(SOCKET_ENV, &ipc_path)
        .env(CONFIG_PATH_ENV, config_path)
        .env("RUSTC_WRAPPER", cackle_exe()?);

    let cargo_thread: JoinHandle<Result<process::Output>> =
        std::thread::spawn(move || -> Result<process::Output> {
            // TODO: Rather than collecting all output, we should read cargo's stdout/stderr as it
            // is emitted and pass it through to our stdout/stderr, but only until we encounter a
            // permissions problem - all output after that from cargo should be dropped.
            let output = command
                .output()
                .with_context(|| format!("Failed to run {command:?}"))?;
            Ok(output)
        });

    listener
        .set_nonblocking(true)
        .context("Failed to set socket to non-blocking")?;
    loop {
        if cargo_thread.is_finished() {
            // The following unwrap will only panic if the cargo thread panicked.
            let output = cargo_thread.join().unwrap()?;
            drop(listener);
            // Deleting the socket is best-effort only, so we don't report an error if we can't.
            let _ = std::fs::remove_file(&ipc_path);
            if output.status.code() != Some(0) {
                return Ok(Some(CargoBuildFailure { output }));
            }
            break;
        }
        // We need to concurrently accept connections from our proxy subprocesses and also check to
        // see if our main subprocess has terminated. It should be possible to do this without
        // polling... but it's so much simpler to just poll.
        if let Ok((mut connection, _)) = listener.accept() {
            let request: rpc::Request = rpc::read_from_stream(&mut connection)
                .context("Malformed request from subprocess")?;
            let response = (callback)(request);
            rpc::write_to_stream(&response, &mut connection)?;
        } else {
            // Avoid using too much CPU with our polling.
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    Ok(None)
}

fn run_command(command: &mut Command) -> Result<std::process::ExitStatus> {
    command
        .status()
        .with_context(|| format!("Failed to run {command:?}"))
}

fn cackle_exe() -> Result<PathBuf> {
    std::env::current_exe().context("Failed to get current exe")
}

impl Display for CargoBuildFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", String::from_utf8_lossy(&self.output.stdout))?;
        write!(f, "{}", String::from_utf8_lossy(&self.output.stderr))?;
        Ok(())
    }
}
