use anyhow::{Result, anyhow, bail};
use prost::Message;

mod request;

pub use request::*;

const CONNECT_HEADER_LENGTH: usize = 5;
const CONNECT_END_STREAM_FLAG: u8 = 0x02;
const CONNECT_COMPRESSED_FLAG: u8 = 0x01;
const DEFAULT_MAX_FRAME_LENGTH: usize = 8 * 1024 * 1024;

pub const CURSOR_TAB_API_URL: &str =
    "https://us-only.gcpp.cursor.sh/aiserver.v1.AiService/StreamCpp";
pub const CURSOR_TAB_MODEL: &str = "fast";

pub fn encode_connect_message(message: &impl Message) -> Result<Vec<u8>> {
    let payload = message.encode_to_vec();
    let payload_length = u32::try_from(payload.len())?;
    let mut frame = Vec::with_capacity(CONNECT_HEADER_LENGTH + payload.len());
    frame.push(0);
    frame.extend_from_slice(&payload_length.to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

#[derive(Clone, PartialEq, Message)]
pub struct StreamCppResponse {
    #[prost(string, tag = "1")]
    pub text: String,
    #[prost(int32, optional, tag = "2")]
    pub suggestion_start_line: Option<i32>,
    #[prost(int32, optional, tag = "3")]
    pub suggestion_confidence: Option<i32>,
    #[prost(bool, optional, tag = "4")]
    pub done_stream: Option<bool>,
    #[prost(string, optional, tag = "5")]
    pub debug_model_output: Option<String>,
    #[prost(string, optional, tag = "6")]
    pub debug_model_input: Option<String>,
    #[prost(string, optional, tag = "7")]
    pub debug_stream_time: Option<String>,
    #[prost(string, optional, tag = "8")]
    pub debug_total_time: Option<String>,
    #[prost(string, optional, tag = "9")]
    pub debug_ttft_time: Option<String>,
    #[prost(string, optional, tag = "10")]
    pub debug_server_timing: Option<String>,
    #[prost(message, optional, tag = "11")]
    pub range_to_replace: Option<RangeToReplace>,
    #[prost(message, optional, tag = "12")]
    pub cursor_prediction_target: Option<CursorPredictionTarget>,
    #[prost(bool, optional, tag = "13")]
    pub done_edit: Option<bool>,
    #[prost(message, optional, tag = "14")]
    pub model_info: Option<ModelInfo>,
    #[prost(bool, optional, tag = "15")]
    pub begin_edit: Option<bool>,
    #[prost(bool, optional, tag = "16")]
    pub should_remove_leading_eol: Option<bool>,
    #[prost(string, optional, tag = "17")]
    pub binding_id: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Message)]
pub struct RangeToReplace {
    #[prost(int32, tag = "1")]
    pub start_line: i32,
    #[prost(int32, tag = "2")]
    pub start_column: i32,
    #[prost(int32, tag = "3")]
    pub end_line: i32,
    #[prost(int32, tag = "4")]
    pub end_column: i32,
}

#[derive(Clone, PartialEq, Eq, Message)]
pub struct CursorPredictionTarget {
    #[prost(string, tag = "1")]
    pub relative_path: String,
    #[prost(int32, tag = "2")]
    pub line_number_one_indexed: i32,
    #[prost(string, tag = "3")]
    pub expected_content: String,
    #[prost(bool, tag = "4")]
    pub should_retrigger_cpp: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Message)]
pub struct ModelInfo {
    #[prost(bool, tag = "1")]
    pub is_fused_cursor_prediction_model: bool,
    #[prost(bool, tag = "2")]
    pub is_multidiff_model: bool,
}

#[derive(Debug, PartialEq)]
pub enum ConnectFrame {
    Message(Box<StreamCppResponse>),
    EndStream(serde_json::Value),
}

pub struct ConnectDecoder {
    buffer: Vec<u8>,
    max_frame_length: usize,
}

impl Default for ConnectDecoder {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_FRAME_LENGTH)
    }
}

impl ConnectDecoder {
    pub fn new(max_frame_length: usize) -> Self {
        Self {
            buffer: Vec::new(),
            max_frame_length,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<ConnectFrame>> {
        self.buffer.extend_from_slice(bytes);
        let mut frames = Vec::new();

        while let Some(header) = self.buffer.get(..CONNECT_HEADER_LENGTH) {
            let flags = header[0];
            let frame_length =
                u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
            if frame_length > self.max_frame_length {
                bail!(
                    "Connect frame length {frame_length} exceeds limit {}",
                    self.max_frame_length
                );
            }
            let total_length = CONNECT_HEADER_LENGTH
                .checked_add(frame_length)
                .ok_or_else(|| anyhow!("Connect frame length overflow"))?;
            if self.buffer.len() < total_length {
                break;
            }

            let payload = &self.buffer[CONNECT_HEADER_LENGTH..total_length];
            if flags & CONNECT_COMPRESSED_FLAG != 0 {
                bail!("compressed Connect frames require transport decompression");
            }
            let frame = if flags & CONNECT_END_STREAM_FLAG != 0 {
                ConnectFrame::EndStream(serde_json::from_slice(payload)?)
            } else {
                ConnectFrame::Message(Box::new(StreamCppResponse::decode(payload)?))
            };
            frames.push(frame);
            self.buffer.drain(..total_length);
        }

        Ok(frames)
    }

    pub fn finish(self) -> Result<()> {
        if self.buffer.is_empty() {
            Ok(())
        } else {
            bail!("incomplete Connect frame at end of stream")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(flags: u8, payload: &[u8]) -> Vec<u8> {
        let mut frame = Vec::with_capacity(CONNECT_HEADER_LENGTH + payload.len());
        frame.push(flags);
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(payload);
        frame
    }

    #[test]
    fn decodes_fragmented_and_coalesced_frames() {
        let first = StreamCppResponse {
            text: "function ".into(),
            begin_edit: Some(true),
            range_to_replace: Some(RangeToReplace {
                start_line: 0,
                start_column: 0,
                end_line: 0,
                end_column: 9,
            }),
            ..Default::default()
        };
        let second = StreamCppResponse {
            text: "get_data() {}".into(),
            done_edit: Some(true),
            done_stream: Some(true),
            cursor_prediction_target: Some(CursorPredictionTarget {
                relative_path: "src/main.rs".into(),
                line_number_one_indexed: 2,
                expected_content: "get_data() {}".into(),
                should_retrigger_cpp: true,
            }),
            ..Default::default()
        };
        let first_frame = frame(0, &first.encode_to_vec());
        let second_frame = frame(0, &second.encode_to_vec());
        let trailer = frame(CONNECT_END_STREAM_FLAG, br#"{"metadata":{}}"#);
        let split = first_frame.len() - 2;

        let mut decoder = ConnectDecoder::default();
        assert!(decoder.push(&first_frame[..split]).unwrap().is_empty());

        let mut remaining = first_frame[split..].to_vec();
        remaining.extend_from_slice(&second_frame);
        remaining.extend_from_slice(&trailer);
        let decoded = decoder.push(&remaining).unwrap();

        assert_eq!(decoded.len(), 3);
        assert_eq!(decoded[0], ConnectFrame::Message(Box::new(first)));
        assert_eq!(decoded[1], ConnectFrame::Message(Box::new(second)));
        assert!(
            matches!(&decoded[2], ConnectFrame::EndStream(value) if value["metadata"].is_object())
        );
        decoder.finish().unwrap();
    }

    #[test]
    fn rejects_oversized_and_incomplete_frames() {
        let mut decoder = ConnectDecoder::new(3);
        assert!(decoder.push(&frame(0, &[0; 4])).is_err());

        let mut decoder = ConnectDecoder::default();
        assert!(decoder.push(&[0, 0, 0, 0, 2, 1]).unwrap().is_empty());
        assert!(decoder.finish().is_err());
    }

    #[test]
    fn rejects_compressed_frames_until_decompression_is_supported() {
        let mut decoder = ConnectDecoder::default();
        assert!(decoder.push(&frame(CONNECT_COMPRESSED_FLAG, &[])).is_err());
    }
}
