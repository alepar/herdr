//! Splits the herdr frame stream into whole frames (u32-LE length prefix) and encodes
//! injected `x-mosh.link.v1` frames with the existing `crate::protocol` frame writer.
//! Does not own status semantics.

use crate::client::mosh_outage::predict::ECHO_SUBSCRIBE_KIND;
use crate::client::mosh_outage::server_hooks::SURFACE_RESYNC_KIND;
use crate::client::mosh_outage::{LinkStatus, LINK_KIND};
use crate::protocol::{
    surface_delta, surface_reuse, surface_scroll, write_message, ClientMessage, ServerMessage,
    MAX_GRAPHICS_FRAME_SIZE,
};

const PREFIX: usize = 4;

/// Advertised by remotes that support render split + resync (status line: mosh-19g.2.2).
pub(super) const SUPERSEDE_CAPABILITY: &str = "x-mosh-supersede-v1";

/// `ServerMessage` variant tags (bincode varint, one byte below 251).
const TAG_SURFACE_FRAME: u8 = 13;
const TAG_SURFACE_DELTA: u8 = 19;
const TAG_ENDPOINT_CONTROL: u8 = 20;

/// How a server frame may be treated on the render split: dropped freely, droppable only
/// together with the frames chained to it, or never dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(super) enum FrameClass {
    SelfContained,
    Chained,
    Reliable,
}

#[cfg_attr(not(test), allow(dead_code))]
impl FrameClass {
    /// Classify one whole frame as `FrameSplitter` yields it (4-byte LE length + payload).
    /// Anything short, unparseable or unrecognised is `Reliable`.
    pub fn of(frame: &[u8]) -> FrameClass {
        let Some(payload) = frame.get(PREFIX..) else {
            return FrameClass::Reliable;
        };
        match payload.split_first() {
            Some((&TAG_SURFACE_FRAME, _)) => FrameClass::SelfContained,
            Some((&TAG_SURFACE_DELTA, _)) => FrameClass::Chained,
            Some((&TAG_ENDPOINT_CONTROL, rest)) => {
                match bincode::serde::decode_from_slice::<String, _>(
                    rest,
                    bincode::config::standard(),
                ) {
                    Ok((kind, _))
                        if kind == surface_reuse::MESSAGE_KIND
                            || kind == surface_delta::MESSAGE_KIND
                            || kind == surface_scroll::MESSAGE_KIND =>
                    {
                        FrameClass::Chained
                    }
                    _ => FrameClass::Reliable,
                }
            }
            _ => FrameClass::Reliable,
        }
    }

    /// The render-stream header class byte.
    pub fn wire_byte(self) -> u8 {
        match self {
            FrameClass::SelfContained => 0,
            FrameClass::Chained => 1,
            FrameClass::Reliable => 2,
        }
    }

    pub fn from_wire_byte(byte: u8) -> Option<FrameClass> {
        match byte {
            0 => Some(FrameClass::SelfContained),
            1 => Some(FrameClass::Chained),
            2 => Some(FrameClass::Reliable),
            _ => None,
        }
    }
}

/// A frame prefix claimed more than `MAX_GRAPHICS_FRAME_SIZE`. Session-ending.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct OversizedFrame {
    pub claimed: usize,
    pub max: usize,
}

/// Incremental u32-LE length-prefix splitter (the `wire.rs` framing). Never interprets
/// payloads. After `push` returns `Err` the splitter is unusable: the session ends.
pub(super) struct FrameSplitter {
    buf: Vec<u8>,
    frames_seen: u64,
}

impl FrameSplitter {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            frames_seen: 0,
        }
    }

    /// Append bytes and return every frame now complete (each = 4-byte prefix + payload,
    /// verbatim).
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Vec<u8>>, OversizedFrame> {
        self.buf.extend_from_slice(bytes);
        let mut frames = Vec::new();
        let mut cursor = 0;
        while let Some(prefix) = self.buf[cursor..].first_chunk::<PREFIX>() {
            let claimed = u32::from_le_bytes(*prefix) as usize;
            if claimed > MAX_GRAPHICS_FRAME_SIZE {
                return Err(OversizedFrame {
                    claimed,
                    max: MAX_GRAPHICS_FRAME_SIZE,
                });
            }
            let end = cursor + PREFIX + claimed;
            if self.buf.len() < end {
                break;
            }
            frames.push(self.buf[cursor..end].to_vec());
            cursor = end;
        }
        self.buf.drain(..cursor);
        self.frames_seen += frames.len() as u64;
        Ok(frames)
    }

    /// Bytes of the trailing partial frame (0 at a boundary); bounded by one max frame.
    pub fn held_back(&self) -> usize {
        self.buf.len()
    }

    pub fn at_boundary(&self) -> bool {
        self.buf.is_empty()
    }

    /// Complete frames yielded so far.
    pub fn frames_seen(&self) -> u64 {
        self.frames_seen
    }

    /// Server EOF: discard the trailing partial frame and return its length.
    pub fn drop_partial(&mut self) -> usize {
        let n = self.buf.len();
        self.buf.clear();
        n
    }
}

/// The `x-mosh.link.v1` frame for one status, via the existing frame writer.
pub(super) fn encode_link_frame(status: &LinkStatus) -> Vec<u8> {
    let data = serde_json::to_string(status).expect("link frame encodes");
    let mut out = Vec::new();
    write_message(
        &mut out,
        &ServerMessage::EndpointControl {
            kind: LINK_KIND.to_owned(),
            data,
        },
    )
    .expect("link frame encodes");
    out
}

/// The client's request for a full `PaneSurface`, via the existing frame writer.
pub(super) fn encode_resync_frame() -> Vec<u8> {
    let mut out = Vec::new();
    write_message(
        &mut out,
        &ClientMessage::EndpointControl {
            kind: SURFACE_RESYNC_KIND.to_owned(),
            data: "{}".to_owned(),
        },
    )
    .expect("resync frame encodes");
    out
}

/// The client's one-time request that the server acknowledge echoed input
/// (`x-mosh.echo-subscribe.v1`), via the existing frame writer.
pub(super) fn encode_echo_subscribe_frame() -> Vec<u8> {
    let mut out = Vec::new();
    write_message(
        &mut out,
        &ClientMessage::EndpointControl {
            kind: ECHO_SUBSCRIBE_KIND.to_owned(),
            data: "{}".to_owned(),
        },
    )
    .expect("echo subscribe frame encodes");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::endpoint::{decode_endpoint_control, EndpointControlMessage};
    use crate::protocol::read_message;

    fn fixture() -> (Vec<u8>, usize) {
        let msgs = [
            ServerMessage::EndpointControl {
                kind: "a".into(),
                data: String::new(),
            },
            ServerMessage::EndpointControl {
                kind: "k".into(),
                data: "x".repeat(200 * 1024),
            },
            ServerMessage::EndpointControl {
                kind: "b".into(),
                data: "hello".into(),
            },
        ];
        let mut stream = Vec::new();
        for m in &msgs {
            write_message(&mut stream, m).unwrap();
        }
        (stream, msgs.len())
    }

    fn feed(chunks: impl Iterator<Item = Vec<u8>>) -> (Vec<Vec<u8>>, FrameSplitter) {
        let mut s = FrameSplitter::new();
        let mut out = Vec::new();
        for c in chunks {
            out.extend(s.push(&c).unwrap());
        }
        (out, s)
    }

    #[test]
    fn byte_at_a_time_reproduces_frames() {
        let (stream, n) = fixture();
        let (frames, s) = feed(stream.iter().map(|b| vec![*b]));
        assert_eq!(frames.len(), n);
        assert_eq!(frames.concat(), stream);
        assert!(s.at_boundary());
        assert_eq!(s.frames_seen(), n as u64);
    }

    #[test]
    fn random_chunking_reproduces_frames() {
        let (stream, n) = fixture();
        for seed in 1..=50u64 {
            let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15);
            let mut pos = 0;
            let mut chunks = Vec::new();
            while pos < stream.len() {
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let len = 1 + ((x >> 33) as usize % 70000);
                let end = (pos + len).min(stream.len());
                chunks.push(stream[pos..end].to_vec());
                pos = end;
            }
            let (frames, s) = feed(chunks.into_iter());
            assert_eq!(frames.len(), n, "seed {seed}");
            assert_eq!(frames.concat(), stream, "seed {seed}");
            assert!(s.at_boundary());
        }
    }

    fn with_partial() -> (FrameSplitter, Vec<Vec<u8>>) {
        let (stream, _) = fixture();
        let first_len = PREFIX + u32::from_le_bytes(stream[..4].try_into().unwrap()) as usize;
        let mut s = FrameSplitter::new();
        let frames = s.push(&stream[..first_len + 10]).unwrap();
        (s, frames)
    }

    #[test]
    fn partial_held_back_and_reported() {
        let (s, frames) = with_partial();
        assert_eq!(frames.len(), 1);
        assert_eq!(s.held_back(), 10);
        assert!(!s.at_boundary());
        assert_eq!(s.frames_seen(), 1);
    }

    #[test]
    fn trailing_partial_droppable() {
        let (mut s, _) = with_partial();
        assert_eq!(s.drop_partial(), 10);
        assert!(s.at_boundary());
        assert_eq!(s.held_back(), 0);
    }

    #[test]
    fn oversize_rejected() {
        let mut s = FrameSplitter::new();
        let prefix = ((MAX_GRAPHICS_FRAME_SIZE + 1) as u32).to_le_bytes();
        assert_eq!(s.push(&prefix[..3]), Ok(vec![]));
        assert_eq!(
            s.push(&prefix[3..]),
            Err(OversizedFrame {
                claimed: MAX_GRAPHICS_FRAME_SIZE + 1,
                max: MAX_GRAPHICS_FRAME_SIZE
            })
        );
    }

    #[test]
    fn exact_max_accepted_header() {
        let mut s = FrameSplitter::new();
        let prefix = (MAX_GRAPHICS_FRAME_SIZE as u32).to_le_bytes();
        assert_eq!(s.push(&prefix), Ok(vec![]));
        assert_eq!(s.held_back(), 4);
    }

    #[test]
    fn link_frame_decodes_as_ignored() {
        for status in [
            LinkStatus::Resuming { last_contact_ms: 1500 },
            LinkStatus::Linked { srtt_ms: None, echo: false },
            LinkStatus::Linked { srtt_ms: Some(80), echo: false },
        ] {
            let bytes = encode_link_frame(&status);
            let msg: ServerMessage =
                read_message(&mut &bytes[..], MAX_GRAPHICS_FRAME_SIZE).unwrap();
            let ServerMessage::EndpointControl { kind, data } = msg else {
                panic!("not EndpointControl");
            };
            assert_eq!(kind, LINK_KIND);
            assert_eq!(serde_json::from_str::<LinkStatus>(&data).unwrap(), status);
            assert!(matches!(
                decode_endpoint_control(&kind, &data),
                Ok(EndpointControlMessage::Ignored)
            ));
            let (frames, s) = feed(std::iter::once(bytes.clone()));
            assert_eq!(frames, vec![bytes]);
            assert!(s.at_boundary());
        }
    }

    fn control_frame(kind: &str) -> Vec<u8> {
        let mut out = Vec::new();
        write_message(
            &mut out,
            &ServerMessage::EndpointControl {
                kind: kind.into(),
                data: "{}".into(),
            },
        )
        .unwrap();
        out
    }

    #[test]
    fn every_tag_is_classified() {
        for tag in 0u8..=19 {
            let mut frame = 8u32.to_le_bytes().to_vec();
            frame.push(tag);
            frame.extend([0u8; 7]);
            let want = match tag {
                13 => FrameClass::SelfContained,
                19 => FrameClass::Chained,
                _ => FrameClass::Reliable,
            };
            assert_eq!(FrameClass::of(&frame), want, "tag {tag}");
        }
        // Tag 20 with an undecodable body is Reliable; tags above 20 never occur.
        let mut frame = 2u32.to_le_bytes().to_vec();
        frame.extend([20, 0xff]);
        assert_eq!(FrameClass::of(&frame), FrameClass::Reliable);
        // Guard: the single-byte varint tag assumption holds for the real encoder.
        assert_eq!(control_frame("k")[PREFIX], 20);
    }

    #[test]
    fn render_kinds_are_chained() {
        for kind in [
            surface_reuse::MESSAGE_KIND,
            surface_delta::MESSAGE_KIND,
            surface_scroll::MESSAGE_KIND,
        ] {
            assert_eq!(FrameClass::of(&control_frame(kind)), FrameClass::Chained, "{kind}");
        }
    }

    #[test]
    fn other_control_kinds_are_reliable() {
        for kind in [
            "shell.snapshot.v1",
            "x-mosh.echo-ack.v1",
            LINK_KIND,
            "x-mosh.anything.v1",
        ] {
            assert_eq!(FrameClass::of(&control_frame(kind)), FrameClass::Reliable, "{kind}");
        }
    }

    #[test]
    fn truncated_and_garbage_are_reliable() {
        for len in 0..=4 {
            assert_eq!(FrameClass::of(&vec![0u8; len]), FrameClass::Reliable, "len {len}");
        }
        let full = control_frame(surface_delta::MESSAGE_KIND);
        // Tag byte only, then a string cut short of its claimed length.
        for cut in [PREFIX + 1, PREFIX + 2, PREFIX + 1 + 1 + surface_delta::MESSAGE_KIND.len() - 1] {
            assert_eq!(FrameClass::of(&full[..cut]), FrameClass::Reliable, "cut {cut}");
        }
        assert_eq!(
            FrameClass::of(&[9, 0, 0, 0, 0xfb, 0xfc, 0xfd, 0xfe, 0xff]),
            FrameClass::Reliable
        );
    }

    #[test]
    fn wire_byte_round_trips() {
        for class in [FrameClass::SelfContained, FrameClass::Chained, FrameClass::Reliable] {
            assert_eq!(FrameClass::from_wire_byte(class.wire_byte()), Some(class));
        }
        assert_eq!(FrameClass::from_wire_byte(3), None);
    }

    #[test]
    fn supersede_capability_name() {
        assert_eq!(SUPERSEDE_CAPABILITY, "x-mosh-supersede-v1");
    }

    #[test]
    fn encode_resync_frame_decodes_as_client_endpoint_control() {
        let bytes = encode_resync_frame();
        let msg: ClientMessage = read_message(&mut &bytes[..], MAX_GRAPHICS_FRAME_SIZE).unwrap();
        let ClientMessage::EndpointControl { kind, .. } = msg else {
            panic!("not EndpointControl");
        };
        assert_eq!(kind, SURFACE_RESYNC_KIND);
    }

    #[test]
    fn echo_subscribe_frame_decodes() {
        let bytes = encode_echo_subscribe_frame();
        let msg: ClientMessage = read_message(&mut &bytes[..], MAX_GRAPHICS_FRAME_SIZE).unwrap();
        let ClientMessage::EndpointControl { kind, data } = msg else {
            panic!("not EndpointControl");
        };
        assert_eq!(kind, ECHO_SUBSCRIBE_KIND);
        assert_eq!(data, "{}");
        let frames = FrameSplitter::new().push(&bytes).unwrap();
        assert_eq!(frames, vec![bytes], "one whole frame");
    }

    #[test]
    fn splitter_splits_client_frames_across_chunking() {
        use crate::protocol::ClientPaneInputEvent;
        let mut input = Vec::new();
        write_message(
            &mut input,
            &ClientMessage::ClientShellPaneInput {
                pane_id: "w1:p2".into(),
                events: vec![ClientPaneInputEvent::TextCommit("hello".into())],
            },
        )
        .unwrap();
        let frames = vec![encode_resync_frame(), input, encode_resync_frame()];
        let stream = frames.concat();
        for chunk in [1usize, 3, 7, 64, stream.len()] {
            let (got, s) = feed(stream.chunks(chunk).map(<[u8]>::to_vec));
            assert_eq!(got, frames, "chunk {chunk}");
            assert!(s.at_boundary());
        }
    }
}
