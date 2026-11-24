//! Opens a folder or URL with the desktop's default handler: `open` on
//! macOS, `xdg-open` elsewhere.

use std::ffi::OsStr;
use std::io;
use std::process::{Command, ExitStatus};
use std::thread;

use futures::channel::oneshot;

/// The program that hands a path or URL to the desktop.
pub const PROGRAM: &str = if cfg!(target_os = "macos") {
    "open"
} else {
    "xdg-open"
};

/// Starts [`PROGRAM`] on `target`. A thread of its own reaps it (xdg-open
/// may only exit with the application it started) and sends its exit
/// status to the returned receiver, which callers may simply drop.
pub fn open(target: &OsStr) -> io::Result<oneshot::Receiver<io::Result<ExitStatus>>> {
    let mut child = Command::new(PROGRAM).arg(target).spawn()?;
    let (tx, rx) = oneshot::channel();
    thread::Builder::new().name(PROGRAM.into()).spawn(move || {
        let _ = tx.send(child.wait());
    })?;
    Ok(rx)
}
