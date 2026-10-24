//! A bounded replay ring keyed by absolute byte offset: `push`, `ack(offset)`,
//! `replay_from(offset)`. Overflow is reported, never silently dropped. Does not decide
//! what to do on overflow (the caller does).
