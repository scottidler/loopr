//! IPC codec for length-prefixed JSON framing.
//!
//! Provides a codec for encoding/decoding JSON messages over a stream.

use bytes::{Buf, BufMut, BytesMut};
use serde::{Serialize, de::DeserializeOwned};
use std::marker::PhantomData;
use tokio_util::codec::{Decoder, Encoder};

use crate::error::{LooprError, Result};

/// Length-prefixed JSON codec.
///
/// Messages are framed as:
/// - 4 bytes: message length (big-endian u32)
/// - N bytes: JSON payload
#[derive(Debug)]
pub struct JsonCodec<T> {
    _phantom: PhantomData<T>,
    max_length: usize,
}

impl<T> JsonCodec<T> {
    /// Create a new codec with default max length (16 MB).
    pub fn new() -> Self {
        Self {
            _phantom: PhantomData,
            max_length: 16 * 1024 * 1024,
        }
    }

    /// Create a new codec with custom max length.
    pub fn with_max_length(max_length: usize) -> Self {
        Self {
            _phantom: PhantomData,
            max_length,
        }
    }

    /// Get the max message length.
    pub fn max_length(&self) -> usize {
        self.max_length
    }
}

impl<T> Default for JsonCodec<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Clone for JsonCodec<T> {
    fn clone(&self) -> Self {
        Self {
            _phantom: PhantomData,
            max_length: self.max_length,
        }
    }
}

impl<T: DeserializeOwned> Decoder for JsonCodec<T> {
    type Item = T;
    type Error = std::io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> std::result::Result<Option<Self::Item>, Self::Error> {
        // Need at least 4 bytes for the length prefix
        if src.len() < 4 {
            return Ok(None);
        }

        // Peek at the length without consuming
        let length = u32::from_be_bytes([src[0], src[1], src[2], src[3]]) as usize;

        // Check max length
        if length > self.max_length {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Message too large: {} > {}", length, self.max_length),
            ));
        }

        // Check if we have the full message
        if src.len() < 4 + length {
            // Reserve space for the message
            src.reserve(4 + length - src.len());
            return Ok(None);
        }

        // Consume the length prefix
        src.advance(4);

        // Take the message bytes
        let data = src.split_to(length);

        // Deserialize
        serde_json::from_slice(&data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("JSON error: {}", e)))
    }
}

impl<T: Serialize> Encoder<T> for JsonCodec<T> {
    type Error = std::io::Error;

    fn encode(&mut self, item: T, dst: &mut BytesMut) -> std::result::Result<(), Self::Error> {
        // Serialize to JSON
        let json = serde_json::to_vec(&item)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("JSON error: {}", e)))?;

        let length = json.len();

        // Check max length
        if length > self.max_length {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Message too large: {} > {}", length, self.max_length),
            ));
        }

        // Write length prefix and data
        dst.reserve(4 + length);
        dst.put_u32(length as u32);
        dst.put_slice(&json);

        Ok(())
    }
}

/// Newline-delimited JSON codec (alternative).
///
/// Messages are separated by newlines. Each message is a single JSON object.
#[derive(Debug)]
pub struct NdJsonCodec<T> {
    _phantom: PhantomData<T>,
    max_length: usize,
}

impl<T> NdJsonCodec<T> {
    /// Create a new newline-delimited codec with default max length (16 MB).
    pub fn new() -> Self {
        Self {
            _phantom: PhantomData,
            max_length: 16 * 1024 * 1024,
        }
    }

    /// Create a new codec with custom max length.
    pub fn with_max_length(max_length: usize) -> Self {
        Self {
            _phantom: PhantomData,
            max_length,
        }
    }
}

impl<T> Default for NdJsonCodec<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Clone for NdJsonCodec<T> {
    fn clone(&self) -> Self {
        Self {
            _phantom: PhantomData,
            max_length: self.max_length,
        }
    }
}

impl<T: DeserializeOwned> Decoder for NdJsonCodec<T> {
    type Item = T;
    type Error = std::io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> std::result::Result<Option<Self::Item>, Self::Error> {
        // Find newline
        let newline_pos = src.iter().position(|&b| b == b'\n');

        match newline_pos {
            Some(pos) => {
                // Check max length
                if pos > self.max_length {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("Message too large: {} > {}", pos, self.max_length),
                    ));
                }

                // Take the line (without newline)
                let line = src.split_to(pos);
                // Skip the newline
                src.advance(1);

                // Deserialize
                serde_json::from_slice(&line)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("JSON error: {}", e)))
            }
            None => {
                // Check if buffer is getting too large
                if src.len() > self.max_length {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("Message too large: {} > {}", src.len(), self.max_length),
                    ));
                }
                Ok(None)
            }
        }
    }
}

impl<T: Serialize> Encoder<T> for NdJsonCodec<T> {
    type Error = std::io::Error;

    fn encode(&mut self, item: T, dst: &mut BytesMut) -> std::result::Result<(), Self::Error> {
        // Serialize to JSON (compact, no newlines)
        let json = serde_json::to_vec(&item)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("JSON error: {}", e)))?;

        // Check max length
        if json.len() > self.max_length {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Message too large: {} > {}", json.len(), self.max_length),
            ));
        }

        // Write data and newline
        dst.reserve(json.len() + 1);
        dst.put_slice(&json);
        dst.put_u8(b'\n');

        Ok(())
    }
}

/// Encode a message to bytes using length-prefixed framing.
pub fn encode_message<T: Serialize>(msg: &T) -> Result<Vec<u8>> {
    let json = serde_json::to_vec(msg)?;
    let length = json.len() as u32;
    let mut result = Vec::with_capacity(4 + json.len());
    result.extend_from_slice(&length.to_be_bytes());
    result.extend_from_slice(&json);
    Ok(result)
}

/// Decode a message from bytes using length-prefixed framing.
pub fn decode_message<T: DeserializeOwned>(data: &[u8]) -> Result<T> {
    if data.len() < 4 {
        return Err(LooprError::Ipc("Message too short".into()));
    }

    let length = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;

    if data.len() < 4 + length {
        return Err(LooprError::Ipc("Incomplete message".into()));
    }

    let json_data = &data[4..4 + length];
    serde_json::from_slice(json_data).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct TestMessage {
        id: u32,
        text: String,
    }

    #[test]
    fn test_json_codec_new() {
        let codec: JsonCodec<TestMessage> = JsonCodec::new();
        assert_eq!(codec.max_length(), 16 * 1024 * 1024);
    }

    #[test]
    fn test_json_codec_with_max_length() {
        let codec: JsonCodec<TestMessage> = JsonCodec::with_max_length(1024);
        assert_eq!(codec.max_length(), 1024);
    }

    #[test]
    fn test_json_codec_clone() {
        let codec: JsonCodec<TestMessage> = JsonCodec::with_max_length(2048);
        let cloned = codec.clone();
        assert_eq!(cloned.max_length(), 2048);
    }

    #[test]
    fn test_json_codec_encode_decode() {
        let mut encoder: JsonCodec<TestMessage> = JsonCodec::new();
        let mut decoder: JsonCodec<TestMessage> = JsonCodec::new();

        let msg = TestMessage {
            id: 42,
            text: "hello world".into(),
        };

        // Encode
        let mut buf = BytesMut::new();
        encoder.encode(msg.clone(), &mut buf).unwrap();

        // Decode
        let decoded = decoder.decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn test_json_codec_partial_message() {
        let mut decoder: JsonCodec<TestMessage> = JsonCodec::new();

        // Only send first 2 bytes (incomplete length prefix)
        let mut buf = BytesMut::from(&[0u8, 0][..]);
        let result = decoder.decode(&mut buf).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_json_codec_incomplete_body() {
        let mut decoder: JsonCodec<TestMessage> = JsonCodec::new();

        // Send length prefix for 100 bytes, but only 10 bytes of data
        let mut buf = BytesMut::new();
        buf.put_u32(100);
        buf.put_slice(&[0u8; 10]);

        let result = decoder.decode(&mut buf).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_json_codec_message_too_large_decode() {
        let mut decoder: JsonCodec<TestMessage> = JsonCodec::with_max_length(10);

        // Send length prefix for 100 bytes
        let mut buf = BytesMut::new();
        buf.put_u32(100);

        let result = decoder.decode(&mut buf);
        assert!(result.is_err());
    }

    #[test]
    fn test_json_codec_message_too_large_encode() {
        let mut encoder: JsonCodec<TestMessage> = JsonCodec::with_max_length(10);

        let msg = TestMessage {
            id: 42,
            text: "this is a very long message that exceeds the limit".into(),
        };

        let mut buf = BytesMut::new();
        let result = encoder.encode(msg, &mut buf);
        assert!(result.is_err());
    }

    #[test]
    fn test_json_codec_multiple_messages() {
        let mut encoder: JsonCodec<TestMessage> = JsonCodec::new();
        let mut decoder: JsonCodec<TestMessage> = JsonCodec::new();

        let msg1 = TestMessage {
            id: 1,
            text: "first".into(),
        };
        let msg2 = TestMessage {
            id: 2,
            text: "second".into(),
        };

        // Encode both messages
        let mut buf = BytesMut::new();
        encoder.encode(msg1.clone(), &mut buf).unwrap();
        encoder.encode(msg2.clone(), &mut buf).unwrap();

        // Decode both
        let decoded1 = decoder.decode(&mut buf).unwrap().unwrap();
        let decoded2 = decoder.decode(&mut buf).unwrap().unwrap();

        assert_eq!(decoded1, msg1);
        assert_eq!(decoded2, msg2);
    }

    #[test]
    fn test_ndjson_codec_new() {
        let codec: NdJsonCodec<TestMessage> = NdJsonCodec::new();
        assert_eq!(codec.max_length, 16 * 1024 * 1024);
    }

    #[test]
    fn test_ndjson_codec_encode_decode() {
        let mut encoder: NdJsonCodec<TestMessage> = NdJsonCodec::new();
        let mut decoder: NdJsonCodec<TestMessage> = NdJsonCodec::new();

        let msg = TestMessage {
            id: 42,
            text: "hello world".into(),
        };

        // Encode
        let mut buf = BytesMut::new();
        encoder.encode(msg.clone(), &mut buf).unwrap();

        // Verify newline at end
        assert_eq!(buf[buf.len() - 1], b'\n');

        // Decode
        let decoded = decoder.decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn test_ndjson_codec_partial_message() {
        let mut decoder: NdJsonCodec<TestMessage> = NdJsonCodec::new();

        // Message without newline
        let mut buf = BytesMut::from(&br#"{"id":1,"text":"hello"}"#[..]);
        let result = decoder.decode(&mut buf).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_ndjson_codec_multiple_messages() {
        let mut encoder: NdJsonCodec<TestMessage> = NdJsonCodec::new();
        let mut decoder: NdJsonCodec<TestMessage> = NdJsonCodec::new();

        let msg1 = TestMessage {
            id: 1,
            text: "first".into(),
        };
        let msg2 = TestMessage {
            id: 2,
            text: "second".into(),
        };

        // Encode both messages
        let mut buf = BytesMut::new();
        encoder.encode(msg1.clone(), &mut buf).unwrap();
        encoder.encode(msg2.clone(), &mut buf).unwrap();

        // Decode both
        let decoded1 = decoder.decode(&mut buf).unwrap().unwrap();
        let decoded2 = decoder.decode(&mut buf).unwrap().unwrap();

        assert_eq!(decoded1, msg1);
        assert_eq!(decoded2, msg2);
    }

    #[test]
    fn test_encode_message() {
        let msg = TestMessage {
            id: 42,
            text: "test".into(),
        };

        let encoded = encode_message(&msg).unwrap();

        // Check length prefix
        let length = u32::from_be_bytes([encoded[0], encoded[1], encoded[2], encoded[3]]) as usize;
        assert_eq!(encoded.len(), 4 + length);

        // Check JSON content
        let json: TestMessage = serde_json::from_slice(&encoded[4..]).unwrap();
        assert_eq!(json, msg);
    }

    #[test]
    fn test_decode_message() {
        let msg = TestMessage {
            id: 42,
            text: "test".into(),
        };

        let encoded = encode_message(&msg).unwrap();
        let decoded: TestMessage = decode_message(&encoded).unwrap();

        assert_eq!(decoded, msg);
    }

    #[test]
    fn test_decode_message_too_short() {
        let result: Result<TestMessage> = decode_message(&[0, 1, 2]);
        assert!(result.is_err());
    }

    #[test]
    fn test_decode_message_incomplete() {
        // Length says 100 bytes, but only have 10
        let mut data = vec![0, 0, 0, 100];
        data.extend_from_slice(&[0u8; 10]);

        let result: Result<TestMessage> = decode_message(&data);
        assert!(result.is_err());
    }

    #[test]
    fn test_roundtrip_encode_decode() {
        let messages = vec![
            TestMessage {
                id: 0,
                text: String::new(),
            },
            TestMessage {
                id: u32::MAX,
                text: "max id".into(),
            },
            TestMessage {
                id: 1,
                text: "special chars: äöü 🎉".into(),
            },
        ];

        for msg in messages {
            let encoded = encode_message(&msg).unwrap();
            let decoded: TestMessage = decode_message(&encoded).unwrap();
            assert_eq!(decoded, msg);
        }
    }
}
