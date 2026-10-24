//! Splits the herdr frame stream into whole frames (u32-LE length prefix) and encodes
//! injected `x-mosh.link.v1` frames with the existing `crate::protocol` frame writer.
//! Does not own status semantics.
