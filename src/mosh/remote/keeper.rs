//! The detached process-lifetime keeper: token table, HELLO state table, server
//! connections, rings, drain policy, grace timers, leg stealing, ghost eviction and
//! lock-ordered idle exit. Never parses herdr frames. Filled by mosh-dqw.7.

use std::io;

/// `args` is the slice `remote::dispatch` receives (the `remote-client-bridge` arguments).
pub(super) fn run(args: &[String]) -> io::Result<()> {
    let _ = args;
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "mosh keeper not implemented",
    ))
}
