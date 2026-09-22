use anyhow::{Context as _, Result, anyhow, bail};
use edit_prediction::EditPredictionStore;
use edit_prediction_types::{
    EditPrediction, EditPredictionDelegate, EditPredictionDiscardReason, EditPredictionIconSet,
    EditPredictionRequestTrigger, PredictedCursorPosition, interpolate_edits,
};
use futures::AsyncReadExt as _;
use gpui::{App, AppContext as _, Context, Entity, Global, SharedString, Task};
use http_client::HttpClient;
use icons::IconName;
use language::{
    Anchor, Bias, Buffer, BufferSnapshot, DiagnosticEntry, EditPreview, Point, PointUtf16,
    RelatedLocation, TextBufferSnapshot, ToOffset, ToPointUtf16, Unclipped,
    language_settings::all_language_settings,
};
use language_model::{ApiKeyState, AuthenticateError, EnvVar, env_var};
use lsp::DiagnosticSeverity;
use project::Project;
use prost::Message;
use sha2::{Digest as _, Sha256};
use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    fs::{File, OpenOptions},
    io::Write as _,
    mem,
    ops::Range,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use time::UtcOffset;

mod request;

pub use request::*;

const CONNECT_HEADER_LENGTH: usize = 5;
const CONNECT_END_STREAM_FLAG: u8 = 0x02;
const CONNECT_COMPRESSED_FLAG: u8 = 0x01;
const DEFAULT_MAX_FRAME_LENGTH: usize = 8 * 1024 * 1024;

fn cursor_tab_debug_log_path() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    let build_directory = executable.ancestors().find(|path| {
        path.file_name()
            .is_some_and(|name| name == "debug" || name == "release")
    })?;
    Some(build_directory.join("cursor-tab-debug.log"))
}

fn write_cursor_tab_debug(arguments: fmt::Arguments<'_>) {
    static DEBUG_LOG: OnceLock<Option<Mutex<File>>> = OnceLock::new();
    let Some(file) = DEBUG_LOG
        .get_or_init(|| {
            let path = cursor_tab_debug_log_path()?;
            let file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(path)
                .ok()?;
            Some(Mutex::new(file))
        })
        .as_ref()
    else {
        return;
    };
    let Ok(mut file) = file.lock() else {
        return;
    };
    if let Err(error) = writeln!(file, "{arguments}") {
        log::error!("failed to write Cursor Tab debug log: {error}");
    }
}

pub const CURSOR_TAB_API_URL: &str = "https://us-only.gcpp.cursor.sh";
pub const CURSOR_TAB_MODEL: &str = "fast";

const STREAM_CPP_PATH: &str = "aiserver.v1.AiService/StreamCpp";
const FILE_SYNC_SERVICE_PATH: &str = "aiserver.v1.FileSyncService/";

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
    pub end_line: i32,
    #[prost(int32, tag = "3")]
    pub start_column: i32,
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
    cursor_position: Option<PredictedCursorPosition>,
    should_retrigger: bool,
    edit_preview: EditPreview,
}

impl CurrentCompletion {
    fn interpolate(&self, snapshot: &BufferSnapshot) -> Option<Vec<(Range<Anchor>, Arc<str>)>> {
        if self.edits.is_empty() && self.snapshot.version() != snapshot.version() {
            return None;
        }
        interpolate_edits(&self.snapshot, snapshot, &self.edits).filter(|edits| {
            !edits.is_empty() || (self.edits.is_empty() && self.cursor_position.is_some())
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct CursorTabCompletion {
    text: String,
    range: Option<RangeToReplace>,
    suggestion_start_line: Option<i32>,
    cursor_prediction_target: Option<CursorPredictionTarget>,
    binding_id: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct CursorTabStream {
    is_multidiff_model: bool,
    edits: Vec<CursorTabCompletion>,
    cursor_prediction_target: Option<CursorPredictionTarget>,
}

struct StreamAccumulator {
    edits: Vec<CursorTabCompletion>,
    current: CursorTabCompletion,
    current_has_content: bool,
    cursor_prediction_target: Option<CursorPredictionTarget>,
    remove_leading_eol: bool,
    is_multidiff_model: Option<bool>,
}

impl StreamAccumulator {
    fn apply(&mut self, message: StreamCppResponse) -> bool {
        if let Some(model) = message.model_info {
            self.is_multidiff_model = Some(model.is_multidiff_model);
        }
        if message.begin_edit == Some(true) {
            self.finish_current();
        }
        if message.should_remove_leading_eol == Some(true) {
            self.remove_leading_eol = true;
        }
        if !message.text.is_empty() {
            self.current.text.push_str(&message.text);
            self.current_has_content = true;
        }
        if let Some(range) = message.range_to_replace {
            self.current.range = Some(range);
            self.current_has_content = true;
        }
        if let Some(start_line) = message.suggestion_start_line {
            self.current.suggestion_start_line = Some(start_line);
            self.current_has_content = true;
        }
        if let Some(target) = message.cursor_prediction_target {
            self.cursor_prediction_target = Some(target.clone());
            if self.current_has_content {
                self.current.cursor_prediction_target = Some(target);
            }
        }
        if let Some(binding_id) = message.binding_id {
            self.current.binding_id = Some(binding_id);
        }
        // Non-multidiff streams can send the replacement range after done_edit.
        if message.done_edit == Some(true) && self.is_multidiff_model != Some(false) {
            self.finish_current();
        }
        message.done_stream == Some(true)
    }

    fn finish_current(&mut self) {
        if self.current_has_content {
            if self.remove_leading_eol
                && let Some(stripped) = self.current.text.strip_prefix('\n')
            {
                self.current.text = stripped.to_owned();
            }
            self.remove_leading_eol = false;
            self.edits.push(mem::take(&mut self.current));
        } else {
            self.current = CursorTabCompletion::default();
        }
        self.current_has_content = false;
    }

    fn finish(mut self) -> CursorTabStream {
        self.finish_current();
        CursorTabStream {
            is_multidiff_model: self.is_multidiff_model == Some(true),
            edits: self.edits,
            cursor_prediction_target: self.cursor_prediction_target,
        }
    }
}

impl CursorTabCompletion {
    fn normalize_document_echo(&mut self, contents: &str, cursor_at_end: bool) {
        let has_reversed_range = self.range.is_some_and(|range| {
            (range.end_line, range.end_column) < (range.start_line, range.start_column)
        });
        if !cursor_at_end || contents.is_empty() || !has_reversed_range {
            return;
        }

        let echoed_suffix = std::iter::once(0)
            .chain(contents.match_indices('\n').map(|(index, _)| index + 1))
            .filter_map(|start| {
                let suffix = &contents[start..];
                (!suffix.is_empty() && self.text.starts_with(suffix)).then_some(suffix)
            })
            .next();
        let Some(echoed_suffix) = echoed_suffix else {
            return;
        };
        let remaining = &self.text[echoed_suffix.len()..];
        if remaining.trim().is_empty() || remaining.trim() == contents.trim() {
            self.text.clear();
        } else {
            self.text = remaining.to_owned();
        }
    }

    fn replacement<'a>(
        &'a self,
        cursor: PointUtf16,
        current_line_prefix: &str,
    ) -> Result<(Range<Unclipped<PointUtf16>>, &'a str)> {
        if let Some(range) = self.range {
            if range.start_line > 0 && range.end_line >= range.start_line {
                // Cursor's one-based, inclusive line ranges exclude the final newline.
                // Clipping the column to the line end preserves that separator.
                let start = PointUtf16::new(u32::try_from(range.start_line - 1)?, 0);
                let end = PointUtf16::new(u32::try_from(range.end_line - 1)?, u32::MAX);
                return Ok((Unclipped(start)..Unclipped(end), &self.text));
            }
            let start = PointUtf16::new(
                u32::try_from(range.start_line)?,
                u32::try_from(range.start_column)?,
            );
            let end = PointUtf16::new(
                u32::try_from(range.end_line)?,
                u32::try_from(range.end_column)?,
            );
            if end < start {
                if !current_line_prefix.is_empty()
                    && let Some(suffix) = self.text.strip_prefix(current_line_prefix)
                {
                    return Ok((Unclipped(cursor)..Unclipped(cursor), suffix));
                }
                return Ok((Unclipped(cursor)..Unclipped(cursor), &self.text));
            }
            return Ok((Unclipped(start)..Unclipped(end), &self.text));
        }

        if !current_line_prefix.is_empty()
            && let Some(suffix) = self.text.strip_prefix(current_line_prefix)
        {
            return Ok((Unclipped(cursor)..Unclipped(cursor), suffix));
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

fn repeats_current_line(replacement_text: &str, current_line_prefix: &str) -> bool {
    if current_line_prefix.is_empty() {
        return false;
    }

    let mut lines = replacement_text
        .lines()
        .filter(|line| !line.trim().is_empty());
    let Some(first_line) = lines.next() else {
        return false;
    };
    first_line == current_line_prefix && lines.all(|line| line == current_line_prefix)
}

fn contains_only_line_breaks_and_indentation(text: &str) -> bool {
    (text.contains('\n') || text.contains('\r')) && text.chars().all(char::is_whitespace)
}

const CURSOR_TARGET_SEARCH_RADIUS: u32 = 32;

fn cursor_target_offset(
    replacement_start_row: u32,
    replacement_text: &str,
    current_file_path: &str,
    target: &CursorPredictionTarget,
) -> Option<usize> {
    if target.relative_path != current_file_path {
        return None;
    }

    let expected_lines = target.expected_content.lines().collect::<Vec<_>>();
    if expected_lines.is_empty() {
        return None;
    }

    let replacement_lines = replacement_text.split('\n').collect::<Vec<_>>();
    let relative_row = target
        .line_number_one_indexed
        .checked_sub(1)
        .and_then(|target_row| u32::try_from(target_row).ok())
        .and_then(|target_row| target_row.checked_sub(replacement_start_row))
        .and_then(|relative_row| usize::try_from(relative_row).ok());
    if let Some(relative_row) = relative_row
        && let Some(offset) = offset_of_expected_lines(
            replacement_text,
            &replacement_lines,
            relative_row,
            &expected_lines,
        )
    {
        return Some(offset);
    }

    (0..replacement_lines.len()).find_map(|relative_row| {
        offset_of_expected_lines(
            replacement_text,
            &replacement_lines,
            relative_row,
            &expected_lines,
        )
    })
}

fn offset_of_expected_lines(
    replacement_text: &str,
    replacement_lines: &[&str],
    relative_row: usize,
    expected_lines: &[&str],
) -> Option<usize> {
    let matched_lines = replacement_lines.get(relative_row..)?;
    if matched_lines.len() < expected_lines.len()
        || !matched_lines
            .iter()
            .zip(expected_lines)
            .all(|(actual, expected)| actual.trim() == expected.trim())
    {
        return None;
    }

    let last_index = relative_row + expected_lines.len() - 1;
    let last_actual = replacement_lines.get(last_index)?;
    let last_expected = expected_lines.last()?.trim();
    let column = last_actual.find(last_expected)? + last_expected.len();
    let line_start = replacement_text
        .split_inclusive('\n')
        .take(last_index)
        .map(str::len)
        .sum::<usize>();
    Some(line_start + column)
}

fn should_refresh_for_trigger(
    trigger: EditPredictionRequestTrigger,
    retrigger_after_accept: bool,
) -> bool {
    trigger != EditPredictionRequestTrigger::PredictionAccepted || retrigger_after_accept
}

fn predicted_cursor_in_snapshot(
    snapshot: &BufferSnapshot,
    current_file_path: &str,
    target: &CursorPredictionTarget,
    skip_row: impl Fn(u32) -> bool,
) -> Option<PredictedCursorPosition> {
    if target.relative_path != current_file_path || target.line_number_one_indexed <= 0 {
        return None;
    }
    let expected_lines = target.expected_content.lines().collect::<Vec<_>>();
    if expected_lines.is_empty() {
        return None;
    }
    let target_row = u32::try_from(target.line_number_one_indexed - 1).ok()?;
    let max_row = snapshot.max_point().row;
    let search_start = target_row.saturating_sub(CURSOR_TARGET_SEARCH_RADIUS);
    let search_end = target_row
        .saturating_add(CURSOR_TARGET_SEARCH_RADIUS)
        .min(max_row);

    let mut best: Option<(u32, Point)> = None;
    for row in search_start..=search_end {
        if skip_row(row) {
            continue;
        }
        let Some(point) = snapshot_match_at_row(snapshot, row, &expected_lines) else {
            continue;
        };
        let distance = row.abs_diff(target_row);
        if best.is_none_or(|(best_distance, _)| distance < best_distance) {
            best = Some((distance, point));
        }
    }
    best.map(|(_, point)| PredictedCursorPosition::at_anchor(snapshot.anchor_before(point)))
}

fn snapshot_match_at_row(
    snapshot: &BufferSnapshot,
    start_row: u32,
    expected_lines: &[&str],
) -> Option<Point> {
    let max_row = snapshot.max_point().row;
    if start_row > max_row {
        return None;
    }
    let end_row = start_row
        .saturating_add(u32::try_from(expected_lines.len().saturating_sub(1)).ok()?)
        .min(max_row);
    let actual_text = snapshot
        .text_for_range(Point::new(start_row, 0)..Point::new(end_row, snapshot.line_len(end_row)))
        .collect::<String>();
    let actual_lines = actual_text.lines().collect::<Vec<_>>();
    if actual_lines.len() < expected_lines.len()
        || !actual_lines
            .iter()
            .zip(expected_lines)
            .all(|(actual, expected)| actual.trim() == expected.trim())
    {
        return None;
    }

    let last_expected = expected_lines.last()?.trim();
    let last_row = start_row
        .saturating_add(u32::try_from(expected_lines.len().saturating_sub(1)).unwrap_or(u32::MAX));
    let last_actual = actual_lines.last()?;
    let column = last_actual
        .find(last_expected)
        .map(|index| u32::try_from(index + last_expected.len()).unwrap_or(u32::MAX))
        .unwrap_or(0)
        .min(snapshot.line_len(last_row.min(max_row)));
    Some(Point::new(last_row.min(max_row), column))
}

fn cursor_target_from_stream(stream: &CursorTabStream) -> Option<&CursorPredictionTarget> {
    stream.cursor_prediction_target.as_ref().or_else(|| {
        stream
            .edits
            .iter()
            .rev()
            .find_map(|edit| edit.cursor_prediction_target.as_ref())
    })
}

fn map_cursor_target(
    snapshot: &BufferSnapshot,
    prepared_edits: &[(Range<PointUtf16>, String)],
    current_file_path: &str,
    target: &CursorPredictionTarget,
) -> Option<PredictedCursorPosition> {
    if let Some(position) = prepared_edits.iter().find_map(|(range, text)| {
        cursor_target_offset(range.start.row, text, current_file_path, target)
            .map(|offset| PredictedCursorPosition::new(snapshot.anchor_before(range.start), offset))
    }) {
        return Some(position);
    }

    predicted_cursor_in_snapshot(snapshot, current_file_path, target, |row| {
        prepared_edits.iter().any(|(range, _)| {
            row >= range.start.row
                && (row < range.end.row || (row == range.end.row && range.end.column > 0))
        })
    })
}

fn apply_multidiff_stream(stream: &CursorTabStream, contents: &str) -> Result<String> {
    let mut updated = contents.to_owned();
    // Multidiff ranges refer to the result of all preceding edits, including
    // edits that change the line count or revisit an earlier rewrite window.
    for edit in &stream.edits {
        let range = edit
            .range
            .context("Cursor Tab multidiff edit has no range")?;
        anyhow::ensure!(
            range.start_line > 0 && range.end_line >= range.start_line - 1,
            "invalid Cursor Tab multidiff line range: {range:?}"
        );
        let starts: Vec<_> = std::iter::once(0)
            .chain(updated.match_indices('\n').map(|(offset, _)| offset + 1))
            .collect();
        let start_row = usize::try_from(range.start_line - 1)?;
        let end_row = usize::try_from(range.end_line)?;
        let insertion = start_row == end_row;
        let mut start = starts
            .get(start_row)
            .copied()
            .or_else(|| (insertion && start_row == starts.len()).then_some(updated.len()))
            .context("Cursor Tab multidiff starts beyond the document")?;
        anyhow::ensure!(
            end_row <= starts.len(),
            "Cursor Tab multidiff ends beyond the document"
        );
        let mut replacement = edit.text.clone();
        let end = if insertion {
            if !replacement.is_empty() {
                if start_row == starts.len() {
                    replacement.insert(0, '\n');
                } else {
                    replacement.push('\n');
                }
            }
            start
        } else if replacement.is_empty() {
            if end_row == starts.len() && start > 0 {
                start -= 1;
            }
            starts.get(end_row).copied().unwrap_or(updated.len())
        } else {
            starts
                .get(end_row)
                .map(|offset| offset - 1)
                .unwrap_or(updated.len())
        };
        updated.replace_range(start..end, &replacement);
    }
    Ok(updated)
}

fn multidiff_edits(contents: &str, updated: &str) -> Vec<(Range<usize>, Arc<str>)> {
    // Restrict tokenization to the changed window: a one-character completion
    // near the end of a large file should not diff every unchanged line.
    let prefix = contents
        .chars()
        .zip(updated.chars())
        .take_while(|(left, right)| left == right)
        .map(|(character, _)| character.len_utf8())
        .sum::<usize>();
    let suffix = contents[prefix..]
        .chars()
        .rev()
        .zip(updated[prefix..].chars().rev())
        .take_while(|(left, right)| left == right)
        .map(|(character, _)| character.len_utf8())
        .sum::<usize>();
    language::text_diff(
        &contents[prefix..contents.len() - suffix],
        &updated[prefix..updated.len() - suffix],
    )
    .into_iter()
    .map(|(range, text)| (range.start + prefix..range.end + prefix, text))
    .collect()
}

fn cursor_local_multidiff_edits(
    stream: &CursorTabStream,
    contents: &str,
    cursor_offset: usize,
) -> Result<Vec<(Range<usize>, Arc<str>)>> {
    let mut updated = contents.to_owned();
    let mut previous_changes: Vec<(Range<usize>, usize)> = Vec::new();
    let mut nearest: Option<(usize, Range<usize>, Arc<str>)> = None;
    for edit in &stream.edits {
        let next = apply_multidiff_stream(
            &CursorTabStream {
                is_multidiff_model: true,
                edits: vec![edit.clone()],
                ..Default::default()
            },
            &updated,
        )?;
        let changes = multidiff_edits(&updated, &next);
        for (range, text) in &changes {
            let mut original_range = range.clone();
            let mut independent = true;
            // Response coordinates move as earlier predictions add/remove text.
            // Only offer changes that can be mapped back to the actual buffer;
            // changes inside unaccepted replacement text need a fresh prediction.
            for (previous, inserted_length) in previous_changes.iter().rev() {
                let inserted_end = previous.start + inserted_length;
                if original_range.end <= previous.start {
                    continue;
                }
                if original_range.start >= inserted_end {
                    original_range = (previous.end + original_range.start - inserted_end)
                        ..(previous.end + original_range.end - inserted_end);
                } else {
                    independent = false;
                    break;
                }
            }
            if !independent {
                continue;
            }
            let distance = original_range.start.saturating_sub(cursor_offset)
                + cursor_offset.saturating_sub(original_range.end);
            if nearest.as_ref().is_none_or(|(best, _, _)| distance < *best) {
                nearest = Some((distance, original_range, text.clone()));
            }
        }
        // Applying a batch from the end preserves its original byte coordinates.
        previous_changes.extend(
            changes
                .into_iter()
                .rev()
                .map(|(range, text)| (range, text.len())),
        );
        updated = next;
    }
    Ok(nearest
        .into_iter()
        .map(|(_, range, text)| (range, text))
        .collect())
}

fn prepared_edits_from_stream(
    stream: &mut CursorTabStream,
    snapshot: &TextBufferSnapshot,
    cursor: PointUtf16,
    current_line_prefix: &str,
    contents: &str,
    cursor_at_end: bool,
) -> Result<Vec<(Range<PointUtf16>, String)>> {
    if stream.is_multidiff_model {
        let edits = cursor_local_multidiff_edits(stream, contents, cursor.to_offset(snapshot))?;
        // Cursor targets from later predictions describe a hypothetical document,
        // not the local edit being offered. Refresh after accepting that edit.
        if !edits.is_empty() {
            stream.cursor_prediction_target = None;
            for edit in &mut stream.edits {
                edit.cursor_prediction_target = None;
            }
        }
        return Ok(edits
            .into_iter()
            .map(|(range, text)| {
                (
                    range.start.to_point_utf16(snapshot)..range.end.to_point_utf16(snapshot),
                    text.to_string(),
                )
            })
            .collect());
    }
    let mut prepared = Vec::new();
    for edit in &mut stream.edits {
        edit.normalize_document_echo(contents, cursor_at_end);
        if edit.text.is_empty() && edit.range.is_none() && edit.suggestion_start_line.is_none() {
            continue;
        }

        let (replacement_range, replacement_text) =
            edit.replacement(cursor, current_line_prefix)?;
        let start = snapshot.clip_point_utf16(replacement_range.start, Bias::Left);
        let end = snapshot.clip_point_utf16(replacement_range.end, Bias::Right);
        let existing_text = snapshot.text_for_range(start..end).collect::<String>();
        let at_cursor = start <= cursor && cursor <= end;
        let (replacement_range, replacement_text) = if at_cursor {
            minimize_replacement(start..end, cursor, &existing_text, replacement_text)
        } else {
            (start..end, replacement_text)
        };
        if replacement_range.is_empty() && replacement_text.is_empty() {
            continue;
        }
        if contains_only_line_breaks_and_indentation(replacement_text) {
            continue;
        }
        if at_cursor && repeats_current_line(replacement_text, current_line_prefix) {
            continue;
        }
        prepared.push((replacement_range, replacement_text.to_owned()));
    }
    prepared.sort_by_key(|(range, _)| {
        (
            range.start.row,
            range.start.column,
            range.end.row,
            range.end.column,
        )
    });
    Ok(prepared)
}

fn anchors_from_prepared_edits(
    snapshot: &TextBufferSnapshot,
    prepared_edits: &[(Range<PointUtf16>, String)],
) -> Arc<[(Range<Anchor>, Arc<str>)]> {
    prepared_edits
        .iter()
        .flat_map(|(replacement_range, replacement_text)| {
            let start_offset = replacement_range.start.to_offset(snapshot);
            let existing_text = snapshot
                .text_for_range(replacement_range.clone())
                .collect::<String>();
            log::debug!(
                "Cursor Tab replacement diff: range={replacement_range:?}\n{}",
                language::unified_diff(&existing_text, replacement_text),
            );
            // The server sends rewrite windows containing unchanged neighbors. Keep
            // those neighbors anchored in place instead of replacing the whole window.
            language::text_diff(&existing_text, replacement_text)
                .into_iter()
                .map(move |(range, text)| {
                    let start = start_offset + range.start;
                    let end = start_offset + range.end;
                    let edit_range = if range.is_empty() {
                        let anchor = snapshot.anchor_after(start);
                        anchor..anchor
                    } else {
                        snapshot.anchor_before(start)..snapshot.anchor_after(end)
                    };
                    (edit_range, text)
                })
        })
        .collect()
}

struct CursorTabRequestContext {
    file_diff_histories: Vec<FileDiffHistory>,
    additional_files: Vec<AdditionalFile>,
    code_results: Vec<CodeResult>,
}

struct SyncedFile {
    model_version: i32,
    contents: String,
}

enum FileSyncRequest {
    Upload(FSUploadFileRequest),
    Sync(FSSyncFileRequest),
}

impl FileSyncRequest {
    fn method_name(&self) -> &'static str {
        match self {
            Self::Upload(_) => "FSUploadFile",
            Self::Sync(_) => "FSSyncFile",
        }
    }

    fn encode(&self) -> Vec<u8> {
        match self {
            Self::Upload(request) => request.encode_to_vec(),
            Self::Sync(request) => request.encode_to_vec(),
        }
    }
}

fn file_sync_api_url(api_url: &str, method_name: &str) -> Result<String> {
    let api_url = api_url.trim_end_matches('/');
    if api_url.is_empty() {
        bail!("Cursor Tab API URL must not be empty");
    }
    Ok(format!("{api_url}/{FILE_SYNC_SERVICE_PATH}{method_name}"))
}

fn stream_cpp_api_url(api_url: &str) -> Result<String> {
    let api_url = api_url.trim_end_matches('/');
    if api_url.is_empty() {
        bail!("Cursor Tab API URL must not be empty");
    }
    Ok(format!("{api_url}/{STREAM_CPP_PATH}"))
}

fn file_sync_cookie(workspace_root_path: &str) -> String {
    let hash = sha_256(workspace_root_path);
    format!("FilesyncCookie={}", &hash[..32])
}

fn single_file_update(old_contents: &str, new_contents: &str) -> Result<SingleUpdateRequest> {
    let old_bytes = old_contents.as_bytes();
    let new_bytes = new_contents.as_bytes();
    let mut prefix_length = old_bytes
        .iter()
        .zip(new_bytes)
        .take_while(|(old, new)| old == new)
        .count();
    while !old_contents.is_char_boundary(prefix_length)
        || !new_contents.is_char_boundary(prefix_length)
    {
        prefix_length = prefix_length.saturating_sub(1);
    }

    let maximum_suffix_length = old_contents
        .len()
        .saturating_sub(prefix_length)
        .min(new_contents.len().saturating_sub(prefix_length));
    let mut suffix_length = old_bytes
        .iter()
        .rev()
        .zip(new_bytes.iter().rev())
        .take(maximum_suffix_length)
        .take_while(|(old, new)| old == new)
        .count();
    while !old_contents.is_char_boundary(old_contents.len() - suffix_length)
        || !new_contents.is_char_boundary(new_contents.len() - suffix_length)
    {
        suffix_length = suffix_length.saturating_sub(1);
    }

    let old_end = old_contents.len() - suffix_length;
    let new_end = new_contents.len() - suffix_length;
    let start_position = old_contents[..prefix_length].encode_utf16().count();
    let end_position = old_contents[..old_end].encode_utf16().count();
    let replaced_string = new_contents[prefix_length..new_end].to_owned();
    Ok(SingleUpdateRequest {
        start_position: i32::try_from(start_position)?,
        end_position: i32::try_from(end_position)?,
        change_length: i32::try_from(end_position.saturating_sub(start_position))?,
        replaced_string,
        range: None,
    })
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
        .diagnostic_entries_in_range(0..snapshot.len(), false)
        .map(|entry| linter_error(snapshot, entry))
        .collect()
}

fn linter_error(snapshot: &BufferSnapshot, entry: &DiagnosticEntry<Anchor>) -> LinterError {
    let range =
        entry.range.start.to_point_utf16(snapshot)..entry.range.end.to_point_utf16(snapshot);
    LinterError {
        message: entry.diagnostic.message.as_str().to_owned(),
        range: Some(code_range(range)),
        source: entry.diagnostic.source.clone(),
        related_information: entry
            .related_information
            .as_deref()
            .into_iter()
            .flatten()
            .map(|information| RelatedInformation {
                message: information.message.clone(),
                range: Some(match &information.location {
                    RelatedLocation::InBuffer(range) => code_range(
                        range.start.to_point_utf16(snapshot)..range.end.to_point_utf16(snapshot),
                    ),
                    RelatedLocation::InAnotherFile(location) => CodeRange {
                        start_position: Some(position_from_lsp(location.range.start)),
                        end_position: Some(position_from_lsp(location.range.end)),
                    },
                }),
            })
            .collect(),
        severity: Some(match entry.diagnostic.severity {
            DiagnosticSeverity::ERROR => 1,
            DiagnosticSeverity::WARNING => 2,
            DiagnosticSeverity::INFORMATION => 3,
            DiagnosticSeverity::HINT => 4,
            _ => 0,
        }),
        is_stale: Some(entry.diagnostic.is_disk_based),
    }
}

fn code_range(range: Range<PointUtf16>) -> CodeRange {
    CodeRange {
        start_position: Some(position(range.start)),
        end_position: Some(position(range.end)),
    }
}

fn position(point: PointUtf16) -> Position {
    Position {
        line: i32::try_from(point.row).unwrap_or(i32::MAX),
        column: i32::try_from(point.column).unwrap_or(i32::MAX),
    }
}

fn position_from_lsp(position: lsp::Position) -> Position {
    Position {
        line: i32::try_from(position.line).unwrap_or(i32::MAX),
        column: i32::try_from(position.character).unwrap_or(i32::MAX),
    }
}

fn workspace_id(workspace_root_path: &str) -> Option<String> {
    (!workspace_root_path.is_empty()).then(|| sha_256(workspace_root_path))
}

fn cursor_relative_history_path(
    history_path: &str,
    current_file_full_path: Option<&str>,
    current_file_relative_path: &str,
) -> String {
    if history_path == current_file_relative_path {
        return history_path.to_owned();
    }
    if current_file_full_path == Some(history_path) {
        return current_file_relative_path.to_owned();
    }
    if let Some(full_path) = current_file_full_path
        && let Some(prefix) = full_path.strip_suffix(current_file_relative_path)
        && !prefix.is_empty()
        && let Some(relative) = history_path.strip_prefix(prefix)
        && !relative.is_empty()
    {
        return relative.to_owned();
    }
    history_path.to_owned()
}

fn accumulate_stream_frames(
    frames: impl IntoIterator<Item = ConnectFrame>,
) -> Result<CursorTabStream> {
    let mut accumulator = StreamAccumulator {
        edits: Vec::new(),
        current: CursorTabCompletion::default(),
        current_has_content: false,
        cursor_prediction_target: None,
        remove_leading_eol: false,
        is_multidiff_model: None,
    };
    for frame in frames {
        match frame {
            ConnectFrame::Message(message) => {
                let range = message.range_to_replace.map(|range| {
                    (
                        range.start_line,
                        range.end_line,
                        range.start_column,
                        range.end_column,
                    )
                });
                let cursor_target = message.cursor_prediction_target.as_ref().map(|target| {
                    (
                        target.relative_path.clone(),
                        target.line_number_one_indexed,
                        target.expected_content.clone(),
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
                write_cursor_tab_debug(format_args!(
                    "response frame: text={:?}, range={range:?}, begin_edit={:?}, done_edit={:?}, done_stream={:?}, should_remove_leading_eol={:?}, binding_id={:?}, cursor_target={cursor_target:?}",
                    message.text,
                    message.begin_edit,
                    message.done_edit,
                    message.done_stream,
                    message.should_remove_leading_eol,
                    message.binding_id,
                ));
                if accumulator.apply(*message) {
                    break;
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
    Ok(accumulator.finish())
}

pub struct CursorTabEditPredictionDelegate {
    http_client: Arc<dyn HttpClient>,
    project: Entity<Project>,
    edit_prediction_store: Entity<EditPredictionStore>,
    pending_request: Option<Task<Result<()>>>,
    pending_file_sync: Option<Task<Result<()>>>,
    synced_files: HashMap<String, SyncedFile>,
    file_sync_uuid: String,
    current_completion: Option<CurrentCompletion>,
    retrigger_after_accept: bool,
    request_generation: u64,
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
            pending_file_sync: None,
            synced_files: HashMap::new(),
            file_sync_uuid: uuid::Uuid::new_v4().to_string(),
            current_completion: None,
            retrigger_after_accept: true,
            request_generation: 0,
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

        let (current_file_relative_path, current_file_full_path) = {
            let buffer = active_buffer.read(cx);
            let relative_path = buffer
                .file()
                .map(|file| file.path().as_std_path().to_string_lossy().into_owned())
                .filter(|path| !path.is_empty())
                .unwrap_or_else(|| "Untitled-1".to_string());
            let full_path = buffer
                .file()
                .map(|file| file.full_path(cx).to_string_lossy().into_owned());
            (relative_path, full_path)
        };

        let mut histories_by_file: BTreeMap<String, (Vec<String>, Vec<f64>)> = BTreeMap::new();
        for event in events {
            let zeta_prompt::Event::BufferChange { path, diff, .. } = event.event.as_ref();
            let file_name = cursor_relative_history_path(
                &path.to_string_lossy(),
                current_file_full_path.as_deref(),
                &current_file_relative_path,
            );
            let history = histories_by_file.entry(file_name).or_default();
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
                        file_contents: None,
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
                    score: 1.0 / (excerpt.order.saturating_add(1) as f32),
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
    ) -> Result<CursorTabStream> {
        let request_body = encode_connect_message(&request)?;
        let request = http_client::Request::builder()
            .method(http_client::Method::POST)
            .uri(stream_cpp_api_url(api_url.as_ref())?)
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
        let frames = decoder.push(&body)?;
        decoder.finish()?;
        let stream = accumulate_stream_frames(frames)?;
        log::debug!(
            "Cursor Tab accumulated stream: edits={}, cursor_target={:?}",
            stream.edits.len(),
            stream.cursor_prediction_target.as_ref().map(|target| (
                target.relative_path.as_str(),
                target.line_number_one_indexed,
                target.expected_content.as_str(),
                target.should_retrigger_cpp,
            )),
        );
        for (index, edit) in stream.edits.iter().enumerate() {
            log::debug!(
                "Cursor Tab accumulated edit {index}: text={:?}, suggestion_start_line={:?}, range={:?}, binding_id={:?}",
                edit.text,
                edit.suggestion_start_line,
                edit.range.map(|range| (
                    range.start_line,
                    range.end_line,
                    range.start_column,
                    range.end_column,
                )),
                edit.binding_id,
            );
        }
        Ok(stream)
    }

    async fn send_file_sync_request(
        http_client: Arc<dyn HttpClient>,
        completion_api_url: SharedString,
        bearer_token: Arc<str>,
        client_version: String,
        request_id: String,
        session_id: String,
        file_sync_client_key: String,
        cookie: String,
        file_sync_request: FileSyncRequest,
    ) -> Result<()> {
        let api_url =
            file_sync_api_url(completion_api_url.as_ref(), file_sync_request.method_name())?;
        let request = http_client::Request::builder()
            .method(http_client::Method::POST)
            .uri(api_url)
            .header("content-type", "application/proto")
            .header("connect-protocol-version", "1")
            .header("x-cursor-client-type", "ide")
            .header("x-cursor-client-version", client_version)
            .header("x-request-id", request_id)
            .header("x-session-id", session_id)
            .header("x-fs-client-key", file_sync_client_key)
            .header("authorization", authorization_header_value(&bearer_token))
            .header("cookie", cookie)
            .body(http_client::AsyncBody::from(file_sync_request.encode()))?;

        let mut response = http_client.send(request).await?;
        let status = response.status();
        if !status.is_success() {
            let mut body = Vec::new();
            response.body_mut().read_to_end(&mut body).await?;
            bail!(
                "Cursor file sync API error: {status} - {}",
                String::from_utf8_lossy(&body)
            );
        }
        Ok(())
    }

    fn enqueue_file_sync(
        &mut self,
        relative_workspace_path: String,
        workspace_root_path: &str,
        contents: String,
        trigger: EditPredictionRequestTrigger,
        bearer_token: Arc<str>,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let settings = &all_language_settings(None, cx).edit_predictions.cursor_tab;
        let file_sync_client_key = settings.file_sync_client_key.clone();
        if file_sync_client_key.is_empty() {
            return Ok(());
        }
        let file_sync_request =
            if let Some(synced_file) = self.synced_files.get_mut(&relative_workspace_path) {
                if trigger != EditPredictionRequestTrigger::BufferEdit
                    || synced_file.contents == contents
                {
                    return Ok(());
                }
                let model_version = synced_file.model_version.saturating_add(1);
                let update = single_file_update(&synced_file.contents, &contents)?;
                synced_file.contents.clone_from(&contents);
                synced_file.model_version = model_version;
                FileSyncRequest::Sync(FSSyncFileRequest {
                    uuid: self.file_sync_uuid.clone(),
                    relative_workspace_path: relative_workspace_path.clone(),
                    model_version,
                    filesync_updates: vec![FilesyncUpdate {
                        model_version,
                        relative_workspace_path: relative_workspace_path.clone(),
                        updates: vec![update],
                        expected_file_length: i32::try_from(contents.encode_utf16().count())?,
                    }],
                    sha256_hash: sha_256(&contents),
                })
            } else {
                let model_version = 1;
                self.synced_files.insert(
                    relative_workspace_path.clone(),
                    SyncedFile {
                        model_version,
                        contents: contents.clone(),
                    },
                );
                FileSyncRequest::Upload(FSUploadFileRequest {
                    uuid: self.file_sync_uuid.clone(),
                    relative_workspace_path,
                    contents: contents.clone(),
                    model_version,
                    sha256_hash: sha_256(&contents),
                })
            };

        let previous_request = self.pending_file_sync.take();
        let http_client = self.http_client.clone();
        let api_url = settings.api_url.clone().into();
        let client_version = settings.client_version.clone();
        let request_id = settings.request_id.clone();
        let session_id = settings.session_id.clone();
        let cookie = file_sync_cookie(workspace_root_path);
        self.pending_file_sync = Some(cx.spawn(async move |_, _cx| {
            if let Some(previous_request) = previous_request
                && let Err(error) = previous_request.await
            {
                log::error!("previous Cursor file sync request failed: {error:#}");
            }
            let result = Self::send_file_sync_request(
                http_client,
                api_url,
                bearer_token,
                client_version,
                request_id,
                session_id,
                file_sync_client_key,
                cookie,
                file_sync_request,
            )
            .await;
            if let Err(error) = &result {
                log::error!("Cursor file sync request failed: {error:#}");
            }
            result
        }));
        Ok(())
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

    fn show_predictions_inline_with_menu() -> bool {
        true
    }

    fn show_tab_accept_marker() -> bool {
        true
    }

    fn accepts_by_line(&self) -> bool {
        true
    }

    fn supports_jump_to_edit() -> bool {
        false
    }

    fn refresh_on_cursor_click() -> bool {
        true
    }

    fn prioritizes_over_completions(&self) -> bool {
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
        trigger: EditPredictionRequestTrigger,
        cx: &mut Context<Self>,
    ) {
        let Some(bearer_token) = cursor_tab_bearer_token(cx) else {
            return;
        };
        if !should_refresh_for_trigger(trigger, self.retrigger_after_accept) {
            self.retrigger_after_accept = true;
            log::debug!(
                "Cursor Tab skipping refresh after accept because should_retrigger_cpp was false"
            );
            write_cursor_tab_debug(format_args!(
                "skipping refresh after accept: trigger={trigger:?}"
            ));
            return;
        }
        let snapshot = buffer.read(cx).snapshot();
        if trigger != EditPredictionRequestTrigger::CursorClick
            && self
                .current_completion
                .as_ref()
                .is_some_and(|completion| completion.interpolate(&snapshot).is_some())
        {
            return;
        }

        let cursor = cursor_position.to_point_utf16(&snapshot);
        let CursorTabRequestContext {
            file_diff_histories,
            additional_files,
            code_results,
        } = self.request_context(&buffer, cursor_position, cx);
        log::debug!(
            "Cursor Tab request context: file_diff_histories={}, additional_files={}, code_results={}",
            file_diff_histories.len(),
            additional_files.len(),
            code_results.len()
        );
        write_cursor_tab_debug(format_args!(
            "request context: file_diff_histories={}, additional_files={}, code_results={}",
            file_diff_histories.len(),
            additional_files.len(),
            code_results.len()
        ));
        for history in &file_diff_histories {
            log::debug!(
                "Cursor Tab file diff history: file={:?}, edits={}, latest={:?}",
                history.file_name,
                history.diff_history.len(),
                history.diff_history.last()
            );
            write_cursor_tab_debug(format_args!(
                "file diff history: file={:?}, edits={}",
                history.file_name,
                history.diff_history.len(),
            ));
        }
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
        let current_file_path = relative_workspace_path.clone();
        let settings = &all_language_settings(None, cx).edit_predictions.cursor_tab;
        let api_url = settings.api_url.clone().into();
        let model = settings.model.clone();
        let client_version = settings.client_version.clone();
        let request_id = settings.request_id.clone();
        let session_id = settings.session_id.clone();
        let http_client = self.http_client.clone();
        let contents = snapshot.text();
        if let Err(error) = self.enqueue_file_sync(
            relative_workspace_path.clone(),
            &workspace_root_path,
            contents.clone(),
            trigger,
            bearer_token.clone(),
            cx,
        ) {
            log::error!("failed to prepare Cursor file sync request: {error:#}");
        }
        let started_at = Instant::now();
        let intent_source = match trigger {
            EditPredictionRequestTrigger::DiagnosticNavigation => "linter_errors",
            EditPredictionRequestTrigger::Explicit => "manual_trigger",
            EditPredictionRequestTrigger::LSPCompletionAccepted => "lsp_suggestions",
            EditPredictionRequestTrigger::PredictionAccepted
            | EditPredictionRequestTrigger::PredictionPartiallyAccepted => "cursor_prediction",
            EditPredictionRequestTrigger::BufferEdit => "editor_change",
            EditPredictionRequestTrigger::EditorCreated
            | EditPredictionRequestTrigger::ProviderChanged
            | EditPredictionRequestTrigger::UserInfoChanged
            | EditPredictionRequestTrigger::VimModeChanged
            | EditPredictionRequestTrigger::SettingsChanged
            | EditPredictionRequestTrigger::CursorClick
            | EditPredictionRequestTrigger::Other => "typing",
        };

        self.request_generation = self.request_generation.wrapping_add(1);
        let generation = self.request_generation;
        self.current_completion = None;
        write_cursor_tab_debug(format_args!(
            "starting request generation={generation}, trigger={trigger:?}"
        ));
        cx.notify();
        self.pending_request = Some(cx.spawn(async move |this, cx| {
            let result: Result<Option<CurrentCompletion>> = async {
                if !debounce_duration.is_zero() {
                    cx.background_executor().timer(debounce_duration).await;
                }
                let is_current = this.update(cx, |this, _cx| this.request_generation == generation)?;
                if !is_current {
                    write_cursor_tab_debug(format_args!(
                        "discarding stale generation {generation} after debounce"
                    ));
                    return Ok(None);
                }

                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .context("system clock is before the Unix epoch")?
                    .as_millis() as f64;
                let linter_errors = linter_errors(&snapshot);
                let client_timezone_offset = UtcOffset::current_local_offset()
                    .ok()
                    .map(|offset| -f64::from(offset.whole_minutes()));
                write_cursor_tab_debug(format_args!(
                    "request metadata: intent={intent_source:?}, generation={generation}, linter_errors={}",
                    linter_errors.len(),
                ));
                log::debug!(
                    "Cursor Tab current file: path={relative_workspace_path:?}, language={language_id:?}, cursor={cursor:?}, bytes={}, lines={}, version={:?}",
                    contents.len(),
                    contents.lines().count(),
                    file_version(&snapshot)
                );
                write_cursor_tab_debug(format_args!(
                    "current file: path={relative_workspace_path:?}, language={language_id:?}, cursor={cursor:?}, bytes={}, lines={}, version={:?}",
                    contents.len(),
                    contents.lines().count(),
                    file_version(&snapshot),
                ));
                let request = StreamCppRequestInput {
                    relative_workspace_path,
                    workspace_root_path,
                    sha_256_hash: Some(sha_256(&contents)),
                    file_version: file_version(&snapshot),
                    contents,
                    cursor_position: CursorPosition {
                        line: i32::try_from(cursor.row)?,
                        column: i32::try_from(cursor.column)?,
                    },
                    selection: None,
                    language_id,
                    linter_errors,
                    file_diff_histories,
                    merged_diff_histories: Vec::new(),
                    additional_files,
                    code_results,
                    model_name: Some(model),
                    intent_source: Some(intent_source.to_string()),
                    workspace_id: None,
                    client_time: now,
                    time_since_request_start: started_at.elapsed().as_millis() as f64,
                    time_at_request_send: now,
                    client_timezone_offset,
                    supports_cpt: true,
                    supports_crlf_cpt: true,
                }
                .build()?;

                let mut stream = Self::fetch_completion(
                    http_client,
                    api_url,
                    bearer_token,
                    client_version,
                    request_id,
                    session_id,
                    request,
                )
                .await?;
                let is_current = this.update(cx, |this, _cx| this.request_generation == generation)?;
                if !is_current {
                    write_cursor_tab_debug(format_args!(
                        "discarding stale generation {generation} after fetch"
                    ));
                    return Ok(None);
                }
                let contents = snapshot.text();
                let cursor_at_end = cursor == snapshot.max_point().to_point_utf16(&snapshot);
                let current_line_prefix = snapshot
                    .text_for_range(PointUtf16::new(cursor.row, 0)..cursor)
                    .collect::<String>();
                let prepared_edits = prepared_edits_from_stream(
                    &mut stream,
                    &snapshot,
                    cursor,
                    &current_line_prefix,
                    &contents,
                    cursor_at_end,
                )?;
                log::debug!(
                    "Cursor Tab prepared edits: cursor={cursor:?}, current_line_prefix={current_line_prefix:?}, edits={prepared_edits:?}"
                );
                let cursor_target = cursor_target_from_stream(&stream);
                let should_retrigger = cursor_target.is_none_or(|target| target.should_retrigger_cpp);
                let mut predicted_cursor_position = cursor_target.and_then(|target| {
                    map_cursor_target(&snapshot, &prepared_edits, &current_file_path, target)
                });
                log::debug!(
                    "Cursor Tab cursor target mapping: target={:?}, edits={:?}, mapped={:?}",
                    cursor_target.map(|target| (
                        target.relative_path.as_str(),
                        target.line_number_one_indexed,
                        target.expected_content.as_str(),
                        target.should_retrigger_cpp,
                    )),
                    prepared_edits,
                    predicted_cursor_position,
                );
                write_cursor_tab_debug(format_args!(
                    "cursor target mapping: target={:?}, edits={prepared_edits:?}, mapped={predicted_cursor_position:?}",
                    cursor_target.map(|target| (
                        target.relative_path.as_str(),
                        target.line_number_one_indexed,
                        target.expected_content.as_str(),
                        target.should_retrigger_cpp,
                    )),
                ));
                let edits = anchors_from_prepared_edits(&snapshot, &prepared_edits);
                if edits.is_empty() {
                    predicted_cursor_position = cursor_target.and_then(|target| {
                        predicted_cursor_in_snapshot(&snapshot, &current_file_path, target, |_| false)
                    }).filter(|position| position.anchor.to_point_utf16(&snapshot) != cursor);
                }
                if edits.is_empty() && predicted_cursor_position.is_none() {
                    log::debug!("Cursor Tab suppressed no-op: server replacement matches the request snapshot");
                    write_cursor_tab_debug(format_args!(
                        "suppressed no-op: generation={generation}, server replacement matches the request snapshot"
                    ));
                    return Ok(None);
                }
                let edit_preview = buffer
                    .read_with(cx, |buffer, cx| buffer.preview_edits(edits.clone(), cx))
                    .await;
                Ok(Some(CurrentCompletion {
                    snapshot,
                    edits,
                    cursor_position: predicted_cursor_position,
                    should_retrigger,
                    edit_preview,
                }))
            }
            .await;

            this.update(cx, |this, cx| {
                if this.request_generation != generation {
                    write_cursor_tab_debug(format_args!(
                        "discarding stale generation {generation} before store, current={}",
                        this.request_generation
                    ));
                    return;
                }
                this.pending_request = None;
                if let Ok(Some(completion)) = &result {
                    log::debug!(
                        "Cursor Tab storing completion: generation={generation}, edits={}, text={:?}, cursor_position={:?}",
                        completion.edits.len(),
                        completion
                            .edits
                            .iter()
                            .map(|(_, text)| text.as_ref())
                            .collect::<Vec<_>>(),
                        completion.cursor_position,
                    );
                    write_cursor_tab_debug(format_args!(
                        "storing completion: generation={generation}, edits={}, cursor_position={:?}",
                        completion.edits.len(),
                        completion.cursor_position,
                    ));
                    this.current_completion = Some(completion.clone());
                } else if let Err(error) = &result {
                    log::debug!("Cursor Tab did not store completion: error={error:#}");
                } else {
                    log::debug!("Cursor Tab did not store completion: empty result");
                }
                cx.notify();
            })?;
            result.map(|_| ())
        }));
    }

    fn accept(&mut self, _cx: &mut Context<Self>) {
        self.retrigger_after_accept = self
            .current_completion
            .as_ref()
            .is_none_or(|completion| completion.should_retrigger);
        self.request_generation = self.request_generation.wrapping_add(1);
        self.pending_request = None;
        self.current_completion = None;
    }

    fn discard(&mut self, _reason: EditPredictionDiscardReason, _cx: &mut Context<Self>) {
        self.retrigger_after_accept = true;
        self.request_generation = self.request_generation.wrapping_add(1);
        self.pending_request = None;
        self.current_completion = None;
    }

    fn suggest(
        &mut self,
        buffer: &Entity<Buffer>,
        cursor_position: Anchor,
        cx: &mut Context<Self>,
    ) -> Option<EditPrediction> {
        let Some(completion) = self.current_completion.as_ref() else {
            log::debug!("Cursor Tab suggest: no stored completion");
            return None;
        };
        let Some(edits) = completion.interpolate(&buffer.read(cx).snapshot()) else {
            log::debug!("Cursor Tab suggest: completion did not interpolate into live buffer");
            return None;
        };
        if edits.is_empty()
            && completion.cursor_position.is_some_and(|position| {
                let snapshot = buffer.read(cx).snapshot();
                position.anchor.to_offset(&snapshot) == cursor_position.to_offset(&snapshot)
            })
        {
            return None;
        }
        log::debug!(
            "Cursor Tab suggest: edits={}, text={:?}, cursor_position={:?}",
            edits.len(),
            edits
                .iter()
                .map(|(_, text)| text.as_ref())
                .collect::<Vec<_>>(),
            completion.cursor_position,
        );
        write_cursor_tab_debug(format_args!(
            "suggest: edits={}, cursor_position={:?}",
            edits.len(),
            completion.cursor_position,
        ));
        Some(EditPrediction::Local {
            id: None,
            edits,
            cursor_position: completion.cursor_position,
            edit_preview: Some(completion.edit_preview.clone()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use language::{Diagnostic, DiagnosticSet, LanguageServerId};

    #[gpui::test]
    fn sends_every_diagnostic_with_related_information(cx: &mut gpui::TestAppContext) {
        let text = "fn main() {\n    unknown();\n}\n";
        let buffer = cx.new(|cx| Buffer::local(text, cx));
        buffer.update(cx, |buffer, cx| {
            let snapshot = buffer.snapshot();
            let mut error = DiagnosticEntry::new(
                PointUtf16::new(1, 4)..PointUtf16::new(1, 11),
                Diagnostic {
                    message: "cannot find function `unknown`".into(),
                    severity: DiagnosticSeverity::ERROR,
                    source: Some("rust-analyzer".into()),
                    is_disk_based: true,
                    is_primary: true,
                    ..Default::default()
                },
            );
            error.related_information = Some(Arc::from([
                language::RelatedInformation {
                    message: "not found in this scope".into(),
                    location: RelatedLocation::InBuffer(
                        PointUtf16::new(0, 3)..PointUtf16::new(0, 7),
                    ),
                },
                language::RelatedInformation {
                    message: "defined here".into(),
                    location: RelatedLocation::InAnotherFile(lsp::Location {
                        uri: lsp::Uri::from_file_path("/work/other.rs").expect("uri"),
                        range: lsp::Range::new(lsp::Position::new(4, 1), lsp::Position::new(4, 8)),
                    }),
                },
            ]));
            let warning = DiagnosticEntry::new(
                PointUtf16::new(0, 3)..PointUtf16::new(0, 7),
                Diagnostic {
                    message: "unused function".into(),
                    severity: DiagnosticSeverity::WARNING,
                    source: Some("rustc".into()),
                    ..Default::default()
                },
            );
            let hint = DiagnosticEntry::new(
                PointUtf16::new(2, 0)..PointUtf16::new(2, 1),
                Diagnostic {
                    message: "consider adding a return type".into(),
                    severity: DiagnosticSeverity::HINT,
                    ..Default::default()
                },
            );
            buffer.update_diagnostics(
                LanguageServerId(1),
                DiagnosticSet::new([error, warning, hint], &snapshot),
                cx,
            );
        });

        let errors = linter_errors(&buffer.read(cx).snapshot());
        assert_eq!(errors.len(), 3);
        assert_eq!(errors[0].message, "unused function");
        assert_eq!(errors[0].severity, Some(2));
        assert!(errors[0].related_information.is_empty());
        assert_eq!(errors[1].message, "cannot find function `unknown`");
        assert_eq!(errors[1].severity, Some(1));
        assert_eq!(errors[1].is_stale, Some(true));
        assert_eq!(
            errors[1]
                .range
                .and_then(|range| range.start_position)
                .map(|position| (position.line, position.column)),
            Some((1, 4))
        );
        assert_eq!(errors[1].related_information.len(), 2);
        assert_eq!(
            errors[1].related_information[0].message,
            "not found in this scope"
        );
        assert_eq!(
            errors[1].related_information[0]
                .range
                .and_then(|range| range.end_position)
                .map(|position| (position.line, position.column)),
            Some((0, 7))
        );
        assert_eq!(errors[1].related_information[1].message, "defined here");
        assert_eq!(
            errors[1].related_information[1]
                .range
                .and_then(|range| range.start_position)
                .map(|position| (position.line, position.column)),
            Some((4, 1))
        );
        assert_eq!(errors[2].message, "consider adding a return type");
        assert_eq!(errors[2].severity, Some(4));
    }

    #[test]
    fn official_multidiff_streams_match_debug_diffs() -> Result<()> {
        let captures: serde_json::Value =
            serde_json::from_str(include_str!("fixtures/official_streams.json"))?;
        for capture in captures.as_array().context("missing captures")? {
            let mut hunks: Vec<(usize, Vec<String>, Vec<String>)> = Vec::new();
            for line in capture["diff"].as_str().context("missing diff")?.lines() {
                if let Some(header) = line.strip_prefix("@@ main.js:") {
                    hunks.push((header.parse()?, Vec::new(), Vec::new()));
                } else if let Some((_, old, new)) = hunks.last_mut() {
                    if let Some(line) = line.strip_prefix("-|") {
                        old.push(line.to_owned());
                    } else if let Some(line) = line.strip_prefix("+|") {
                        new.push(line.to_owned());
                    }
                }
            }
            let mut original_lines = vec!["unchanged".to_owned(); 100];
            for (start, old, new) in hunks.iter().rev() {
                original_lines.splice(*start..start + new.len(), old.iter().cloned());
            }
            let original = original_lines.join("\n");
            let mut expected_lines = original_lines;
            for (start, old, new) in &hunks {
                assert_eq!(&expected_lines[*start..start + old.len()], old);
                expected_lines.splice(*start..start + old.len(), new.iter().cloned());
            }
            let mut frames = Vec::new();
            for message in capture["messages"].as_array().context("missing messages")? {
                let range = message
                    .get("rangeToReplace")
                    .map(|range| -> Result<_> {
                        Ok(RangeToReplace {
                            start_line: i32::try_from(
                                range["startLine"].as_i64().context("missing start")?,
                            )?,
                            end_line: i32::try_from(
                                range["endLine"].as_i64().context("missing end")?,
                            )?,
                            ..Default::default()
                        })
                    })
                    .transpose()?;
                frames.push(ConnectFrame::Message(Box::new(StreamCppResponse {
                    model_info: message.get("modelInfo").map(|_| ModelInfo {
                        is_fused_cursor_prediction_model: true,
                        is_multidiff_model: true,
                    }),
                    range_to_replace: range,
                    text: message["text"].as_str().unwrap_or_default().to_owned(),
                    should_remove_leading_eol: message["shouldRemoveLeadingEol"].as_bool(),
                    begin_edit: message["beginEdit"].as_bool(),
                    done_edit: message["doneEdit"].as_bool(),
                    done_stream: message["doneStream"].as_bool(),
                    ..Default::default()
                })));
            }
            let stream = accumulate_stream_frames(frames)?;
            let updated = apply_multidiff_stream(&stream, &original)?;
            assert_eq!(updated, expected_lines.join("\n"));
            let mut replayed = original.clone();
            for (range, text) in multidiff_edits(&original, &updated).into_iter().rev() {
                replayed.replace_range(range, &text);
            }
            assert_eq!(replayed, updated);
        }
        Ok(())
    }

    #[test]
    fn accepting_bracket_does_not_accept_later_rewrites_or_deletion() -> Result<()> {
        let prefix = "// unchanged\n".repeat(15);
        let fields = "  artists: [\"\"],\n".to_owned() + &"    field: {},\n".repeat(9);
        let suffix = "  {\n".to_owned() + &"    contributor: [\"復古 🎵\"],\n".repeat(20);
        let original = format!("{prefix}{fields}  }},\n{suffix}");
        let mut stream = CursorTabStream {
            is_multidiff_model: true,
            edits: [
                (26, 26, "  }, ]"),
                (16, 16, "  [artists: [\"\"],"),
                (27, 27, "  [{"),
                (28, 47, ""),
            ]
            .into_iter()
            .map(|(start_line, end_line, text)| CursorTabCompletion {
                range: Some(RangeToReplace {
                    start_line,
                    end_line,
                    ..Default::default()
                }),
                text: text.into(),
                ..Default::default()
            })
            .collect(),
            cursor_prediction_target: Some(CursorPredictionTarget {
                relative_path: "main.js".into(),
                line_number_one_indexed: 27,
                should_retrigger_cpp: false,
                ..Default::default()
            }),
        };
        let mut buffer = language::TextBuffer::new(
            Default::default(),
            language::BufferId::new(1)?,
            original.clone(),
        );
        let prepared = prepared_edits_from_stream(
            &mut stream,
            buffer.snapshot(),
            PointUtf16::new(25, 4),
            "  },",
            &original,
            false,
        )?;
        let edits = anchors_from_prepared_edits(buffer.snapshot(), &prepared);
        let (range, text) = edits.first().context("missing bracket insertion")?;
        assert_eq!(edits.len(), 1);
        assert_eq!(
            range.start.to_offset(buffer.snapshot()),
            range.end.to_offset(buffer.snapshot())
        );
        assert_eq!(text.as_ref(), " ]");
        buffer.edit(edits.iter().cloned());
        assert_eq!(
            buffer.snapshot().text(),
            format!("{prefix}{fields}  }}, ]\n{suffix}")
        );
        Ok(())
    }

    #[test]
    fn cursor_local_change_maps_back_after_earlier_line_insertion() -> Result<()> {
        let original =
            "header\nneighbor\ntarget\nartists: [\"Zpecial\"],\ncontributors: { song: {} }";
        let cursor = "header\nneighbor\ntarget".len();
        let stream = CursorTabStream {
            is_multidiff_model: true,
            edits: [(1, "header\nmore"), (4, "target},")]
                .into_iter()
                .map(|(line, text)| CursorTabCompletion {
                    range: Some(RangeToReplace {
                        start_line: line,
                        end_line: line,
                        ..Default::default()
                    }),
                    text: text.into(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let edits = cursor_local_multidiff_edits(&stream, original, cursor)?;
        assert_eq!(edits, vec![(cursor..cursor, Arc::from("},"))]);
        let mut actual = original.to_owned();
        for (range, text) in edits.into_iter().rev() {
            actual.replace_range(range, &text);
        }
        assert_eq!(
            actual,
            "header\nneighbor\ntarget},\nartists: [\"Zpecial\"],\ncontributors: { song: {} }"
        );
        Ok(())
    }

    #[test]
    fn cursor_local_change_can_remove_text_at_cursor() -> Result<()> {
        let original = "header\nvalue: [],,\nkeep: [\"復古 🎵\"],";
        let cursor = "header\nvalue: [],,".len();
        let stream = CursorTabStream {
            is_multidiff_model: true,
            edits: vec![CursorTabCompletion {
                range: Some(RangeToReplace {
                    start_line: 2,
                    end_line: 2,
                    ..Default::default()
                }),
                text: "value: [],".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            cursor_local_multidiff_edits(&stream, original, cursor)?,
            vec![(cursor - 1..cursor, Arc::from(""))]
        );
        Ok(())
    }

    #[test]
    fn multidiff_incremental_edit_in_large_unicode_file() {
        let prefix = "unchanged 🎵\n".repeat(50_000);
        let suffix = "\nunchanged 復古".repeat(50_000);
        let original = format!("{prefix}artists: [\"Z\"],{suffix}");
        let updated = format!("{prefix}artists: [\"Zpecial\"],{suffix}");
        let edits = multidiff_edits(&original, &updated);
        let offset = prefix.len() + "artists: [\"Z".len();
        assert_eq!(edits, vec![(offset..offset, Arc::from("pecial"))]);
    }

    #[test]
    fn multidiff_preserves_explicit_whitespace_and_line_boundaries() -> Result<()> {
        for (original, start_line, end_line, text, expected) in [
            ("one\ntwo", 2, 2, "", "one"),
            ("one\ntwo", 1, 1, "", "two"),
            ("one\ntwo", 1, 2, "", ""),
            ("one\ntwo", 2, 1, "inserted", "one\ninserted\ntwo"),
            ("one\ntwo", 3, 2, "inserted", "one\ntwo\ninserted"),
            ("one\ntwo", 2, 2, "  \n    ", "one\n  \n    "),
        ] {
            let stream = CursorTabStream {
                is_multidiff_model: true,
                edits: vec![CursorTabCompletion {
                    range: Some(RangeToReplace {
                        start_line,
                        end_line,
                        ..Default::default()
                    }),
                    text: text.into(),
                    ..Default::default()
                }],
                ..Default::default()
            };
            assert_eq!(apply_multidiff_stream(&stream, original)?, expected);
        }
        Ok(())
    }

    #[test]
    fn multidiff_rejects_out_of_bounds_ranges() {
        let stream = CursorTabStream {
            is_multidiff_model: true,
            edits: vec![CursorTabCompletion {
                range: Some(RangeToReplace {
                    start_line: 20,
                    end_line: 20,
                    ..Default::default()
                }),
                text: "replacement".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(apply_multidiff_stream(&stream, "one\ntwo").is_err());
    }

    #[test]
    fn bracket_repairs_in_large_files_preserve_unchanged_objects() -> Result<()> {
        let prefix = "// preceding line\n".repeat(5_000);
        let suffix = "\n  { name: \"another object\" },".repeat(5_000);
        let fields = "    artists: [\"Zpecial\"],\n    name: \"復古 🎵\",\n    contributors: { song: {}, musicVideo: {} },";
        let neighbor = "  {\n    artists: [\"Zpecial\"],\n    name: \"復古 🎵\",\n  },";
        let original_window = format!("{fields}\n{neighbor}");
        let replacement = format!("  {{\n{fields}\n  }},\n{neighbor}");
        let original = format!("{prefix}{original_window}{suffix}");
        let mut buffer =
            language::TextBuffer::new(Default::default(), language::BufferId::new(1)?, original);
        let snapshot = buffer.snapshot();
        let start = prefix.len().to_point_utf16(snapshot);
        let end = (prefix.len() + original_window.len()).to_point_utf16(snapshot);
        let edits = anchors_from_prepared_edits(snapshot, &[(start..end, replacement.clone())]);

        assert_eq!(edits.len(), 2);
        assert!(edits.iter().all(|(range, _)| range.start == range.end));
        assert!(
            edits
                .iter()
                .all(|(_, text)| !text.contains("artists") && !text.contains("name"))
        );
        buffer.edit(edits.iter().cloned());
        assert_eq!(
            buffer.snapshot().text(),
            format!("{prefix}{replacement}{suffix}")
        );
        Ok(())
    }

    #[test]
    fn server_deletion_does_not_copy_unchanged_neighbor() -> Result<()> {
        let removed =
            "image: \"placeholder\",\ncontributors: {\n  song: {},\n  musicVideo: {},\n}, \n";
        let neighbor = "  {\n    artists: [\"Zpecial\"],\n    address: \"Hong Kong\",\n    coordinates: [22.313329, 114.168362],\n    name: \"復古\",";
        let mut buffer = language::TextBuffer::new(
            Default::default(),
            language::BufferId::new(1)?,
            format!("{removed}{neighbor}"),
        );
        let snapshot = buffer.snapshot();
        let range = PointUtf16::new(0, 0)..snapshot.max_point().to_point_utf16(snapshot);
        let edits = anchors_from_prepared_edits(snapshot, &[(range, neighbor.into())]);

        assert_eq!(edits.len(), 1);
        let (range, text) = edits.first().context("missing deletion")?;
        assert_eq!(
            range.start.to_offset(snapshot)..range.end.to_offset(snapshot),
            0..removed.len()
        );
        assert!(text.is_empty());
        buffer.edit(edits.iter().cloned());
        assert_eq!(buffer.snapshot().text(), neighbor);
        Ok(())
    }

    #[test]
    fn unchanged_rewrite_window_has_no_edits() -> Result<()> {
        let original = "  { name: \"復古 🎵\" },";
        let buffer =
            language::TextBuffer::new(Default::default(), language::BufferId::new(1)?, original);
        let snapshot = buffer.snapshot();
        let range = PointUtf16::new(0, 0)..snapshot.max_point().to_point_utf16(snapshot);
        assert!(anchors_from_prepared_edits(snapshot, &[(range, original.into())]).is_empty());
        Ok(())
    }

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
    fn maps_cursor_target_to_offset_inside_replacement() {
        let replacement =
            "     musicVideo: {},\n   }, \n  {\n    artists: [\"\"],\n    address: \"\",";
        let target = CursorPredictionTarget {
            relative_path: "src/app/common/locations.ts".into(),
            line_number_one_indexed: 30,
            expected_content: "artists: [\"\"],\naddress: \"\",".into(),
            should_retrigger_cpp: true,
        };

        assert_eq!(
            cursor_target_offset(26, replacement, "src/app/common/locations.ts", &target,),
            Some(replacement.len())
        );
        assert_eq!(
            cursor_target_offset(26, replacement, "other.ts", &target),
            None
        );
    }

    #[test]
    fn maps_cursor_target_inside_replacement_even_when_line_number_does_not_align() {
        let replacement =
            "     musicVideo: {},\n   }, \n  {\n    artists: [\"\"],\n    address: \"\",";
        let target = CursorPredictionTarget {
            relative_path: "src/app/common/locations.ts".into(),
            line_number_one_indexed: 18,
            expected_content: "artists: [\"\"],\naddress: \"\",".into(),
            should_retrigger_cpp: false,
        };

        assert_eq!(
            cursor_target_offset(27, replacement, "src/app/common/locations.ts", &target),
            Some(replacement.len())
        );
    }

    #[test]
    fn ignores_cursor_target_when_expected_content_does_not_match() {
        let replacement = "    artists: [\"The National\"],\n    address: \"Brooklyn\",";
        let target = CursorPredictionTarget {
            relative_path: "src/app/common/locations.ts".into(),
            line_number_one_indexed: 30,
            expected_content: "artists: [\"\"],\naddress: \"\",".into(),
            should_retrigger_cpp: true,
        };

        assert_eq!(
            cursor_target_offset(29, replacement, "src/app/common/locations.ts", &target),
            None
        );
    }

    #[test]
    fn skips_refresh_after_accept_when_retrigger_is_disabled() {
        assert!(!should_refresh_for_trigger(
            EditPredictionRequestTrigger::PredictionAccepted,
            false,
        ));
        assert!(should_refresh_for_trigger(
            EditPredictionRequestTrigger::PredictionAccepted,
            true,
        ));
        assert!(should_refresh_for_trigger(
            EditPredictionRequestTrigger::PredictionPartiallyAccepted,
            false,
        ));
        assert!(should_refresh_for_trigger(
            EditPredictionRequestTrigger::BufferEdit,
            false,
        ));
    }

    #[test]
    fn accumulate_stream_frames_stops_at_done_stream_and_strips_leading_eol() -> Result<()> {
        let first = StreamCppResponse {
            text: "\nfirst".into(),
            should_remove_leading_eol: Some(true),
            ..Default::default()
        };
        let done = StreamCppResponse {
            text: " edit".into(),
            done_stream: Some(true),
            cursor_prediction_target: Some(CursorPredictionTarget {
                relative_path: "src/main.rs".into(),
                line_number_one_indexed: 2,
                expected_content: "first edit".into(),
                should_retrigger_cpp: false,
            }),
            ..Default::default()
        };
        let stale = StreamCppResponse {
            text: "stale".into(),
            ..Default::default()
        };

        let stream = accumulate_stream_frames([
            ConnectFrame::Message(Box::new(first)),
            ConnectFrame::Message(Box::new(done)),
            ConnectFrame::Message(Box::new(stale)),
        ])?;

        assert_eq!(stream.edits.len(), 1);
        assert_eq!(stream.edits[0].text, "first edit");
        assert_eq!(
            stream
                .cursor_prediction_target
                .as_ref()
                .map(|target| target.should_retrigger_cpp),
            Some(false)
        );
        Ok(())
    }

    #[test]
    fn accumulate_stream_frames_finalizes_edits_at_done_edit() -> Result<()> {
        let first_range = StreamCppResponse {
            begin_edit: Some(true),
            range_to_replace: Some(RangeToReplace {
                start_line: 10,
                end_line: 11,
                start_column: 0,
                end_column: 0,
            }),
            binding_id: Some("edit-1".into()),
            ..Default::default()
        };
        let first_text = StreamCppResponse {
            text: "first".into(),
            ..Default::default()
        };
        let first_cursor = StreamCppResponse {
            cursor_prediction_target: Some(CursorPredictionTarget {
                relative_path: "src/main.rs".into(),
                line_number_one_indexed: 11,
                expected_content: "first edit".into(),
                should_retrigger_cpp: true,
            }),
            ..Default::default()
        };
        let first_done = StreamCppResponse {
            text: " edit".into(),
            done_edit: Some(true),
            ..Default::default()
        };
        let second_begin = StreamCppResponse {
            begin_edit: Some(true),
            range_to_replace: Some(RangeToReplace {
                start_line: 20,
                end_line: 21,
                start_column: 0,
                end_column: 1,
            }),
            text: "\n}".into(),
            should_remove_leading_eol: Some(true),
            binding_id: Some("edit-2".into()),
            done_edit: Some(true),
            done_stream: Some(true),
            ..Default::default()
        };

        let stream = accumulate_stream_frames([
            ConnectFrame::Message(Box::new(first_range)),
            ConnectFrame::Message(Box::new(first_text)),
            ConnectFrame::Message(Box::new(first_cursor)),
            ConnectFrame::Message(Box::new(first_done)),
            ConnectFrame::Message(Box::new(second_begin)),
        ])?;

        assert_eq!(stream.edits.len(), 2);
        assert_eq!(stream.edits[0].text, "first edit");
        assert_eq!(stream.edits[0].binding_id.as_deref(), Some("edit-1"));
        assert_eq!(
            stream.edits[0].range.map(|range| range.start_line),
            Some(10)
        );
        assert_eq!(
            stream.edits[0]
                .cursor_prediction_target
                .as_ref()
                .map(|target| target.line_number_one_indexed),
            Some(11)
        );
        assert_eq!(stream.edits[1].text, "}");
        assert_eq!(stream.edits[1].binding_id.as_deref(), Some("edit-2"));
        assert_eq!(
            stream.edits[1].range.map(|range| range.start_line),
            Some(20)
        );
        assert_eq!(
            stream
                .cursor_prediction_target
                .as_ref()
                .map(|target| target.line_number_one_indexed),
            Some(11)
        );
        Ok(())
    }

    #[test]
    fn accumulate_stream_frames_keeps_cursor_target_between_edits() -> Result<()> {
        let first = StreamCppResponse {
            range_to_replace: Some(RangeToReplace {
                start_line: 1,
                end_line: 1,
                start_column: 0,
                end_column: 1,
            }),
            text: "one".into(),
            done_edit: Some(true),
            ..Default::default()
        };
        let between = StreamCppResponse {
            cursor_prediction_target: Some(CursorPredictionTarget {
                relative_path: "src/main.rs".into(),
                line_number_one_indexed: 4,
                expected_content: "two".into(),
                should_retrigger_cpp: false,
            }),
            ..Default::default()
        };
        let second = StreamCppResponse {
            begin_edit: Some(true),
            range_to_replace: Some(RangeToReplace {
                start_line: 3,
                end_line: 3,
                start_column: 0,
                end_column: 1,
            }),
            text: "two".into(),
            done_edit: Some(true),
            done_stream: Some(true),
            ..Default::default()
        };

        let stream = accumulate_stream_frames([
            ConnectFrame::Message(Box::new(first)),
            ConnectFrame::Message(Box::new(between)),
            ConnectFrame::Message(Box::new(second)),
        ])?;

        assert_eq!(stream.edits.len(), 2);
        assert!(stream.edits[0].cursor_prediction_target.is_none());
        assert!(stream.edits[1].cursor_prediction_target.is_none());
        assert_eq!(
            stream
                .cursor_prediction_target
                .as_ref()
                .map(|target| (target.line_number_one_indexed, target.should_retrigger_cpp)),
            Some((4, false))
        );
        Ok(())
    }

    #[test]
    fn should_remove_leading_eol_applies_to_the_following_edit() -> Result<()> {
        let flag = StreamCppResponse {
            should_remove_leading_eol: Some(true),
            ..Default::default()
        };
        let begin = StreamCppResponse {
            begin_edit: Some(true),
            range_to_replace: Some(RangeToReplace {
                start_line: 8,
                end_line: 8,
                start_column: 0,
                end_column: 1,
            }),
            text: "\n}".into(),
            done_edit: Some(true),
            done_stream: Some(true),
            ..Default::default()
        };

        let stream = accumulate_stream_frames([
            ConnectFrame::Message(Box::new(flag)),
            ConnectFrame::Message(Box::new(begin)),
        ])?;

        assert_eq!(stream.edits.len(), 1);
        assert_eq!(stream.edits[0].text, "}");
        Ok(())
    }

    #[test]
    fn current_file_history_path_matches_current_file() {
        assert_eq!(
            cursor_relative_history_path(
                "cantopop-map/src/app/common/locations.ts",
                Some("cantopop-map/src/app/common/locations.ts"),
                "src/app/common/locations.ts",
            ),
            "src/app/common/locations.ts"
        );
        assert_eq!(
            cursor_relative_history_path(
                "src/app/common/locations.ts",
                Some("cantopop-map/src/app/common/locations.ts"),
                "src/app/common/locations.ts",
            ),
            "src/app/common/locations.ts"
        );
        assert_eq!(
            cursor_relative_history_path(
                "cantopop-map/src/other.ts",
                Some("cantopop-map/src/app/common/locations.ts"),
                "src/app/common/locations.ts",
            ),
            "src/other.ts"
        );
        assert_eq!(
            cursor_relative_history_path("external/lib.rs", None, "src/main.rs"),
            "external/lib.rs"
        );
    }

    #[test]
    fn non_multidiff_keeps_text_with_range_after_done_edit() -> Result<()> {
        let stream = accumulate_stream_frames(
            [
                StreamCppResponse {
                    model_info: Some(ModelInfo {
                        is_fused_cursor_prediction_model: true,
                        is_multidiff_model: false,
                    }),
                    text: "  {\n    name: \"復古\",\n  },".into(),
                    ..Default::default()
                },
                StreamCppResponse {
                    done_edit: Some(true),
                    ..Default::default()
                },
                StreamCppResponse {
                    range_to_replace: Some(RangeToReplace {
                        start_line: 2,
                        end_line: 2,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                StreamCppResponse {
                    done_stream: Some(true),
                    ..Default::default()
                },
            ]
            .map(|message| ConnectFrame::Message(Box::new(message))),
        )?;

        assert_eq!(stream.edits.len(), 1);
        let edit = stream.edits.first().context("missing replacement")?;
        let (range, replacement) = edit.replacement(PointUtf16::new(1, 4), "    ")?;
        let mut document = language::Rope::from(
            "const locations = [\n    name: \"復古\",\n  { name: \"next\" },\n];",
        );
        let start = document.clip_point_utf16(range.start, Bias::Left);
        let end = document.clip_point_utf16(range.end, Bias::Right);
        document.replace(
            document.point_utf16_to_offset(start)..document.point_utf16_to_offset(end),
            replacement,
        );
        assert_eq!(
            document.to_string(),
            "const locations = [\n  {\n    name: \"復古\",\n  },\n  { name: \"next\" },\n];"
        );
        Ok(())
    }

    #[test]
    fn legacy_completion_replaces_from_suggestion_start_line_to_cursor() -> Result<()> {
        let completion = CursorTabCompletion {
            text: "console.log(\"hello world\")".into(),
            range: None,
            suggestion_start_line: Some(3),
            cursor_prediction_target: None,
            ..Default::default()
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
            cursor_prediction_target: None,
            ..Default::default()
        };

        let (range, text) = completion.replacement(PointUtf16::new(3, 11), "ignored")?;

        assert_eq!(range.start.0, PointUtf16::new(0, 0));
        assert_eq!(range.end.0, PointUtf16::new(3, u32::MAX));
        assert_eq!(text, "replacement");
        Ok(())
    }

    #[test]
    fn cursor_line_range_maps_one_based_inclusive_lines() -> Result<()> {
        let completion = CursorTabCompletion {
            text: "replacement loop".into(),
            range: Some(RangeToReplace {
                start_line: 11,
                end_line: 14,
                start_column: 0,
                end_column: 0,
            }),
            suggestion_start_line: None,
            cursor_prediction_target: None,
            ..Default::default()
        };

        let (range, text) = completion.replacement(PointUtf16::new(13, 1), "}")?;

        assert_eq!(range.start.0, PointUtf16::new(10, 0));
        assert_eq!(range.end.0, PointUtf16::new(13, u32::MAX));
        assert_eq!(text, "replacement loop");
        Ok(())
    }

    #[test]
    fn explicit_line_range_takes_precedence_over_whitespace_prefix() -> Result<()> {
        let completion = CursorTabCompletion {
            text: "    D: Infinity,\n    finish: Infinity\n}".into(),
            range: Some(RangeToReplace {
                start_line: 16,
                end_line: 17,
                start_column: 0,
                end_column: 0,
            }),
            suggestion_start_line: None,
            cursor_prediction_target: None,
            ..Default::default()
        };

        let (range, text) = completion.replacement(PointUtf16::new(16, 2), "  ")?;

        assert_eq!(range.start.0, PointUtf16::new(15, 0));
        assert_eq!(range.end.0, PointUtf16::new(16, u32::MAX));
        assert_eq!(text, "    D: Infinity,\n    finish: Infinity\n}");
        Ok(())
    }

    #[test]
    fn full_line_completion_without_range_inserts_only_unmatched_suffix() -> Result<()> {
        let completion = CursorTabCompletion {
            text: "console.log(\"hello world\");".into(),
            range: None,
            suggestion_start_line: None,
            cursor_prediction_target: None,
            ..Default::default()
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
            cursor_prediction_target: None,
            ..Default::default()
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
    fn exact_replacement_is_minimized_to_empty_insertion() {
        let cursor = PointUtf16::new(3, 10);
        let (range, text) = minimize_replacement(
            PointUtf16::new(3, 0)..cursor,
            cursor,
            "console.lo",
            "console.lo",
        );

        assert_eq!(range, cursor..cursor);
        assert!(text.is_empty());
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
            cursor_prediction_target: None,
            ..Default::default()
        };
        let cursor = PointUtf16::new(0, 8);

        let (range, text) = completion.replacement(cursor, "console.")?;

        assert_eq!(range.start.0, cursor);
        assert_eq!(range.end.0, cursor);
        assert_eq!(text, "log(\"Hello World\");");
        Ok(())
    }

    #[test]
    fn reversed_explicit_range_for_multiline_continuation_inserts_at_cursor() -> Result<()> {
        let completion = CursorTabCompletion {
            text: "\nget_data(\"https://example.com/posts\");".into(),
            range: Some(RangeToReplace {
                start_line: 6,
                start_column: 7,
                end_line: 0,
                end_column: 0,
            }),
            suggestion_start_line: None,
            cursor_prediction_target: None,
            ..Default::default()
        };
        let cursor = PointUtf16::new(6, 55);

        let (range, text) =
            completion.replacement(cursor, "get_data(\"https://example.com/posts\");")?;

        assert_eq!(range.start.0, cursor);
        assert_eq!(range.end.0, cursor);
        assert_eq!(text, "\nget_data(\"https://example.com/posts\");");
        Ok(())
    }

    #[test]
    fn repeated_current_line_is_not_a_completion() {
        let current_line = "get_data(\"https://example.com/posts\");";

        assert!(repeats_current_line(
            &format!("\n{current_line}\n{current_line}\n{current_line}"),
            current_line,
        ));
        assert!(!repeats_current_line(
            &format!("\n{current_line}\nconsole.log(\"done\");"),
            current_line,
        ));
    }

    #[test]
    fn line_breaks_and_indentation_are_not_a_completion() {
        assert!(contains_only_line_breaks_and_indentation("\n\n    \n\t"));
        assert!(contains_only_line_breaks_and_indentation("\r\n  \r\n"));
        assert!(!contains_only_line_breaks_and_indentation("    "));
        assert!(!contains_only_line_breaks_and_indentation("\nvalue"));
    }

    #[test]
    fn suppresses_reversed_range_completion_that_echoes_document_at_end() {
        let contents = "function getRandomNumber(min, max) {\n    return max;\n}\n\nconsole.log(getRandomNumber(1, 10));";
        let mut completion = CursorTabCompletion {
            text: contents.into(),
            range: Some(RangeToReplace {
                start_line: 1,
                start_column: 5,
                end_line: 0,
                end_column: 0,
            }),
            suggestion_start_line: None,
            cursor_prediction_target: None,
            ..Default::default()
        };

        completion.normalize_document_echo(contents, true);

        assert!(completion.text.is_empty());
    }

    #[test]
    fn suppresses_reversed_range_completion_that_echoes_document_twice_at_end() {
        let contents = "function getRandomNumber(min, max) {\n    return max;\n}\n\nconsole.log(getRandomNumber(1, 10));";
        let mut completion = CursorTabCompletion {
            text: format!("{contents}\n\n{contents}"),
            range: Some(RangeToReplace {
                start_line: 1,
                start_column: 5,
                end_line: 0,
                end_column: 0,
            }),
            suggestion_start_line: None,
            cursor_prediction_target: None,
            ..Default::default()
        };

        completion.normalize_document_echo(contents, true);

        assert!(completion.text.is_empty());
    }

    #[test]
    fn strips_echoed_document_suffix_without_replacing_existing_text() {
        let current_line = "get_data(\"https://example.com/posts\");";
        let contents =
            format!("async function get_data() {{\n    return response;\n}}\n\n{current_line}");
        let mut completion = CursorTabCompletion {
            text: format!("}}\n\n{current_line}\n{current_line}\n{current_line}"),
            range: Some(RangeToReplace {
                start_line: 5,
                start_column: 8,
                end_line: 0,
                end_column: 0,
            }),
            suggestion_start_line: None,
            cursor_prediction_target: None,
            ..Default::default()
        };

        completion.normalize_document_echo(&contents, true);

        assert_eq!(completion.text, format!("\n{current_line}\n{current_line}"));
        assert!(repeats_current_line(&completion.text, current_line));
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

    #[test]
    fn cursor_tab_predictions_stay_inline_with_completion_menu() {
        assert!(CursorTabEditPredictionDelegate::show_predictions_in_menu());
        assert!(CursorTabEditPredictionDelegate::show_predictions_inline_with_menu());
    }

    #[test]
    fn constructs_file_sync_service_urls() {
        assert_eq!(
            file_sync_api_url(CURSOR_TAB_API_URL, "FSSyncFile").unwrap(),
            "https://us-only.gcpp.cursor.sh/aiserver.v1.FileSyncService/FSSyncFile"
        );
        assert_eq!(
            stream_cpp_api_url(CURSOR_TAB_API_URL).unwrap(),
            "https://us-only.gcpp.cursor.sh/aiserver.v1.AiService/StreamCpp"
        );
    }

    #[test]
    fn computes_file_sync_update_in_utf16_offsets() {
        let update = single_file_update("a😀c", "a😀longer c").unwrap();

        assert_eq!(update.start_position, 3);
        assert_eq!(update.end_position, 3);
        assert_eq!(update.change_length, 0);
        assert_eq!(update.replaced_string, "longer ");
    }

    #[test]
    fn computes_file_sync_replacement() {
        let update = single_file_update("hello world", "hello rust").unwrap();

        assert_eq!(update.start_position, 6);
        assert_eq!(update.end_position, 11);
        assert_eq!(update.change_length, 5);
        assert_eq!(update.replaced_string, "rust");
    }
}
