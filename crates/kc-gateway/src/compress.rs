//! `zlib-stream` transport compression, as the web client uses it in Chrome:
//! one zlib stream runs for the whole connection, and each gateway message
//! ends in a sync flush (`00 00 ff ff`). A message may span several frames.

use flate2::{Decompress, FlushDecompress, Status};

const SYNC_FLUSH: [u8; 4] = [0x00, 0x00, 0xff, 0xff];

pub struct ZlibStream {
    inflate: Decompress,
    pending: Vec<u8>,
}

impl Default for ZlibStream {
    fn default() -> Self {
        Self::new()
    }
}

impl ZlibStream {
    pub fn new() -> Self {
        Self {
            inflate: Decompress::new(true),
            pending: Vec::new(),
        }
    }

    /// Feeds one binary frame. Returns the message's JSON once its last frame
    /// has arrived, `Ok(None)` while it is still incomplete.
    pub fn feed(&mut self, frame: &[u8]) -> Result<Option<String>, String> {
        self.pending.extend_from_slice(frame);
        if !self.pending.ends_with(&SYNC_FLUSH) {
            return Ok(None);
        }
        let input = std::mem::take(&mut self.pending);
        let mut output = Vec::with_capacity(input.len() * 6 + 1024);
        let mut offset = 0;
        loop {
            if output.capacity() - output.len() < 16 * 1024 {
                output.reserve(output.capacity().max(64 * 1024));
            }
            let (read_before, written_before) = (self.inflate.total_in(), output.len());
            let status = self
                .inflate
                .decompress_vec(&input[offset..], &mut output, FlushDecompress::Sync)
                .map_err(|error| error.to_string())?;
            offset += (self.inflate.total_in() - read_before) as usize;
            let progressed =
                self.inflate.total_in() != read_before || output.len() != written_before;
            // Done when all input is used and the output was not cut short.
            let room_left = output.len() < output.capacity();
            if matches!(status, Status::StreamEnd)
                || (offset >= input.len() && room_left)
                || !progressed
            {
                break;
            }
        }
        String::from_utf8(output)
            .map(Some)
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compress, Compression, FlushCompress};

    fn compress(stream: &mut Compress, text: &str) -> Vec<u8> {
        let mut out = Vec::with_capacity(text.len() + 1024);
        stream
            .compress_vec(text.as_bytes(), &mut out, FlushCompress::Sync)
            .unwrap();
        assert!(out.ends_with(&SYNC_FLUSH));
        out
    }

    #[test]
    fn messages_share_one_stream_and_may_span_frames() {
        let mut sender = Compress::new(Compression::default(), true);
        let mut receiver = ZlibStream::new();
        let first = compress(&mut sender, r#"{"op":10,"d":{"heartbeat_interval":41250}}"#);
        assert_eq!(
            receiver.feed(&first).unwrap().as_deref(),
            Some(r#"{"op":10,"d":{"heartbeat_interval":41250}}"#)
        );
        // A large message, split across frames; it depends on the first's
        // dictionary, so the stream must carry over.
        let big = format!(r#"{{"op":0,"t":"READY","d":"{}"}}"#, "abc".repeat(100_000));
        let second = compress(&mut sender, &big);
        let (a, b) = second.split_at(second.len() / 2);
        assert_eq!(receiver.feed(a).unwrap(), None);
        assert_eq!(receiver.feed(b).unwrap().as_deref(), Some(big.as_str()));
    }
}
