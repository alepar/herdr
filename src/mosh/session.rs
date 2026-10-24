//! Session wire protocol, per-direction offset and ack state machine, flow-control rule,
//! link timers and hello/welcome negotiation. Pure: operates on buffers, no sockets.
//! Does not own leg lifecycle or ring storage policy.

/// HELLO/WELCOME `v`; keeper rendezvous name input.
pub(super) const SESSION_PROTOCOL_VERSION: u32 = 1;
