const START1: u8 = 0x94;
const START2: u8 = 0xC3;
const HEADER_LEN: usize = 4;
const MAX_PACKET_SIZE: usize = 512;

enum State {
    WaitStart1,
    WaitStart2,
    ReadLenHigh,
    ReadLenLow(u8),
    ReadPayload { expected: usize },
}

pub struct FrameReader {
    state: State,
    buf: Vec<u8>,
}

impl FrameReader {
    pub fn new() -> Self {
        Self {
            state: State::WaitStart1,
            buf: Vec::with_capacity(MAX_PACKET_SIZE + HEADER_LEN),
        }
    }

    /// Feed a single byte into the frame parser.
    /// Returns `Some(payload)` when a complete frame has been received.
    pub fn feed(&mut self, byte: u8) -> Option<Vec<u8>> {
        match self.state {
            State::WaitStart1 => {
                if byte == START1 {
                    self.state = State::WaitStart2;
                }
                None
            }
            State::WaitStart2 => {
                if byte == START2 {
                    self.state = State::ReadLenHigh;
                } else if byte == START1 {
                    // Another START1 — stay in WaitStart2
                } else {
                    self.state = State::WaitStart1;
                }
                None
            }
            State::ReadLenHigh => {
                self.state = State::ReadLenLow(byte);
                None
            }
            State::ReadLenLow(high) => {
                let len = ((high as usize) << 8) | (byte as usize);
                if len > MAX_PACKET_SIZE {
                    log::warn!("frame length {len} exceeds max {MAX_PACKET_SIZE}, resetting");
                    self.state = State::WaitStart1;
                    return None;
                }
                if len == 0 {
                    self.state = State::WaitStart1;
                    return Some(Vec::new());
                }
                self.buf.clear();
                self.state = State::ReadPayload { expected: len };
                None
            }
            State::ReadPayload { expected } => {
                self.buf.push(byte);
                if self.buf.len() >= expected {
                    self.state = State::WaitStart1;
                    Some(std::mem::take(&mut self.buf))
                } else {
                    None
                }
            }
        }
    }

    /// Feed a slice of bytes, returning all complete frames found.
    pub fn feed_bytes(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        for &byte in data {
            if let Some(payload) = self.feed(byte) {
                frames.push(payload);
            }
        }
        frames
    }
}

/// Wrap a protobuf payload in the serial framing header: [START1, START2, len_hi, len_lo, payload...]
pub fn frame_payload(payload: &[u8]) -> Vec<u8> {
    let len = payload.len();
    let mut out = Vec::with_capacity(HEADER_LEN + len);
    out.push(START1);
    out.push(START2);
    out.push((len >> 8) as u8);
    out.push((len & 0xFF) as u8);
    out.extend_from_slice(payload);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frame_payload_roundtrip() {
        let data = b"hello meshtastic";
        let framed = frame_payload(data);
        let mut reader = FrameReader::new();
        let frames = reader.feed_bytes(&framed);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0], data);
    }

    #[test]
    fn test_frame_reader_resyncs_on_garbage() {
        let data = b"\x01\x02\x03\xFF";
        let mut reader = FrameReader::new();

        // Feed garbage including trailing 0x94 (partial START1)
        let garbage = [0xAA, 0xBB, 0xCC, 0xDD, 0x94, 0x00, 0x94];
        let frames = reader.feed_bytes(&garbage);
        assert!(frames.is_empty());

        // Now feed a valid frame
        let framed = frame_payload(data);
        let frames = reader.feed_bytes(&framed);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0], data);
    }

    #[test]
    fn test_frame_reader_rejects_oversized() {
        let mut reader = FrameReader::new();
        // Frame header claiming 513 bytes (> MAX_PACKET_SIZE)
        let header = [START1, START2, 0x02, 0x01]; // length = 513
        let frames = reader.feed_bytes(&header);
        assert!(frames.is_empty());

        // Reader should have reset, so a valid frame should work
        let data = b"ok";
        let framed = frame_payload(data);
        let frames = reader.feed_bytes(&framed);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0], data);
    }

    #[test]
    fn test_frame_reader_multiple_frames() {
        let data1 = b"first";
        let data2 = b"second";
        let mut combined = frame_payload(data1);
        combined.extend_from_slice(&frame_payload(data2));

        let mut reader = FrameReader::new();
        let frames = reader.feed_bytes(&combined);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0], data1);
        assert_eq!(frames[1], data2);
    }

    #[test]
    fn test_frame_payload_empty() {
        let framed = frame_payload(b"");
        assert_eq!(framed, [START1, START2, 0x00, 0x00]);

        let mut reader = FrameReader::new();
        let frames = reader.feed_bytes(&framed);
        assert_eq!(frames.len(), 1);
        assert!(frames[0].is_empty());
    }

    #[test]
    fn test_frame_exact_max_size() {
        let data = vec![0xAB; MAX_PACKET_SIZE]; // exactly 512 bytes
        let framed = frame_payload(&data);
        let mut reader = FrameReader::new();
        let frames = reader.feed_bytes(&framed);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0], data);
    }

    #[test]
    fn test_frame_one_over_max() {
        let mut reader = FrameReader::new();
        // Header claiming 513 bytes
        let header = [START1, START2, 0x02, 0x01]; // length = 513
        let frames = reader.feed_bytes(&header);
        assert!(frames.is_empty());

        // Reader should have reset — verify it can still parse a valid frame
        let data = b"recovery";
        let framed = frame_payload(data);
        let frames = reader.feed_bytes(&framed);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0], data);
    }

    #[test]
    fn test_start_bytes_inside_payload() {
        // Payload that contains the START1+START2 sequence
        let data: Vec<u8> = vec![0x01, START1, START2, 0x02, START1, 0xFF];
        let framed = frame_payload(&data);
        let mut reader = FrameReader::new();
        let frames = reader.feed_bytes(&framed);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0], data);
    }

    #[test]
    fn test_garbage_between_valid_frames() {
        let data1 = b"first";
        let data2 = b"second";
        let mut stream = frame_payload(data1);
        stream.extend_from_slice(&[0xFF, 0xFE, 0xFD, 0x94, 0x00]); // garbage with partial START1
        stream.extend_from_slice(&frame_payload(data2));

        let mut reader = FrameReader::new();
        let frames = reader.feed_bytes(&stream);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0], data1);
        assert_eq!(frames[1], data2);
    }

    #[test]
    fn test_feed_vs_feed_bytes_equivalence() {
        let data1 = b"alpha";
        let data2 = b"beta";
        let mut stream = frame_payload(data1);
        stream.extend_from_slice(&[0xFF, 0x00]); // garbage
        stream.extend_from_slice(&frame_payload(data2));

        // feed_bytes
        let mut reader1 = FrameReader::new();
        let frames_bulk = reader1.feed_bytes(&stream);

        // feed one-at-a-time
        let mut reader2 = FrameReader::new();
        let mut frames_single = Vec::new();
        for &byte in &stream {
            if let Some(payload) = reader2.feed(byte) {
                frames_single.push(payload);
            }
        }

        assert_eq!(frames_bulk, frames_single);
    }

    #[test]
    fn test_frame_payload_oversized_input() {
        // frame_payload doesn't enforce the limit, but FrameReader should reject it
        let big = vec![0xCC; MAX_PACKET_SIZE + 1]; // 513 bytes
        let framed = frame_payload(&big);

        // Verify the header encodes length 513
        assert_eq!(framed[2], 0x02);
        assert_eq!(framed[3], 0x01);

        let mut reader = FrameReader::new();
        let frames = reader.feed_bytes(&framed);
        assert!(frames.is_empty(), "FrameReader should reject >512 byte frames");

        // Reader recovers for subsequent valid frames
        let data = b"ok";
        let framed = frame_payload(data);
        let frames = reader.feed_bytes(&framed);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0], data);
    }
}
