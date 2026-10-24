//! Locates or spawns the keeper, verifies the keeper's peer uid, pumps stdio and the keeper
//! socket, and reports its own bring-up failure as a session `END`. Beyond that one `END` it
//! is a dumb pump. Filled by mosh-dqw.10.

use std::io;

/// `args` is the slice `remote::dispatch` receives (the `remote-client-bridge` arguments).
pub(super) fn run(args: &[String]) -> io::Result<()> {
    let _ = args;
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "mosh attacher not implemented",
    ))
}
