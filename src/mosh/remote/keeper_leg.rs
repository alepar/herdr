//! The keeper's transport-neutral accepted leg.
//!
//! Owns: the leg's boxed reader and writer, a shutdown closure, the optional render-lane
//! slot and the optional silence-timeout override.
//! Hides: which transport the leg arrived on (a Unix stream today).
//! Must not: carry session state; the keeper's table owns that.
//! Lifecycle: built by a producer after it read the leg's first message, consumed by
//! `admit`; the reader stays with `run_leg`, the writer moves to `leg_writer`, and the
//! shutdown closure lives on in the session's `LegSlot` until the leg is dropped.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

/// Placeholder for the render-lane queue handle; mosh-19g.2.8 replaces this type (same field).
pub(super) struct RenderLaneSlot;

/// Closes the leg; callable from any thread (steal, silence verdict, session end).
pub(super) type LegShutdown = Arc<dyn Fn() + Send + Sync>;

pub(super) struct KeeperLeg {
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
    pub shutdown: LegShutdown,
    /// Always `None` until mosh-19g.2.8 gives it a consumer.
    #[allow(dead_code)]
    pub render_lane: Option<RenderLaneSlot>,
    /// `None` = Phase 1 silence timeout; QUIC legs set a longer window.
    pub silence_timeout: Option<Duration>,
}

impl KeeperLeg {
    /// A leg over an accepted Unix stream: reads and writes on clones, shutdown on a third.
    pub(super) fn unix(stream: UnixStream) -> io::Result<KeeperLeg> {
        let writer = stream.try_clone()?;
        let closer = stream.try_clone()?;
        Ok(KeeperLeg {
            reader: Box::new(stream),
            writer: Box::new(writer),
            shutdown: Arc::new(move || {
                let _ = closer.shutdown(Shutdown::Both);
            }),
            render_lane: None,
            silence_timeout: None,
        })
    }
}
