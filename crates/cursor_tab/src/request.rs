use anyhow::{Result, anyhow, bail};
use prost::{Enumeration, Message};

#[derive(Clone, PartialEq, Message)]
pub struct StreamCppRequest {
    #[prost(message, optional, tag = "1")]
    pub current_file: Option<CurrentFileInfo>,
    #[prost(string, repeated, tag = "2")]
    pub diff_history: Vec<String>,
    #[prost(string, optional, tag = "3")]
    pub model_name: Option<String>,
    #[prost(message, optional, tag = "4")]
    pub linter_errors: Option<LinterErrors>,
    #[prost(string, repeated, tag = "5")]
    pub diff_history_keys: Vec<String>,
    #[prost(bool, optional, tag = "6")]
    pub give_debug_output: Option<bool>,
    #[prost(message, repeated, tag = "7")]
    pub file_diff_histories: Vec<FileDiffHistory>,
    #[prost(message, repeated, tag = "8")]
    pub merged_diff_histories: Vec<FileDiffHistory>,
    #[prost(message, repeated, tag = "9")]
    pub block_diff_patches: Vec<BlockDiffPatch>,
    #[prost(bool, optional, tag = "10")]
    pub is_nightly: Option<bool>,
    #[prost(bool, optional, tag = "11")]
    pub is_debug: Option<bool>,
    #[prost(bool, optional, tag = "12")]
    pub immediately_ack: Option<bool>,
    #[prost(message, repeated, tag = "13")]
    pub context_items: Vec<ContextItem>,
    #[prost(message, repeated, tag = "14")]
    pub parameter_hints: Vec<ParameterHint>,
    #[prost(message, repeated, tag = "15")]
    pub lsp_contexts: Vec<LspContext>,
    #[prost(message, optional, tag = "16")]
    pub cpp_intent_info: Option<CppIntentInfo>,
    #[prost(bool, optional, tag = "17")]
    pub enable_more_context: Option<bool>,
    #[prost(string, optional, tag = "18")]
    pub workspace_id: Option<String>,
    #[prost(message, repeated, tag = "19")]
    pub additional_files: Vec<AdditionalFile>,
    #[prost(enumeration = "ControlToken", optional, tag = "20")]
    pub control_token: Option<i32>,
    #[prost(double, optional, tag = "21")]
    pub client_time: Option<f64>,
    #[prost(message, repeated, tag = "22")]
    pub filesync_updates: Vec<FilesyncUpdate>,
    #[prost(double, tag = "23")]
    pub time_since_request_start: f64,
    #[prost(double, tag = "24")]
    pub time_at_request_send: f64,
    #[prost(double, optional, tag = "25")]
    pub client_timezone_offset: Option<f64>,
    #[prost(message, optional, tag = "26")]
    pub lsp_suggested_items: Option<LspSuggestedItems>,
    #[prost(bool, optional, tag = "27")]
    pub supports_cpt: Option<bool>,
    #[prost(bool, optional, tag = "28")]
    pub supports_crlf_cpt: Option<bool>,
    #[prost(message, repeated, tag = "29")]
    pub code_results: Vec<CodeResult>,
}

#[derive(Clone, PartialEq, Message)]
pub struct CurrentFileInfo {
    #[prost(string, tag = "1")]
    pub relative_workspace_path: String,
    #[prost(string, tag = "2")]
    pub contents: String,
    #[prost(message, optional, tag = "3")]
    pub cursor_position: Option<CursorPosition>,
    #[prost(message, repeated, tag = "4")]
    pub dataframes: Vec<DataFrame>,
    #[prost(string, tag = "5")]
    pub language_id: String,
    #[prost(message, optional, tag = "6")]
    pub selection: Option<Selection>,
    #[prost(message, repeated, tag = "7")]
    pub diagnostics: Vec<Diagnostic>,
    #[prost(int32, tag = "8")]
    pub total_number_of_lines: i32,
    #[prost(int32, tag = "9")]
    pub contents_start_at_line: i32,
    #[prost(message, repeated, tag = "10")]
    pub top_chunks: Vec<TopChunk>,
    #[prost(int32, optional, tag = "11")]
    pub alternative_version_id: Option<i32>,
    #[prost(int32, optional, tag = "14")]
    pub file_version: Option<i32>,
    #[prost(int32, repeated, tag = "15")]
    pub cell_start_lines: Vec<i32>,
    #[prost(message, repeated, tag = "16")]
    pub cells: Vec<Cell>,
    #[prost(string, optional, tag = "17")]
    pub sha_256_hash: Option<String>,
    #[prost(bool, tag = "18")]
    pub rely_on_filesync: bool,
    #[prost(string, tag = "19")]
    pub workspace_root_path: String,
    #[prost(string, optional, tag = "20")]
    pub line_ending: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Message)]
pub struct CursorPosition {
    #[prost(int32, tag = "1")]
    pub line: i32,
    #[prost(int32, tag = "2")]
    pub column: i32,
}

#[derive(Clone, Copy, PartialEq, Eq, Message)]
pub struct Selection {
    #[prost(int32, tag = "1")]
    pub start_line: i32,
    #[prost(int32, tag = "2")]
    pub start_column: i32,
    #[prost(int32, tag = "3")]
    pub end_line: i32,
    #[prost(int32, tag = "4")]
    pub end_column: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct LinterErrors {
    #[prost(string, tag = "1")]
    pub relative_workspace_path: String,
    #[prost(message, repeated, tag = "2")]
    pub errors: Vec<LinterError>,
    #[prost(string, tag = "3")]
    pub file_contents: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct LinterError {
    #[prost(string, tag = "1")]
    pub message: String,
    #[prost(message, optional, tag = "2")]
    pub range: Option<CodeRange>,
    #[prost(string, optional, tag = "3")]
    pub source: Option<String>,
    #[prost(message, repeated, tag = "4")]
    pub related_information: Vec<RelatedInformation>,
    #[prost(enumeration = "Severity", optional, tag = "5")]
    pub severity: Option<i32>,
    #[prost(bool, optional, tag = "6")]
    pub is_stale: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct RelatedInformation {
    #[prost(string, tag = "1")]
    pub message: String,
    #[prost(string, tag = "2")]
    pub relative_workspace_path: String,
    #[prost(string, repeated, tag = "3")]
    pub relevant_lines: Vec<String>,
    #[prost(int32, tag = "4")]
    pub start_line: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct FileDiffHistory {
    #[prost(string, tag = "1")]
    pub file_name: String,
    #[prost(string, repeated, tag = "2")]
    pub diff_history: Vec<String>,
    #[prost(double, repeated, tag = "3")]
    pub diff_history_timestamps: Vec<f64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct CppIntentInfo {
    #[prost(string, tag = "1")]
    pub source: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct AdditionalFile {
    #[prost(string, tag = "1")]
    pub relative_workspace_path: String,
    #[prost(bool, tag = "2")]
    pub is_open: bool,
    #[prost(string, repeated, tag = "3")]
    pub visible_range_content: Vec<String>,
    #[prost(double, optional, tag = "4")]
    pub last_viewed_at: Option<f64>,
    #[prost(int32, repeated, tag = "5")]
    pub start_line_number_one_indexed: Vec<i32>,
    #[prost(message, repeated, tag = "6")]
    pub visible_ranges: Vec<LineRange>,
}

#[derive(Clone, Copy, PartialEq, Eq, Message)]
pub struct LineRange {
    #[prost(int32, tag = "1")]
    pub start_line_number: i32,
    #[prost(int32, tag = "2")]
    pub end_line_number_inclusive: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct LspSuggestedItems {
    #[prost(message, repeated, tag = "1")]
    pub suggestions: Vec<LspSuggestion>,
}

#[derive(Clone, PartialEq, Message)]
pub struct CodeResult {
    #[prost(message, optional, tag = "1")]
    pub code_block: Option<CodeBlock>,
    #[prost(float, tag = "2")]
    pub score: f32,
}

#[derive(Clone, PartialEq, Message)]
pub struct CodeBlock {
    #[prost(string, tag = "1")]
    pub relative_workspace_path: String,
    #[prost(string, optional, tag = "2")]
    pub file_contents: Option<String>,
    #[prost(message, optional, tag = "3")]
    pub range: Option<CodeRange>,
    #[prost(string, tag = "4")]
    pub contents: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Message)]
pub struct CodeRange {
    #[prost(message, optional, tag = "1")]
    pub start_position: Option<Position>,
    #[prost(message, optional, tag = "2")]
    pub end_position: Option<Position>,
}

#[derive(Clone, Copy, PartialEq, Eq, Message)]
pub struct Position {
    #[prost(int32, tag = "1")]
    pub line: i32,
    #[prost(int32, tag = "2")]
    pub column: i32,
}

macro_rules! empty_messages {
    ($($name:ident),+ $(,)?) => {
        $(
            #[derive(Clone, Copy, PartialEq, Eq, Message)]
            pub struct $name {}
        )+
    };
}

empty_messages!(
    DataFrame,
    Diagnostic,
    TopChunk,
    Cell,
    BlockDiffPatch,
    ContextItem,
    ParameterHint,
    LspContext,
    FilesyncUpdate,
    LspSuggestion,
);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Enumeration)]
#[repr(i32)]
pub enum ControlToken {
    Unspecified = 0,
    Quiet = 1,
    Loud = 2,
    Op = 3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Enumeration)]
#[repr(i32)]
pub enum Severity {
    Unspecified = 0,
}

pub struct StreamCppRequestInput {
    pub relative_workspace_path: String,
    pub workspace_root_path: String,
    pub contents: String,
    pub cursor_position: CursorPosition,
    pub selection: Option<Selection>,
    pub language_id: String,
    pub file_version: Option<i32>,
    pub sha_256_hash: Option<String>,
    pub linter_errors: Vec<LinterError>,
    pub file_diff_histories: Vec<FileDiffHistory>,
    pub merged_diff_histories: Vec<FileDiffHistory>,
    pub additional_files: Vec<AdditionalFile>,
    pub code_results: Vec<CodeResult>,
    pub model_name: Option<String>,
    pub intent_source: Option<String>,
    pub workspace_id: Option<String>,
    pub client_time: f64,
    pub time_since_request_start: f64,
    pub time_at_request_send: f64,
    pub client_timezone_offset: Option<f64>,
    pub supports_cpt: bool,
    pub supports_crlf_cpt: bool,
}

impl StreamCppRequestInput {
    pub fn build(mut self) -> Result<StreamCppRequest> {
        if self.relative_workspace_path.is_empty() {
            bail!("current file path must not be empty");
        }
        if self.language_id.is_empty() {
            bail!("current file language must not be empty");
        }
        if !self.client_time.is_finite()
            || !self.time_since_request_start.is_finite()
            || !self.time_at_request_send.is_finite()
        {
            bail!("request timings must be finite");
        }
        if self.time_since_request_start < 0.0 {
            bail!("time since request start must not be negative");
        }

        let total_number_of_lines = self.contents.split('\n').count();
        let total_number_of_lines = i32::try_from(total_number_of_lines)
            .map_err(|_| anyhow!("current file has too many lines"))?;
        validate_position(self.cursor_position, total_number_of_lines, "cursor")?;
        if let Some(selection) = self.selection {
            validate_selection(selection, total_number_of_lines)?;
        }

        let line_ending = if self.contents.contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        };
        if let Some(contents) = window_current_file(
            &self.contents,
            usize::try_from(self.cursor_position.line)?,
            line_ending,
        ) {
            log::debug!(
                "Cursor Tab windowed current file: original_bytes={}, sent_bytes={}, cursor_line={}, total_lines={total_number_of_lines}",
                self.contents.len(),
                contents.len(),
                self.cursor_position.line,
            );
            self.contents = contents;
            // This request carries its contents directly; the full-file hash no
            // longer describes the windowed contents and must not accompany them.
            self.sha_256_hash = None;
        }
        let control_token = (self.intent_source.as_deref() == Some("manual_trigger"))
            .then_some(ControlToken::Op as i32);
        let linter_errors = (!self.linter_errors.is_empty()).then(|| LinterErrors {
            relative_workspace_path: self.relative_workspace_path.clone(),
            errors: self.linter_errors,
            file_contents: String::new(),
        });

        Ok(StreamCppRequest {
            current_file: Some(CurrentFileInfo {
                relative_workspace_path: self.relative_workspace_path,
                contents: self.contents,
                cursor_position: Some(self.cursor_position),
                language_id: self.language_id,
                selection: self.selection,
                total_number_of_lines,
                contents_start_at_line: 0,
                file_version: self.file_version,
                sha_256_hash: self.sha_256_hash,
                rely_on_filesync: false,
                workspace_root_path: self.workspace_root_path,
                line_ending: Some(line_ending.into()),
                ..Default::default()
            }),
            model_name: self.model_name,
            control_token,
            linter_errors,
            file_diff_histories: self.file_diff_histories,
            merged_diff_histories: self.merged_diff_histories,
            cpp_intent_info: self.intent_source.map(|source| CppIntentInfo { source }),
            workspace_id: self.workspace_id,
            additional_files: self.additional_files,
            client_time: Some(self.client_time),
            time_since_request_start: self.time_since_request_start,
            time_at_request_send: self.time_at_request_send,
            client_timezone_offset: self.client_timezone_offset,
            supports_cpt: Some(self.supports_cpt),
            supports_crlf_cpt: Some(self.supports_crlf_cpt),
            code_results: self.code_results,
            ..Default::default()
        })
    }
}

fn window_current_file(contents: &str, cursor_line: usize, line_ending: &str) -> Option<String> {
    const MIN_UTF16_LENGTH: usize = 50_000;
    const CONTEXT_LINE_COUNT: usize = 600;
    if contents.encode_utf16().take(MIN_UTF16_LENGTH).count() < MIN_UTF16_LENGTH {
        return None;
    }
    let lines = contents.split(line_ending).collect::<Vec<_>>();
    let start = cursor_line
        .saturating_sub(CONTEXT_LINE_COUNT / 2)
        .min(lines.len().saturating_sub(CONTEXT_LINE_COUNT));
    let end = (start + CONTEXT_LINE_COUNT).min(lines.len());
    if start == 0 && end == lines.len() {
        return None;
    }
    // Cursor blanks distant lines rather than removing them, so cursor and
    // response coordinates remain absolute even for a window near EOF.
    Some(
        lines
            .into_iter()
            .enumerate()
            .map(|(row, line)| {
                if (start..end).contains(&row) {
                    line
                } else {
                    ""
                }
            })
            .collect::<Vec<_>>()
            .join(line_ending),
    )
}

fn validate_selection(selection: Selection, line_count: i32) -> Result<()> {
    let start = CursorPosition {
        line: selection.start_line,
        column: selection.start_column,
    };
    let end = CursorPosition {
        line: selection.end_line,
        column: selection.end_column,
    };
    validate_position(start, line_count, "selection start")?;
    validate_position(end, line_count, "selection end")?;
    if (selection.end_line, selection.end_column) < (selection.start_line, selection.start_column) {
        bail!("selection end must not precede selection start");
    }
    Ok(())
}

fn validate_position(position: CursorPosition, line_count: i32, name: &str) -> Result<()> {
    if position.line < 0 || position.line >= line_count || position.column < 0 {
        bail!("{name} is outside the current file");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_input(contents: &str) -> StreamCppRequestInput {
        StreamCppRequestInput {
            relative_workspace_path: "src/main.rs".into(),
            workspace_root_path: "/worktree".into(),
            contents: contents.into(),
            cursor_position: CursorPosition { line: 0, column: 0 },
            selection: None,
            language_id: "rust".into(),
            file_version: Some(7),
            sha_256_hash: None,
            linter_errors: Vec::new(),
            file_diff_histories: Vec::new(),
            merged_diff_histories: Vec::new(),
            additional_files: Vec::new(),
            code_results: Vec::new(),
            model_name: Some("fast".into()),
            intent_source: Some("line_change".into()),
            workspace_id: Some("workspace".into()),
            client_time: 1000.0,
            time_since_request_start: 4.0,
            time_at_request_send: 1004.0,
            client_timezone_offset: Some(-600.0),
            supports_cpt: false,
            supports_crlf_cpt: false,
        }
    }

    #[test]
    fn builds_current_file_and_linter_context_from_same_snapshot() {
        let contents = "fn main() {\r\n    unknown();\r\n}\r\n";
        let mut input = request_input(contents);
        input.cursor_position = CursorPosition { line: 1, column: 4 };
        input.selection = Some(Selection {
            start_line: 1,
            start_column: 4,
            end_line: 1,
            end_column: 11,
        });
        input.linter_errors.push(LinterError {
            message: "cannot find function `unknown`".into(),
            range: Some(CodeRange {
                start_position: Some(Position { line: 1, column: 4 }),
                end_position: Some(Position {
                    line: 1,
                    column: 11,
                }),
            }),
            source: Some("rustc".into()),
            related_information: Vec::new(),
            severity: None,
            is_stale: Some(false),
        });

        let request = input.build().unwrap();
        let current_file = request.current_file.unwrap();
        assert_eq!(current_file.total_number_of_lines, 4);
        assert_eq!(current_file.line_ending.as_deref(), Some("\r\n"));
        assert_eq!(current_file.language_id, "rust");
        assert_eq!(current_file.selection.unwrap().end_column, 11);
        let linter_errors = request.linter_errors.unwrap();
        assert!(linter_errors.file_contents.is_empty());
        assert_eq!(
            linter_errors.errors[0]
                .range
                .and_then(|range| range.end_position)
                .map(|position| position.column),
            Some(11)
        );
    }

    #[test]
    fn builds_current_file_without_diagnostics() {
        let contents = "function main() {}\n";
        let mut input = request_input(contents);
        input.relative_workspace_path = "main.js".into();
        input.language_id = "javascript".into();
        input.cursor_position = CursorPosition { line: 0, column: 8 };
        input.sha_256_hash = Some("contents-hash".into());

        let request = input.build().unwrap();
        assert!(request.linter_errors.is_none());
        let current_file = request.current_file.unwrap();
        assert_eq!(current_file.relative_workspace_path, "main.js");
        assert_eq!(current_file.contents, contents);
        assert_eq!(current_file.language_id, "javascript");
        assert_eq!(current_file.file_version, Some(7));
        assert_eq!(current_file.sha_256_hash.as_deref(), Some("contents-hash"));
        assert!(current_file.diagnostics.is_empty());
    }

    #[test]
    fn rejects_invalid_positions_and_timings() {
        let mut input = request_input("fn main() {}\n");
        input.cursor_position.line = 2;
        assert!(input.build().is_err());

        let mut input = request_input("fn main() {}\n");
        input.time_at_request_send = f64::NAN;
        assert!(input.build().is_err());
    }

    #[test]
    fn large_file_windows_preserve_absolute_coordinates() -> Result<()> {
        for line_ending in ["\n", "\r\n"] {
            let lines = (0..1_000)
                .map(|row| format!("{row}: {}", "🎵".repeat(30)))
                .collect::<Vec<_>>();
            let contents = lines.join(line_ending);
            for (cursor_line, expected_window) in [
                (0, 0..600),
                (28, 0..600),
                (500, 200..800),
                (999, 400..1_000),
            ] {
                let mut input = request_input(&contents);
                input.cursor_position = CursorPosition {
                    line: cursor_line,
                    column: 3,
                };
                input.sha_256_hash = Some("full-file-hash".into());
                let request = input.build()?;
                let file = request
                    .current_file
                    .ok_or_else(|| anyhow!("missing current file"))?;
                assert_eq!(
                    file.cursor_position,
                    Some(CursorPosition {
                        line: cursor_line,
                        column: 3
                    })
                );
                assert_eq!(file.contents_start_at_line, 0);
                assert_eq!(file.total_number_of_lines, 1_000);
                assert_eq!(file.line_ending.as_deref(), Some(line_ending));
                assert!(file.sha_256_hash.is_none());
                assert!(!file.rely_on_filesync);
                let sent_lines = file.contents.split(line_ending).collect::<Vec<_>>();
                assert_eq!(sent_lines.len(), lines.len());
                for (row, (sent, original)) in sent_lines.iter().zip(&lines).enumerate() {
                    assert_eq!(
                        *sent,
                        if expected_window.contains(&row) {
                            original.as_str()
                        } else {
                            ""
                        }
                    );
                }
            }
        }
        Ok(())
    }

    #[test]
    fn window_threshold_uses_utf16_units_instead_of_bytes() -> Result<()> {
        let contents = vec!["復古".repeat(12); 1_000].join("\n");
        assert!(contents.len() > 50_000);
        let request = request_input(&contents).build()?;
        let file = request
            .current_file
            .ok_or_else(|| anyhow!("missing current file"))?;
        assert_eq!(file.contents, contents);
        Ok(())
    }

    #[test]
    fn manual_trigger_requests_operation_control_token() -> Result<()> {
        for (source, expected) in [
            ("manual_trigger", Some(ControlToken::Op as i32)),
            ("editor_change", None),
        ] {
            let mut input = request_input("const values = [\n");
            input.intent_source = Some(source.into());
            assert_eq!(input.build()?.control_token, expected);
        }
        Ok(())
    }

    #[test]
    fn request_round_trips_through_protobuf() {
        let request = request_input("fn main() {}\n").build().unwrap();
        let encoded = request.encode_to_vec();
        assert_eq!(
            StreamCppRequest::decode(encoded.as_slice()).unwrap(),
            request
        );
    }

    #[test]
    fn code_block_encodes_range_and_contents_at_cursor_field_tags() {
        let code_block = CodeBlock {
            relative_workspace_path: "src/app.ts".into(),
            file_contents: None,
            range: Some(CodeRange {
                start_position: Some(Position {
                    line: 26,
                    column: 0,
                }),
                end_position: Some(Position {
                    line: 28,
                    column: 4,
                }),
            }),
            contents: "artists: [Zpecial],".into(),
        };

        let encoded = code_block.encode_to_vec();
        assert_eq!(
            protobuf_field_numbers(&encoded),
            [1, 3, 4],
            "Cursor CodeBlock fields are path=1, range=3, contents=4"
        );
        assert_eq!(CodeBlock::decode(encoded.as_slice()).unwrap(), code_block);

        let mut input = request_input("export const locations = [\n  {\n");
        input.code_results.push(CodeResult {
            code_block: Some(code_block),
            score: 1.0,
        });
        let request = input.build().unwrap();
        let encoded = request.encode_to_vec();
        assert_eq!(
            StreamCppRequest::decode(encoded.as_slice()).unwrap(),
            request
        );
    }

    fn protobuf_field_numbers(bytes: &[u8]) -> Vec<u32> {
        let mut field_numbers = Vec::new();
        let mut rest = bytes;
        while let Some((tag, remaining)) = rest.split_first() {
            let field_number = u32::from(tag >> 3);
            let wire_type = tag & 0x07;
            field_numbers.push(field_number);
            rest = match wire_type {
                0 => skip_varint(remaining),
                1 => remaining.get(8..).unwrap_or(&[]),
                2 => {
                    let (length, after_length) = remaining
                        .split_first()
                        .map_or((0, remaining), |(length, rest)| (*length as usize, rest));
                    after_length.get(length..).unwrap_or(&[])
                }
                5 => remaining.get(4..).unwrap_or(&[]),
                _ => &[],
            };
        }
        field_numbers
    }

    fn skip_varint(bytes: &[u8]) -> &[u8] {
        let skip = bytes
            .iter()
            .position(|byte| byte & 0x80 == 0)
            .map(|index| index + 1)
            .unwrap_or(bytes.len());
        bytes.get(skip..).unwrap_or(&[])
    }
}
