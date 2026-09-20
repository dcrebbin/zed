use anyhow::{Context as _, Result, anyhow, bail};
use edit_prediction::EditPredictionStore;
use edit_prediction_types::{
    EditPrediction, EditPredictionDelegate, EditPredictionDiscardReason, EditPredictionIconSet,
    EditPredictionRequestTrigger, interpolate_edits,
};
use futures::AsyncReadExt as _;
use gpui::{App, AppContext as _, Context, Entity, Global, SharedString, Task};
use http_client::HttpClient;
use icons::IconName;
use language::{
    Anchor, Bias, Buffer, BufferSnapshot, EditPreview, Point, PointUtf16, ToPointUtf16, Unclipped,
    language_settings::all_language_settings,
};
use language_model::{ApiKeyState, AuthenticateError, EnvVar, env_var};
use lsp::DiagnosticSeverity;
use project::Project;
use prost::Message;
use sha2::{Digest as _, Sha256};
use std::{
    collections::{BTreeMap, HashMap},
    ops::Range,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use time::UtcOffset;

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
    suggestion_start_line: Option<i32>,
}

impl CursorTabCompletion {
    fn replacement<'a>(
        &'a self,
        cursor: PointUtf16,
        current_line_prefix: &str,
    ) -> Result<(Range<Unclipped<PointUtf16>>, &'a str)> {
        if !current_line_prefix.is_empty()
            && let Some(suffix) = self.text.strip_prefix(current_line_prefix)
        {
            return Ok((Unclipped(cursor)..Unclipped(cursor), suffix));
        }

        if let Some(range) = self.range {
            return Ok((
                Unclipped(PointUtf16::new(
                    u32::try_from(range.start_line)?,
                    u32::try_from(range.start_column)?,
                ))
                    ..Unclipped(PointUtf16::new(
                        u32::try_from(range.end_line)?,
                        u32::try_from(range.end_column)?,
                    )),
                &self.text,
            ));
        }

        if let Some(start_row) = self
            .suggestion_start_line
            .map(u32::try_from)
            .transpose()?
            .filter(|start_row| *start_row <= cursor.row)
        {
            return Ok((
                Unclipped(PointUtf16::new(start_row, 0))..Unclipped(cursor),
                &self.text,
            ));
        }

        Ok((Unclipped(cursor)..Unclipped(cursor), &self.text))
    }
}

fn minimize_replacement<'a>(
    range: Range<PointUtf16>,
    cursor: PointUtf16,
    existing_text: &str,
    replacement_text: &'a str,
) -> (Range<PointUtf16>, &'a str) {
    if range.end == cursor
        && !existing_text.is_empty()
        && let Some(suffix) = replacement_text.strip_prefix(existing_text)
    {
        return (cursor..cursor, suffix);
    }

    (range, replacement_text)
}

struct CursorTabRequestContext {
    file_diff_histories: Vec<FileDiffHistory>,
    additional_files: Vec<AdditionalFile>,
    code_results: Vec<CodeResult>,
}

fn milliseconds_since_epoch(time: SystemTime) -> Option<f64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs_f64() * 1000.0)
}

fn sha_256(contents: &str) -> String {
    format!("{:x}", Sha256::digest(contents.as_bytes()))
}

fn file_version(snapshot: &BufferSnapshot) -> Option<i32> {
    snapshot
        .version()
        .most_recent()
        .and_then(|timestamp| i32::try_from(timestamp.value).ok())
}

fn linter_errors(snapshot: &BufferSnapshot) -> Vec<LinterError> {
    snapshot
        .diagnostics_in_range::<_, PointUtf16>(0..snapshot.len(), false)
        .map(|entry| LinterError {
            message: entry.diagnostic.message.as_str().to_owned(),
            range: Some(Selection {
                start_line: i32::try_from(entry.range.start.row).unwrap_or(i32::MAX),
                start_column: i32::try_from(entry.range.start.column).unwrap_or(i32::MAX),
                end_line: i32::try_from(entry.range.end.row).unwrap_or(i32::MAX),
                end_column: i32::try_from(entry.range.end.column).unwrap_or(i32::MAX),
            }),
            source: entry.diagnostic.source.clone(),
            related_information: Vec::new(),
            severity: Some(match entry.diagnostic.severity {
                DiagnosticSeverity::ERROR => 1,
                DiagnosticSeverity::WARNING => 2,
                DiagnosticSeverity::INFORMATION => 3,
                DiagnosticSeverity::HINT => 4,
                _ => 0,
            }),
            is_stale: Some(entry.diagnostic.is_disk_based),
        })
        .collect()
}

fn workspace_id(workspace_root_path: &str) -> Option<String> {
    (!workspace_root_path.is_empty()).then(|| sha_256(workspace_root_path))
}

pub struct CursorTabEditPredictionDelegate {
    http_client: Arc<dyn HttpClient>,
    project: Entity<Project>,
    edit_prediction_store: Entity<EditPredictionStore>,
    pending_request: Option<Task<Result<()>>>,
    current_completion: Option<CurrentCompletion>,
}

impl CursorTabEditPredictionDelegate {
    pub fn new(
        http_client: Arc<dyn HttpClient>,
        project: Entity<Project>,
        edit_prediction_store: Entity<EditPredictionStore>,
    ) -> Self {
        Self {
            http_client,
            project,
            edit_prediction_store,
            pending_request: None,
            current_completion: None,
        }
    }

    fn request_context(
        &self,
        active_buffer: &Entity<Buffer>,
        cursor_position: Anchor,
        cx: &mut App,
    ) -> CursorTabRequestContext {
        let (events, related_files, recently_opened_files) =
            self.edit_prediction_store.update(cx, |store, cx| {
                store.register_buffer(active_buffer, &self.project, cx);
                store.refresh_context(&self.project, active_buffer, cursor_position, cx);
                (
                    store.edit_history_for_project(&self.project, cx),
                    store.context_for_project_with_buffers(&self.project, cx),
                    store.recently_opened_files_for_project(&self.project),
                )
            });

        let mut histories_by_file: BTreeMap<String, (Vec<String>, Vec<f64>)> = BTreeMap::new();
        for event in events {
            let zeta_prompt::Event::BufferChange { path, diff, .. } = event.event.as_ref();
            let history = histories_by_file
                .entry(path.to_string_lossy().into_owned())
                .or_default();
            history.0.push(diff.clone());
        }
        let file_diff_histories = histories_by_file
            .into_iter()
            .map(
                |(file_name, (diff_history, diff_history_timestamps))| FileDiffHistory {
                    file_name,
                    diff_history,
                    diff_history_timestamps,
                },
            )
            .collect();

        let mut additional_files = Vec::new();
        let mut code_results = Vec::new();
        let mut additional_file_indexes = HashMap::new();
        for (related_file, buffer) in related_files {
            if buffer == *active_buffer {
                continue;
            }
            let path = related_file.path.to_string_lossy().into_owned();
            let snapshot = buffer.read(cx).snapshot();
            let mut visible_range_content = Vec::new();
            let mut start_line_number_one_indexed = Vec::new();
            let mut visible_ranges = Vec::new();

            for excerpt in related_file.excerpts {
                let start_row = excerpt.row_range.start.min(snapshot.max_point().row);
                let end_row = excerpt.row_range.end.min(snapshot.max_point().row);
                visible_range_content.push(excerpt.text.to_string());
                start_line_number_one_indexed
                    .push(i32::try_from(start_row.saturating_add(1)).unwrap_or(i32::MAX));
                visible_ranges.push(LineRange {
                    start_line_number: i32::try_from(start_row).unwrap_or(i32::MAX),
                    end_line_number_inclusive: i32::try_from(end_row).unwrap_or(i32::MAX),
                });
                code_results.push(CodeResult {
                    code_block: Some(CodeBlock {
                        relative_workspace_path: path.clone(),
                        range: Some(CodeRange {
                            start_position: Some(Position {
                                line: i32::try_from(start_row).unwrap_or(i32::MAX),
                                column: 0,
                            }),
                            end_position: Some(Position {
                                line: i32::try_from(end_row).unwrap_or(i32::MAX),
                                column: i32::try_from(snapshot.line_len(end_row))
                                    .unwrap_or(i32::MAX),
                            }),
                        }),
                        contents: excerpt.text.to_string(),
                    }),
                    score: 1.0 / (excerpt.order.saturating_add(1) as f64),
                });
            }

            if !visible_range_content.is_empty() {
                additional_file_indexes.insert(path.clone(), additional_files.len());
                additional_files.push(AdditionalFile {
                    relative_workspace_path: path,
                    is_open: true,
                    visible_range_content,
                    last_viewed_at: None,
                    start_line_number_one_indexed,
                    visible_ranges,
                });
            }
        }

        let recently_opened_by_path: HashMap<_, _> = recently_opened_files
            .into_iter()
            .map(|file| (file.path, file.cursor_position))
            .collect();
        for buffer in self.project.read(cx).opened_buffers(cx) {
            if buffer == *active_buffer {
                continue;
            }
            let snapshot = buffer.read(cx).snapshot();
            let Some(file) = snapshot.file() else {
                continue;
            };
            let path = file.path().as_std_path();
            let path_string = path.to_string_lossy().into_owned();
            if additional_file_indexes.contains_key(&path_string) {
                continue;
            }
            let Some(cursor_offset) = recently_opened_by_path.get(path).copied().flatten() else {
                continue;
            };
            let cursor_row = snapshot
                .offset_to_point(cursor_offset.min(snapshot.len()))
                .row;
            let start_row = cursor_row.saturating_sub(100);
            let end_row = cursor_row.saturating_add(100).min(snapshot.max_point().row);
            let content = snapshot
                .text_for_range(
                    Point::new(start_row, 0)..Point::new(end_row, snapshot.line_len(end_row)),
                )
                .collect();
            additional_files.push(AdditionalFile {
                relative_workspace_path: path_string,
                is_open: true,
                visible_range_content: vec![content],
                last_viewed_at: None,
                start_line_number_one_indexed: vec![
                    i32::try_from(start_row.saturating_add(1)).unwrap_or(i32::MAX),
                ],
                visible_ranges: vec![LineRange {
                    start_line_number: i32::try_from(start_row).unwrap_or(i32::MAX),
                    end_line_number_inclusive: i32::try_from(end_row).unwrap_or(i32::MAX),
                }],
            });
        }

        CursorTabRequestContext {
            file_diff_histories,
            additional_files,
            code_results,
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
            suggestion_start_line: None,
        };
        for frame in decoder.push(&body)? {
            match frame {
                ConnectFrame::Message(message) => {
                    let range = message.range_to_replace.map(|range| {
                        (
                            range.start_line,
                            range.start_column,
                            range.end_line,
                            range.end_column,
                        )
                    });
                    let cursor_target = message.cursor_prediction_target.as_ref().map(|target| {
                        (
                            target.relative_path.as_str(),
                            target.line_number_one_indexed,
                            target.expected_content.as_str(),
                            target.should_retrigger_cpp,
                        )
                    });
                    let model_info = message.model_info.map(|model| {
                        (
                            model.is_fused_cursor_prediction_model,
                            model.is_multidiff_model,
                        )
                    });
                    log::debug!(
                        "Cursor Tab response frame: text={:?}, suggestion_start_line={:?}, range={:?}, begin_edit={:?}, done_edit={:?}, done_stream={:?}, should_remove_leading_eol={:?}, binding_id={:?}, cursor_target={:?}, model_info={:?}",
                        message.text,
                        message.suggestion_start_line,
                        range,
                        message.begin_edit,
                        message.done_edit,
                        message.done_stream,
                        message.should_remove_leading_eol,
                        message.binding_id,
                        cursor_target,
                        model_info,
                    );
                    completion.text.push_str(&message.text);
                    if let Some((start_line, start_column, end_line, end_column)) = range {
                        completion.range = Some(RangeToReplace {
                            start_line,
                            start_column,
                            end_line,
                            end_column,
                        });
                    }
                    if let Some(start_line) = message.suggestion_start_line {
                        completion.suggestion_start_line = Some(start_line);
                    }
                }
                ConnectFrame::EndStream(trailer) => {
                    log::debug!("Cursor Tab response trailer: {trailer}");
                    if let Some(error) = trailer.get("error") {
                        bail!("Cursor Tab stream error: {error}");
                    }
                }
            }
        }
        decoder.finish()?;
        log::debug!(
            "Cursor Tab accumulated completion: text={:?}, suggestion_start_line={:?}, range={:?}",
            completion.text,
            completion.suggestion_start_line,
            completion.range.map(|range| (
                range.start_line,
                range.start_column,
                range.end_line,
                range.end_column,
            )),
        );
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

    fn supports_jump_to_edit() -> bool {
        false
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
                    merged_diff_histories: Vec::new(),
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

                let current_line_prefix = snapshot
                    .text_for_range(PointUtf16::new(cursor.row, 0)..cursor)
                    .collect::<String>();
                let (replacement_range, replacement_text) =
                    completion.replacement(cursor, &current_line_prefix)?;
                let start = snapshot.clip_point_utf16(replacement_range.start, Bias::Left);
                let end = snapshot.clip_point_utf16(replacement_range.end, Bias::Right);
                let existing_text = snapshot.text_for_range(start..end).collect::<String>();
                let unminimized_range = start..end;
                let unminimized_text = replacement_text;
                let (replacement_range, replacement_text) =
                    minimize_replacement(unminimized_range.clone(), cursor, &existing_text, replacement_text);
                log::debug!(
                    "Cursor Tab normalized completion: cursor={cursor:?}, current_line_prefix={current_line_prefix:?}, existing_text={existing_text:?}, initial_range={unminimized_range:?}, initial_text={unminimized_text:?}, final_range={replacement_range:?}, final_text={replacement_text:?}"
                );
                let edit_range = if replacement_range.is_empty() {
                    // A right-biased insertion keeps the live cursor before the preview inlay.
                    let insertion_anchor = snapshot.anchor_after(replacement_range.start);
                    insertion_anchor..insertion_anchor
                } else {
                    snapshot.anchor_before(replacement_range.start)
                        ..snapshot.anchor_after(replacement_range.end)
                };
                let edits: Arc<[(Range<Anchor>, Arc<str>)]> =
                    Arc::from([(edit_range, Arc::from(replacement_text))]);
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

    #[test]
    fn legacy_completion_replaces_from_suggestion_start_line_to_cursor() -> Result<()> {
        let completion = CursorTabCompletion {
            text: "console.log(\"hello world\")".into(),
            range: None,
            suggestion_start_line: Some(3),
        };

        let (range, text) = completion.replacement(PointUtf16::new(3, 11), "different prefix")?;

        assert_eq!(range.start.0, PointUtf16::new(3, 0));
        assert_eq!(range.end.0, PointUtf16::new(3, 11));
        assert_eq!(text, "console.log(\"hello world\")");
        Ok(())
    }

    #[test]
    fn explicit_completion_range_takes_precedence() -> Result<()> {
        let completion = CursorTabCompletion {
            text: "replacement".into(),
            range: Some(RangeToReplace {
                start_line: 1,
                start_column: 2,
                end_line: 4,
                end_column: 5,
            }),
            suggestion_start_line: Some(3),
        };

        let (range, text) = completion.replacement(PointUtf16::new(3, 11), "ignored")?;

        assert_eq!(range.start.0, PointUtf16::new(1, 2));
        assert_eq!(range.end.0, PointUtf16::new(4, 5));
        assert_eq!(text, "replacement");
        Ok(())
    }

    #[test]
    fn full_line_completion_without_range_inserts_only_unmatched_suffix() -> Result<()> {
        let completion = CursorTabCompletion {
            text: "console.log(\"hello world\");".into(),
            range: None,
            suggestion_start_line: None,
        };

        let cursor = PointUtf16::new(3, 10);
        let (range, text) = completion.replacement(cursor, "console.lo")?;

        assert_eq!(range.start.0, cursor);
        assert_eq!(range.end.0, cursor);
        assert_eq!(text, "g(\"hello world\");");
        Ok(())
    }

    #[test]
    fn suffix_completion_without_range_is_inserted_at_cursor() -> Result<()> {
        let completion = CursorTabCompletion {
            text: "g(\"hello world\");".into(),
            range: None,
            suggestion_start_line: None,
        };

        let cursor = PointUtf16::new(3, 10);
        let (range, text) = completion.replacement(cursor, "console.lo")?;

        assert_eq!(range.start.0, cursor);
        assert_eq!(range.end.0, cursor);
        assert_eq!(text, "g(\"hello world\");");
        Ok(())
    }

    #[test]
    fn explicit_replacement_of_typed_prefix_is_minimized_to_suffix_insertion() {
        let cursor = PointUtf16::new(3, 10);
        let (range, text) = minimize_replacement(
            PointUtf16::new(3, 0)..cursor,
            cursor,
            "console.lo",
            "console.log(\"hello world\");",
        );

        assert_eq!(range, cursor..cursor);
        assert_eq!(text, "g(\"hello world\");");
    }

    #[test]
    fn typed_prefix_takes_precedence_over_reversed_explicit_range() -> Result<()> {
        let completion = CursorTabCompletion {
            text: "console.log(\"Hello World\");".into(),
            range: Some(RangeToReplace {
                start_line: 0,
                start_column: 8,
                end_line: 0,
                end_column: 0,
            }),
            suggestion_start_line: None,
        };
        let cursor = PointUtf16::new(0, 8);

        let (range, text) = completion.replacement(cursor, "console.")?;

        assert_eq!(range.start.0, cursor);
        assert_eq!(range.end.0, cursor);
        assert_eq!(text, "log(\"Hello World\");");
        Ok(())
    }

    #[test]
    fn replacement_past_cursor_is_not_minimized() {
        let cursor = PointUtf16::new(3, 10);
        let range = PointUtf16::new(3, 0)..PointUtf16::new(3, 12);
        let (minimized_range, text) = minimize_replacement(
            range.clone(),
            cursor,
            "console.load",
            "console.log(\"hello world\");",
        );

        assert_eq!(minimized_range, range);
        assert_eq!(text, "console.log(\"hello world\");");
    }

    #[test]
    fn cursor_tab_does_not_jump_to_distant_edits() {
        assert!(!CursorTabEditPredictionDelegate::supports_jump_to_edit());
    }
}
