use anyhow::{Context as _, Result, anyhow, bail};
use edit_prediction_types::{
    EditPrediction, EditPredictionDelegate, EditPredictionDiscardReason, EditPredictionIconSet,
    EditPredictionRequestTrigger, interpolate_edits,
};
use futures::AsyncReadExt as _;
use gpui::{App, AppContext as _, Context, Entity, Global, SharedString, Task};
use http_client::HttpClient;
use icons::IconName;
use language::{
    Anchor, Bias, Buffer, BufferSnapshot, EditPreview, PointUtf16, ToPointUtf16, Unclipped,
    language_settings::all_language_settings,
};
use language_model::{ApiKeyState, AuthenticateError, EnvVar, env_var};
use prost::Message;
use std::{
    ops::Range,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

mod request;

pub use request::*;

const CONNECT_HEADER_LENGTH: usize = 5;
const CONNECT_END_STREAM_FLAG: u8 = 0x02;
const CONNECT_COMPRESSED_FLAG: u8 = 0x01;
const DEFAULT_MAX_FRAME_LENGTH: usize = 8 * 1024 * 1024;

pub const CURSOR_TAB_API_URL: &str =
    "https://us-only.gcpp.cursor.sh/aiserver.v1.AiService/StreamCpp";
pub const CURSOR_TAB_MODEL: &str = "fast";

static CURSOR_TAB_BEARER_TOKEN_ENV_VAR: std::sync::LazyLock<EnvVar> =
    env_var!("CURSOR_TAB_BEARER_TOKEN");

struct GlobalCursorTabBearerToken(Entity<ApiKeyState>);

impl Global for GlobalCursorTabBearerToken {}

pub fn cursor_tab_api_url(cx: &App) -> SharedString {
    all_language_settings(None, cx)
        .edit_predictions
        .cursor_tab
        .api_url
        .clone()
        .into()
}

pub fn cursor_tab_bearer_token_state(cx: &mut App) -> Entity<ApiKeyState> {
    if let Some(global) = cx.try_global::<GlobalCursorTabBearerToken>() {
        return global.0.clone();
    }

    let api_url = cursor_tab_api_url(cx);
    let token = cx.new(|_| ApiKeyState::new(api_url, CURSOR_TAB_BEARER_TOKEN_ENV_VAR.clone()));
    cx.set_global(GlobalCursorTabBearerToken(token.clone()));
    token
}

pub fn cursor_tab_bearer_token(cx: &App) -> Option<Arc<str>> {
    let api_url = cursor_tab_api_url(cx);
    cx.try_global::<GlobalCursorTabBearerToken>()?
        .0
        .read(cx)
        .key(&api_url)
}

pub fn load_cursor_tab_bearer_token(cx: &mut App) -> Task<Result<(), AuthenticateError>> {
    let credentials_provider = zed_credentials_provider::global(cx);
    let api_url = cursor_tab_api_url(cx);
    cursor_tab_bearer_token_state(cx).update(cx, |token_state, cx| {
        token_state.load_if_needed(api_url, |state| state, credentials_provider, cx)
    })
}

pub fn encode_connect_message(message: &impl Message) -> Result<Vec<u8>> {
    let payload = message.encode_to_vec();
    let payload_length = u32::try_from(payload.len())?;
    let mut frame = Vec::with_capacity(CONNECT_HEADER_LENGTH + payload.len());
    frame.push(0);
    frame.extend_from_slice(&payload_length.to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

fn authorization_header_value(token: &str) -> String {
    let token = token.trim();
    if token
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("bearer "))
    {
        token.to_string()
    } else {
        format!("Bearer {token}")
    }
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

#[derive(Clone)]
struct CurrentCompletion {
    snapshot: BufferSnapshot,
    edits: Arc<[(Range<Anchor>, Arc<str>)]>,
    edit_preview: EditPreview,
}

impl CurrentCompletion {
    fn interpolate(&self, snapshot: &BufferSnapshot) -> Option<Vec<(Range<Anchor>, Arc<str>)>> {
        interpolate_edits(&self.snapshot, snapshot, &self.edits).filter(|edits| !edits.is_empty())
    }
}

struct CursorTabCompletion {
    text: String,
    range: Option<RangeToReplace>,
}

pub struct CursorTabEditPredictionDelegate {
    http_client: Arc<dyn HttpClient>,
    pending_request: Option<Task<Result<()>>>,
    current_completion: Option<CurrentCompletion>,
}

impl CursorTabEditPredictionDelegate {
    pub fn new(http_client: Arc<dyn HttpClient>) -> Self {
        Self {
            http_client,
            pending_request: None,
            current_completion: None,
        }
    }

    async fn fetch_completion(
        http_client: Arc<dyn HttpClient>,
        api_url: SharedString,
        bearer_token: Arc<str>,
        client_version: String,
        request_id: String,
        session_id: String,
        request: StreamCppRequest,
    ) -> Result<CursorTabCompletion> {
        let request_body = encode_connect_message(&request)?;
        let request = http_client::Request::builder()
            .method(http_client::Method::POST)
            .uri(api_url.as_str())
            .header("content-type", "application/connect+proto")
            .header("connect-protocol-version", "1")
            .header("x-cursor-client-type", "ide")
            .header("x-cursor-client-version", client_version)
            .header("x-cursor-streaming", "true")
            .header("x-request-id", request_id)
            .header("x-session-id", session_id)
            .header("authorization", authorization_header_value(&bearer_token))
            .body(http_client::AsyncBody::from(request_body))?;

        let mut response = http_client.send(request).await?;
        let status = response.status();
        let mut body = Vec::new();
        response.body_mut().read_to_end(&mut body).await?;
        if !status.is_success() {
            bail!(
                "Cursor Tab API error: {status} - {}",
                String::from_utf8_lossy(&body)
            );
        }

        let mut decoder = ConnectDecoder::default();
        let mut completion = CursorTabCompletion {
            text: String::new(),
            range: None,
        };
        for frame in decoder.push(&body)? {
            match frame {
                ConnectFrame::Message(message) => {
                    completion.text.push_str(&message.text);
                    if let Some(range) = message.range_to_replace {
                        completion.range = Some(range);
                    }
                }
                ConnectFrame::EndStream(trailer) => {
                    if let Some(error) = trailer.get("error") {
                        bail!("Cursor Tab stream error: {error}");
                    }
                }
            }
        }
        decoder.finish()?;
        Ok(completion)
    }
}

impl EditPredictionDelegate for CursorTabEditPredictionDelegate {
    fn name() -> &'static str {
        "cursor-tab"
    }

    fn display_name() -> &'static str {
        "Cursor Tab"
    }

    fn show_predictions_in_menu() -> bool {
        true
    }

    fn show_tab_accept_marker() -> bool {
        true
    }

    fn icons(&self, _cx: &App) -> EditPredictionIconSet {
        EditPredictionIconSet::new(IconName::EditorCursor)
    }

    fn is_enabled(&self, _buffer: &Entity<Buffer>, _cursor_position: Anchor, cx: &App) -> bool {
        let settings = &all_language_settings(None, cx).edit_predictions.cursor_tab;
        cursor_tab_bearer_token(cx).is_some()
            && !settings.client_version.trim().is_empty()
            && !settings.request_id.trim().is_empty()
            && !settings.session_id.trim().is_empty()
    }

    fn is_refreshing(&self, _cx: &App) -> bool {
        self.pending_request.is_some()
    }

    fn refresh(
        &mut self,
        buffer: Entity<Buffer>,
        cursor_position: Anchor,
        debounce_duration: Duration,
        _trigger: EditPredictionRequestTrigger,
        cx: &mut Context<Self>,
    ) {
        let Some(bearer_token) = cursor_tab_bearer_token(cx) else {
            return;
        };
        let snapshot = buffer.read(cx).snapshot();
        if self
            .current_completion
            .as_ref()
            .is_some_and(|completion| completion.interpolate(&snapshot).is_some())
        {
            return;
        }

        let cursor = cursor_position.to_point_utf16(&snapshot);
        let (relative_workspace_path, workspace_root_path, language_id) = {
            let buffer = buffer.read(cx);
            let relative_workspace_path = buffer
                .file()
                .map(|file| file.path().as_std_path().to_string_lossy().into_owned())
                .filter(|path| !path.is_empty())
                .unwrap_or_else(|| "Untitled-1".to_string());
            let workspace_root_path = buffer
                .file()
                .and_then(|file| file.as_local())
                .map(|file| {
                    let mut root = file.abs_path(cx);
                    for _ in file.path().as_std_path().components() {
                        root.pop();
                    }
                    root
                })
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let language_id = buffer
                .language()
                .map(|language| language.name().lsp_id())
                .unwrap_or_else(|| "plaintext".to_string());
            (relative_workspace_path, workspace_root_path, language_id)
        };
        let settings = &all_language_settings(None, cx).edit_predictions.cursor_tab;
        let api_url = settings.api_url.clone().into();
        let model = settings.model.clone();
        let client_version = settings.client_version.clone();
        let request_id = settings.request_id.clone();
        let session_id = settings.session_id.clone();
        let http_client = self.http_client.clone();
        let started_at = Instant::now();

        self.pending_request = Some(cx.spawn(async move |this, cx| {
            let result: Result<Option<CurrentCompletion>> = async {
                if !debounce_duration.is_zero() {
                    cx.background_executor().timer(debounce_duration).await;
                }

                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .context("system clock is before the Unix epoch")?
                    .as_millis() as f64;
                let request = StreamCppRequestInput {
                    relative_workspace_path,
                    workspace_root_path,
                    contents: snapshot.text(),
                    cursor_position: CursorPosition {
                        line: i32::try_from(cursor.row)?,
                        column: i32::try_from(cursor.column)?,
                    },
                    selection: None,
                    language_id,
                    file_version: None,
                    sha_256_hash: None,
                    linter_errors: Vec::new(),
                    file_diff_histories: Vec::new(),
                    additional_files: Vec::new(),
                    code_results: Vec::new(),
                    model_name: Some(model),
                    intent_source: Some("line_change".to_string()),
                    workspace_id: None,
                    client_time: now,
                    time_since_request_start: started_at.elapsed().as_millis() as f64,
                    time_at_request_send: now,
                    client_timezone_offset: None,
                    supports_cpt: false,
                    supports_crlf_cpt: false,
                }
                .build()?;

                let completion = Self::fetch_completion(
                    http_client,
                    api_url,
                    bearer_token,
                    client_version,
                    request_id,
                    session_id,
                    request,
                )
                .await?;
                if completion.text.is_empty() {
                    return Ok(None);
                }

                let edit_range = if let Some(range) = completion.range {
                    let start = snapshot.clip_point_utf16(
                        Unclipped(PointUtf16::new(
                            u32::try_from(range.start_line)?,
                            u32::try_from(range.start_column)?,
                        )),
                        Bias::Left,
                    );
                    let end = snapshot.clip_point_utf16(
                        Unclipped(PointUtf16::new(
                            u32::try_from(range.end_line)?,
                            u32::try_from(range.end_column)?,
                        )),
                        Bias::Right,
                    );
                    snapshot.anchor_before(start)..snapshot.anchor_after(end)
                } else {
                    cursor_position..cursor_position
                };
                let edits: Arc<[(Range<Anchor>, Arc<str>)]> =
                    Arc::from([(edit_range, Arc::from(completion.text))]);
                let edit_preview = buffer
                    .read_with(cx, |buffer, cx| buffer.preview_edits(edits.clone(), cx))
                    .await;
                Ok(Some(CurrentCompletion {
                    snapshot,
                    edits,
                    edit_preview,
                }))
            }
            .await;

            this.update(cx, |this, cx| {
                this.pending_request = None;
                if let Ok(Some(completion)) = &result {
                    this.current_completion = Some(completion.clone());
                }
                cx.notify();
            })?;
            result.map(|_| ())
        }));
    }

    fn accept(&mut self, _cx: &mut Context<Self>) {
        self.pending_request = None;
        self.current_completion = None;
    }

    fn discard(&mut self, _reason: EditPredictionDiscardReason, _cx: &mut Context<Self>) {
        self.pending_request = None;
        self.current_completion = None;
    }

    fn suggest(
        &mut self,
        buffer: &Entity<Buffer>,
        _cursor_position: Anchor,
        cx: &mut Context<Self>,
    ) -> Option<EditPrediction> {
        let completion = self.current_completion.as_ref()?;
        let edits = completion.interpolate(&buffer.read(cx).snapshot())?;
        Some(EditPrediction::Local {
            id: None,
            edits,
            cursor_position: None,
            edit_preview: Some(completion.edit_preview.clone()),
        })
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

    #[test]
    fn accepts_raw_and_prefixed_bearer_tokens() {
        assert_eq!(authorization_header_value("jwt"), "Bearer jwt");
        assert_eq!(authorization_header_value(" bearer jwt "), "bearer jwt");
    }
}
