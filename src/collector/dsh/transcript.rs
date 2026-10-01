//! Reducer that folds DSH session events into the facts abtop displays.

use super::log::{LogReader, ReadOutcome};
use crate::collector::{redact_secrets, sanitize_terminal_text};
use crate::model::{ChatMessage, ChatRole, FileAccess, FileOp, ToolCall};
use crate::model::{MAX_CHAT_MESSAGES, MAX_FILE_ACCESSES};
use serde::Deserialize;
use serde_json::value::RawValue;
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const MAX_TOOL_CALLS: usize = 500;
const MAX_HISTORY: usize = 10_000;

/// A tool call whose `tool/result` has not been recorded yet.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct PendingTool {
    pub(crate) call_id: String,
    /// Display name (normalized, e.g. `bash` → `Bash`).
    pub(crate) name: String,
    /// Short, redacted first argument (path, command prefix, pattern).
    pub(crate) arg: String,
    pub(crate) started_ms: u64,
}

/// Provider retry announced by `llm/retry` and not yet superseded.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct RetryState {
    /// Failure code, e.g. `RATE_LIMIT`, `SERVER`, `TIMEOUT`.
    pub(crate) code: String,
    pub(crate) attempt: u32,
    pub(crate) max_attempts: u32,
    /// `time + delayMs` of the retry event.
    pub(crate) resume_at_ms: u64,
}

/// A delegated child session named by `subagent/catalog` or `team/member`.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ChildRef {
    pub(crate) id: String,
    pub(crate) label: String,
}

/// Everything abtop derives from one DSH session log.
#[derive(Debug, Clone, Default)]
pub(crate) struct DshTranscript {
    // Header (first row, `type: "session"`).
    pub(crate) id: String,
    pub(crate) cwd: Option<String>,
    pub(crate) created_at_ms: u64,
    pub(crate) parent_session: Option<String>,
    pub(crate) origin_subagent: bool,
    pub(crate) is_seeded: bool,
    pub(crate) format_version: u32,

    // Route.
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) reasoning_effort: String,
    /// Latest `request/context.contextWindow`; 0 when never recorded.
    pub(crate) context_window: u64,

    // Usage, excluding a seeded session's inherited prefix.
    pub(crate) total_input: u64,
    pub(crate) total_output: u64,
    pub(crate) total_cache_read: u64,
    pub(crate) total_cache_write: u64,
    /// Prompt size of the latest model call: input + cacheRead + cacheWrite.
    pub(crate) last_context_tokens: u64,
    pub(crate) max_context_tokens: u64,
    /// Number of `assistant/message` rows (completed model calls).
    pub(crate) model_calls: u32,
    pub(crate) token_history: Vec<u64>,
    pub(crate) context_history: Vec<u64>,
    pub(crate) compaction_count: u32,

    // Lifecycle.
    pub(crate) in_turn: bool,
    pub(crate) turn_started_ms: u64,
    /// Start of the model call in flight (`step/start` not yet answered); 0 when none.
    pub(crate) step_started_ms: u64,
    pub(crate) pending_tools: Vec<PendingTool>,
    /// Tool name of an `approval/asked` without its `approval/decided`.
    pub(crate) pending_approval: Option<String>,
    pub(crate) retry: Option<RetryState>,
    pub(crate) compacting: bool,
    /// Short reason when the latest `turn/end` was an error or abort.
    pub(crate) last_turn_error: Option<String>,
    /// Max `time` over all rows.
    pub(crate) last_event_ms: u64,

    // Content (sanitized and redacted).
    /// Latest `session/title` title.
    pub(crate) title: String,
    /// Own `subagent/descriptor.label` for delegated sessions.
    pub(crate) label: String,
    /// First human prompt (`user/message` with `source.kind == "user"`).
    pub(crate) first_prompt: String,
    /// Objective of the first `goal/change`, used when no human prompt exists.
    pub(crate) goal_objective: String,
    pub(crate) first_assistant_text: String,
    pub(crate) chat_messages: Vec<ChatMessage>,
    pub(crate) tool_calls: Vec<ToolCall>,
    pub(crate) file_accesses: Vec<FileAccess>,
    pub(crate) children: Vec<ChildRef>,

    /// `callId` → absolute tool-call index, for duration back-fill.
    tool_call_index: HashMap<String, usize>,
    /// Tool calls dropped from the front of the sliding window.
    tool_calls_dropped: usize,
}

impl DshTranscript {
    /// Apply one physical JSONL row. Malformed rows and unknown event types are ignored.
    pub(crate) fn apply_row(&mut self, row: &[u8]) {
        let Ok(env) = serde_json::from_slice::<Envelope>(row) else {
            return;
        };
        if env.kind == "session" {
            if let Ok(header) = serde_json::from_slice::<Header>(row) {
                self.apply_header(header);
            }
            return;
        }
        let time = env.time.max(0.0) as u64;
        self.last_event_ms = self.last_event_ms.max(time);
        let data = env.data.map(RawValue::get).unwrap_or("null");
        match env.kind.as_ref() {
            "model/selection" => {
                if let Some(d) = parse::<ModelSelection>(data) {
                    self.set_route(&d.provider, &d.model);
                    if let Some(effort) = d.reasoning_effort {
                        self.reasoning_effort = clean(&effort, 32);
                    }
                }
            }
            "request/context" => {
                if let Some(d) = parse::<RequestContext>(data) {
                    self.set_route(&d.provider, &d.model);
                    if d.context_window > 0.0 {
                        self.context_window = d.context_window as u64;
                    }
                }
            }
            "subagent/descriptor" => {
                if let Some(d) = parse::<SubagentDescriptor>(data) {
                    if !d.label.is_empty() {
                        self.label = clean_text(&d.label, 120);
                    }
                    if self.model.is_empty() {
                        self.set_route(&d.agent_provider, &d.agent_model);
                    }
                    if self.reasoning_effort.is_empty() {
                        self.reasoning_effort = clean(&d.agent_reasoning_effort, 32);
                    }
                }
            }
            "subagent/catalog" => {
                if let Some(d) = parse::<SubagentCatalog>(data) {
                    self.add_child(&d.child_id, &d.label);
                }
            }
            "team/member" => {
                if let Some(d) = parse::<TeamMemberEvent>(data) {
                    self.add_child(&d.member.id, &d.member.name);
                }
            }
            "goal/change" if self.goal_objective.is_empty() => {
                if let Some(d) = parse::<GoalChange>(data) {
                    self.goal_objective = clean_text(&d.goal.objective, 120);
                }
            }
            "turn/start" => {
                self.in_turn = true;
                self.turn_started_ms = time;
                self.last_turn_error = None;
            }
            "step/start" => self.step_started_ms = time,
            "step/end" => self.step_started_ms = 0,
            "assistant/message" => {
                if let Some(d) = parse::<AssistantMessage>(data) {
                    self.apply_assistant(d);
                }
            }
            "tool/call" => {
                if let Some(d) = parse::<ToolCallEvent>(data) {
                    self.apply_tool_call(d, time);
                }
            }
            "tool/result" => {
                if let Some(d) = parse::<ToolResultEvent>(data) {
                    let id = if d.message.tool_call_id.is_empty() {
                        d.message.source.call_id
                    } else {
                        d.message.tool_call_id
                    };
                    self.finish_tool(&id, time);
                }
            }
            "approval/asked" => {
                let tool = parse::<ApprovalAsked>(data)
                    .map(|d| normalize_tool_name(&clean(&d.tool_name, 64)))
                    .unwrap_or_default();
                self.pending_approval = Some(tool);
            }
            "approval/decided" => self.pending_approval = None,
            "llm/retry" => {
                if let Some(d) = parse::<LlmRetry>(data) {
                    self.retry = Some(RetryState {
                        code: clean(&d.failure.code, 32),
                        attempt: d.retry,
                        max_attempts: d.max_retries,
                        resume_at_ms: time.saturating_add(d.delay_ms.max(0.0) as u64),
                    });
                }
            }
            "compaction/start" => self.compacting = true,
            "compaction/end" => {
                self.compacting = false;
                self.compaction_count = self.compaction_count.saturating_add(1);
            }
            "turn/end" => {
                let reason = parse::<TurnEnd>(data).map(|d| d.reason).unwrap_or_default();
                self.close_turn(time);
                self.last_turn_error = match reason.kind.as_str() {
                    "error" => Some(clean_text(&summarize_failure(&reason.error.message), 60)),
                    "aborted" => Some("aborted".to_string()),
                    _ => None,
                };
            }
            "session/title" => {
                if let Some(d) = parse::<SessionTitle>(data) {
                    let title = clean_text(&d.title, 120);
                    if !title.is_empty() {
                        self.title = title;
                    }
                }
            }
            "user/message" => {
                if let Some(d) = parse::<UserMessage>(data) {
                    if d.source.kind == "user" {
                        self.apply_user_text(&join_text(&d.content));
                    }
                }
            }
            "session/end-seed" => {
                let inherited = parse::<EndSeed>(data).is_some_and(|d| d.inherited);
                self.close_turn(time);
                if inherited && self.is_seeded {
                    self.reset_inherited();
                }
            }
            _ => {}
        }
    }

    fn apply_header(&mut self, h: Header) {
        self.id = clean(&h.id, 256);
        self.cwd = h
            .cwd
            .map(|c| sanitize_terminal_text(&c))
            .filter(|c| !c.is_empty());
        self.created_at_ms = h.created_at.max(0.0) as u64;
        self.parent_session = h.parent_session.map(|p| clean(&p, 256));
        self.origin_subagent = h.origin.as_deref() == Some("subagent");
        self.is_seeded = h.is_seeded;
        self.format_version = h.version;
    }

    fn set_route(&mut self, provider: &str, model: &str) {
        if !model.is_empty() {
            self.model = clean(model, 128);
        }
        if !provider.is_empty() {
            self.provider = clean(provider, 64);
        }
    }

    fn add_child(&mut self, id: &str, label: &str) {
        if id.is_empty() {
            return;
        }
        let id = clean(id, 256);
        let label = clean_text(label, 120);
        if let Some(existing) = self.children.iter_mut().find(|c| c.id == id) {
            if !label.is_empty() {
                existing.label = label;
            }
        } else {
            self.children.push(ChildRef { id, label });
        }
    }

    fn apply_assistant(&mut self, d: AssistantMessage) {
        let u = d.usage;
        let (inp, out) = (u.input_tokens as u64, u.output_tokens as u64);
        let (cr, cw) = (u.cache_read_tokens as u64, u.cache_write_tokens as u64);
        self.total_input = self.total_input.saturating_add(inp);
        self.total_output = self.total_output.saturating_add(out);
        self.total_cache_read = self.total_cache_read.saturating_add(cr);
        self.total_cache_write = self.total_cache_write.saturating_add(cw);
        let prompt = inp.saturating_add(cr).saturating_add(cw);
        if prompt > 0 {
            self.last_context_tokens = prompt;
            self.max_context_tokens = self.max_context_tokens.max(prompt);
        }
        if self.context_history.len() < MAX_HISTORY {
            self.context_history.push(prompt);
        }
        if self.token_history.len() < MAX_HISTORY {
            self.token_history.push(prompt.saturating_add(out));
        }
        self.model_calls = self.model_calls.saturating_add(1);
        self.step_started_ms = 0;
        self.retry = None;
        if !d.message.source.model.is_empty() {
            self.model = clean(&d.message.source.model, 128);
        }

        let text = join_text(&d.message.content);
        if !text.is_empty() {
            if self.first_assistant_text.is_empty() {
                self.first_assistant_text = clean_text(&text, 200);
            }
            push_chat(&mut self.chat_messages, ChatRole::Assistant, &text);
        }
    }

    fn apply_tool_call(&mut self, d: ToolCallEvent, time: u64) {
        let name = normalize_tool_name(&clean(&d.name, 64));
        let args = parse_tool_args(d.arguments);
        let arg = tool_arg(&args);
        if let Some(path) = args.file_path.as_deref() {
            let op = match name.as_str() {
                "Read" => Some(FileOp::Read),
                "Edit" => Some(FileOp::Edit),
                "Write" => Some(FileOp::Write),
                _ => None,
            };
            if let Some(operation) = op {
                self.file_accesses.push(FileAccess {
                    path: sanitize_terminal_text(path),
                    operation,
                    turn_index: self.model_calls,
                });
                if self.file_accesses.len() > MAX_FILE_ACCESSES {
                    let excess = self.file_accesses.len() - MAX_FILE_ACCESSES;
                    self.file_accesses.drain(..excess);
                }
            }
        }
        self.tool_call_index.insert(
            d.call_id.clone(),
            self.tool_calls_dropped + self.tool_calls.len(),
        );
        self.tool_calls.push(ToolCall {
            name: name.clone(),
            arg: truncate(&arg, 40),
            duration_ms: 0,
        });
        if self.tool_calls.len() > MAX_TOOL_CALLS {
            let excess = self.tool_calls.len() - MAX_TOOL_CALLS;
            self.tool_calls.drain(..excess);
            self.tool_calls_dropped += excess;
            let floor = self.tool_calls_dropped;
            self.tool_call_index.retain(|_, idx| *idx >= floor);
        }
        self.pending_tools.retain(|p| p.call_id != d.call_id);
        self.pending_tools.push(PendingTool {
            call_id: d.call_id,
            name,
            arg,
            started_ms: time,
        });
    }

    fn finish_tool(&mut self, call_id: &str, time: u64) {
        if let Some(pos) = self.pending_tools.iter().position(|p| p.call_id == call_id) {
            let pending = self.pending_tools.remove(pos);
            self.set_duration(call_id, time.saturating_sub(pending.started_ms).max(1));
        }
    }

    fn set_duration(&mut self, call_id: &str, duration_ms: u64) {
        if let Some(&idx) = self.tool_call_index.get(call_id) {
            let slot = idx.saturating_sub(self.tool_calls_dropped);
            if let Some(tc) = self.tool_calls.get_mut(slot) {
                if tc.duration_ms == 0 {
                    tc.duration_ms = duration_ms;
                }
            }
        }
    }

    /// Leave the turn: tools still pending are closed with their elapsed time.
    fn close_turn(&mut self, time: u64) {
        for pending in std::mem::take(&mut self.pending_tools) {
            let elapsed = time.saturating_sub(pending.started_ms).max(1);
            self.set_duration(&pending.call_id, elapsed);
        }
        self.in_turn = false;
        self.step_started_ms = 0;
        self.pending_approval = None;
        self.retry = None;
        self.compacting = false;
    }

    /// Forget what a fork copied from its parent: usage and the parent's
    /// delegations. Context size stays, since the inherited prefix is still
    /// part of the fork's prompt.
    fn reset_inherited(&mut self) {
        self.total_input = 0;
        self.total_output = 0;
        self.total_cache_read = 0;
        self.total_cache_write = 0;
        self.model_calls = 0;
        self.token_history.clear();
        self.context_history.clear();
        self.compaction_count = 0;
        self.children.clear();
    }

    fn apply_user_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.first_prompt.is_empty() {
            self.first_prompt = clean_text(text, 120);
        }
        push_chat(&mut self.chat_messages, ChatRole::User, text);
    }
}

/// Incremental reader plus reducer for one session log file.
pub(crate) struct DshSessionLog {
    reader: LogReader,
    pub(crate) transcript: DshTranscript,
}

impl DshSessionLog {
    pub(crate) fn open(path: PathBuf) -> Self {
        Self {
            reader: LogReader::new(path),
            transcript: DshTranscript::default(),
        }
    }

    pub(crate) fn log_path(&self) -> &Path {
        self.reader.path()
    }

    /// Bytes on disk not yet consumed (0 when the file is unreadable).
    pub(crate) fn pending_bytes(&self) -> u64 {
        self.reader.pending_bytes()
    }

    /// Consume rows appended since the last call; rebuild from scratch when the log was reset.
    pub(crate) fn refresh(&mut self) {
        for _ in 0..2 {
            let transcript = &mut self.transcript;
            match self.reader.read_new(&mut |row| transcript.apply_row(row)) {
                Ok(ReadOutcome::Reset) => self.transcript = DshTranscript::default(),
                Ok(ReadOutcome::Appended) | Err(_) => return,
            }
        }
    }
}

// --- Row shapes -----------------------------------------------------------

#[derive(Deserialize)]
struct Envelope<'a> {
    #[serde(rename = "type", borrow)]
    kind: Cow<'a, str>,
    #[serde(default)]
    time: f64,
    #[serde(borrow, default)]
    data: Option<&'a RawValue>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Header {
    version: u32,
    id: String,
    created_at: f64,
    cwd: Option<String>,
    parent_session: Option<String>,
    is_seeded: bool,
    origin: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct ModelSelection {
    provider: String,
    model: String,
    reasoning_effort: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct RequestContext {
    provider: String,
    model: String,
    context_window: f64,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct SubagentDescriptor {
    label: String,
    agent_provider: String,
    agent_model: String,
    agent_reasoning_effort: String,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct SubagentCatalog {
    child_id: String,
    label: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct TeamMemberEvent {
    member: TeamMember,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct TeamMember {
    id: String,
    name: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct GoalChange {
    goal: Goal,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Goal {
    objective: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct AssistantMessage<'a> {
    usage: Usage,
    #[serde(borrow)]
    message: MessageBody<'a>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Usage {
    input_tokens: f64,
    output_tokens: f64,
    cache_read_tokens: f64,
    cache_write_tokens: f64,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct MessageBody<'a> {
    #[serde(borrow)]
    content: Vec<Block<'a>>,
    source: MessageSource,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct MessageSource {
    model: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Block<'a> {
    #[serde(rename = "type", borrow)]
    kind: Cow<'a, str>,
    /// Kept raw; only `text` blocks are ever decoded, so reasoning and tool
    /// payloads are never copied out of the row.
    #[serde(borrow)]
    text: Option<&'a RawValue>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct ToolCallEvent {
    call_id: String,
    name: String,
    arguments: Option<Box<RawValue>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ToolResultEvent {
    message: ToolResultMessage,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct ToolResultMessage {
    tool_call_id: String,
    source: ToolResultSource,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct ToolResultSource {
    call_id: String,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct ApprovalAsked {
    tool_name: String,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct LlmRetry {
    retry: u32,
    max_retries: u32,
    delay_ms: f64,
    failure: Failure,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Failure {
    code: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct TurnEnd {
    reason: TurnEndReason,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct TurnEndReason {
    kind: String,
    error: TurnError,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct TurnError {
    message: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SessionTitle {
    title: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct UserMessage<'a> {
    #[serde(borrow)]
    content: Vec<Block<'a>>,
    source: UserSource,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct UserSource {
    kind: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct EndSeed {
    inherited: bool,
}

/// Tool arguments that feed the display arg and the file audit.
#[derive(Deserialize, Default)]
#[serde(default)]
struct ToolArgs {
    file_path: Option<String>,
    command: Option<String>,
    pattern: Option<String>,
    path: Option<String>,
    url: Option<String>,
    query: Option<String>,
    name: Option<String>,
}

// --- Helpers --------------------------------------------------------------

fn parse<'a, T: Deserialize<'a>>(data: &'a str) -> Option<T> {
    serde_json::from_str(data).ok()
}

/// `arguments` is normally a JSON-encoded string; accept a bare object too.
fn parse_tool_args(raw: Option<Box<RawValue>>) -> ToolArgs {
    let Some(raw) = raw else {
        return ToolArgs::default();
    };
    let text = raw.get();
    if text.starts_with('"') {
        serde_json::from_str::<String>(text)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    } else {
        serde_json::from_str(text).unwrap_or_default()
    }
}

fn tool_arg(args: &ToolArgs) -> String {
    if let Some(path) = &args.file_path {
        return sanitize_terminal_text(&shorten_path(path));
    }
    if let Some(cmd) = &args.command {
        let first = cmd.lines().next().unwrap_or_default();
        return truncate(&redact_secrets(&sanitize_terminal_text(first)), 40);
    }
    [
        &args.pattern,
        &args.path,
        &args.url,
        &args.query,
        &args.name,
    ]
    .into_iter()
    .flatten()
    .next()
    .map(|s| truncate(&redact_secrets(&sanitize_terminal_text(s)), 40))
    .unwrap_or_default()
}

pub(crate) fn normalize_tool_name(name: &str) -> String {
    match name {
        "bash" => "Bash",
        "read" => "Read",
        "edit" => "Edit",
        "write" => "Write",
        "grep" => "Grep",
        "glob" => "Glob",
        "skill" => "Skill",
        "subagent" | "subagent_fork" | "spawn_teammate" => "Agent",
        other => other,
    }
    .to_string()
}

/// Unwrap provider JSON envelopes: `429 {"error":{"message":"x"}}` → `429 x`.
fn summarize_failure(message: &str) -> String {
    let Some(start) = message.find('{') else {
        return message.to_string();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&message[start..]) else {
        return message.to_string();
    };
    let inner = value
        .pointer("/error/message")
        .or_else(|| value.get("message"))
        .and_then(|m| m.as_str());
    match inner {
        Some(inner) => format!("{} {}", message[..start].trim(), inner)
            .trim()
            .to_string(),
        None => message.to_string(),
    }
}

fn join_text(blocks: &[Block]) -> String {
    blocks
        .iter()
        .filter(|b| b.kind == "text")
        .filter_map(|b| serde_json::from_str::<String>(b.text?.get()).ok())
        .collect::<Vec<_>>()
        .join(" ")
}

fn push_chat(messages: &mut Vec<ChatMessage>, role: ChatRole, raw: &str) {
    let text = clean_text(raw, 500);
    if text.is_empty() {
        return;
    }
    messages.push(ChatMessage { role, text });
    if messages.len() > MAX_CHAT_MESSAGES {
        let excess = messages.len() - MAX_CHAT_MESSAGES;
        messages.drain(..excess);
    }
}

/// Collapse whitespace, drop code fences, strip control chars, redact secrets.
fn clean_text(raw: &str, max: usize) -> String {
    let joined = raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("```"))
        .collect::<Vec<_>>()
        .join(" ");
    let safe = sanitize_terminal_text(&joined);
    truncate(redact_secrets(&safe).trim(), max)
}

/// Identifier-like field: control chars stripped, bounded length.
fn clean(raw: &str, max: usize) -> String {
    truncate(sanitize_terminal_text(raw).trim(), max)
}

fn shorten_path(path: &str) -> String {
    let parts: Vec<&str> = path.rsplit(['/', '\\']).collect();
    if parts.len() <= 2 {
        path.to_string()
    } else {
        format!("{}/{}", parts[1], parts[0])
    }
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(t: &mut DshTranscript, rows: &[&str]) {
        for row in rows {
            t.apply_row(row.as_bytes());
        }
    }

    fn header(extra: &str) -> String {
        format!(
            r#"{{"type":"session","version":4,"id":"s-1","createdAt":1000,"cwd":"/w/proj","isSeeded":false,"delegationDepth":0{extra}}}"#
        )
    }

    fn assistant(time: u64, usage: &str, content: &str) -> String {
        format!(
            r#"{{"type":"assistant/message","seq":1,"time":{time},"data":{{"turn":1,"step":1,"usage":{usage},"stream":[{{"type":"chunk","chunk":{{"x":[1,2,3]}}}}],"message":{{"role":"assistant","content":[{content}],"source":{{"kind":"model","provider":"anthropic","model":"claude-opus-5-5"}}}}}}}}"#
        )
    }

    #[test]
    fn header_fields() {
        let mut t = DshTranscript::default();
        apply(
            &mut t,
            &[&header(r#","parentSession":"p-9","origin":"subagent""#)],
        );
        assert_eq!(t.id, "s-1");
        assert_eq!(t.cwd.as_deref(), Some("/w/proj"));
        assert_eq!(t.created_at_ms, 1000);
        assert_eq!(t.parent_session.as_deref(), Some("p-9"));
        assert!(t.origin_subagent);
        assert_eq!(t.format_version, 4);
    }

    #[test]
    fn route_and_context_window() {
        let mut t = DshTranscript::default();
        apply(
            &mut t,
            &[
                r#"{"type":"model/selection","seq":1,"time":5,"data":{"provider":"anthropic","model":"claude-opus-5-5","reasoningEffort":"xhigh"}}"#,
                r#"{"type":"request/context","seq":2,"time":6,"data":{"provider":"deepseek","model":"deepseek-v4","contextWindow":1000000}}"#,
            ],
        );
        assert_eq!(t.provider, "deepseek");
        assert_eq!(t.model, "deepseek-v4");
        assert_eq!(t.reasoning_effort, "xhigh");
        assert_eq!(t.context_window, 1_000_000);
        assert_eq!(t.last_event_ms, 6);
    }

    #[test]
    fn assistant_usage_is_disjoint_and_text_only_chat() {
        let mut t = DshTranscript::default();
        let row = assistant(
            10,
            r#"{"inputTokens":4,"outputTokens":649,"totalTokens":79660,"cacheReadTokens":100,"cacheWriteTokens":79007}"#,
            r#"{"type":"reasoning","text":"secret thoughts"},{"type":"text","text":"Hello there"},{"type":"tool-call","id":"t1","name":"bash","arguments":"{}"}"#,
        );
        apply(&mut t, &[&row]);
        assert_eq!(
            (
                t.total_input,
                t.total_output,
                t.total_cache_read,
                t.total_cache_write
            ),
            (4, 649, 100, 79007)
        );
        assert_eq!(t.last_context_tokens, 4 + 100 + 79007);
        assert_eq!(t.model_calls, 1);
        assert_eq!(t.token_history, vec![4 + 100 + 79007 + 649]);
        assert_eq!(t.first_assistant_text, "Hello there");
        assert_eq!(t.chat_messages.len(), 1);
        assert!(!t.chat_messages[0].text.contains("secret"));
        assert_eq!(t.model, "claude-opus-5-5");
    }

    #[test]
    fn tool_lifecycle_durations_and_file_audit() {
        let mut t = DshTranscript::default();
        apply(
            &mut t,
            &[
                r#"{"type":"turn/start","seq":1,"time":100,"data":{"turn":1}}"#,
                r#"{"type":"tool/call","seq":2,"time":200,"data":{"callId":"c1","name":"read","arguments":"{\"file_path\":\"/a/b/src/main.rs\"}"}}"#,
                r#"{"type":"tool/call","seq":3,"time":210,"data":{"callId":"c2","name":"bash","arguments":"{\"command\":\"export T=sk-ant-abc123\\ncargo test\"}"}}"#,
            ],
        );
        assert!(t.in_turn);
        assert_eq!(t.pending_tools.len(), 2);
        assert_eq!(t.pending_tools[0].name, "Read");
        assert_eq!(t.pending_tools[0].arg, "src/main.rs");
        assert_eq!(t.pending_tools[1].arg, "export T=[REDACTED]");
        assert_eq!(t.file_accesses.len(), 1);
        assert_eq!(t.file_accesses[0].operation, FileOp::Read);

        apply(
            &mut t,
            &[
                r#"{"type":"tool/result","seq":4,"time":450,"data":{"turn":1,"message":{"role":"tool","toolCallId":"c1","content":[{"type":"text","text":"FILE BODY"}]}}}"#,
            ],
        );
        assert_eq!(t.pending_tools.len(), 1);
        assert_eq!(t.tool_calls[0].duration_ms, 250);
        assert_eq!(t.tool_calls[1].duration_ms, 0);

        apply(
            &mut t,
            &[
                r#"{"type":"turn/end","seq":5,"time":1210,"data":{"turn":1,"reason":{"kind":"completed"}}}"#,
            ],
        );
        assert!(!t.in_turn);
        assert!(t.pending_tools.is_empty());
        assert_eq!(t.tool_calls[1].duration_ms, 1000);
        assert_eq!(t.last_turn_error, None);
    }

    #[test]
    fn tool_calls_keep_the_latest_window() {
        let mut t = DshTranscript::default();
        for i in 0..(MAX_TOOL_CALLS + 5) {
            let row = format!(
                r#"{{"type":"tool/call","seq":{i},"time":{i},"data":{{"callId":"c{i}","name":"bash","arguments":"{{\"command\":\"echo {i}\"}}"}}}}"#
            );
            t.apply_row(row.as_bytes());
        }
        assert_eq!(t.tool_calls.len(), MAX_TOOL_CALLS);
        assert_eq!(t.tool_calls[0].arg, "echo 5");
        let last = MAX_TOOL_CALLS + 4;
        let result = format!(
            r#"{{"type":"tool/result","seq":9999,"time":{},"data":{{"message":{{"toolCallId":"c{last}"}}}}}}"#,
            last + 10
        );
        t.apply_row(result.as_bytes());
        assert_eq!(t.tool_calls.last().unwrap().duration_ms, 10);
        assert!(t.tool_call_index.len() <= MAX_TOOL_CALLS);
    }

    #[test]
    fn tool_result_falls_back_to_source_call_id() {
        let mut t = DshTranscript::default();
        apply(
            &mut t,
            &[
                r#"{"type":"tool/call","seq":1,"time":10,"data":{"callId":"c1","name":"grep","arguments":"{\"pattern\":\"fn main\"}"}}"#,
                r#"{"type":"tool/result","seq":2,"time":10,"data":{"message":{"source":{"callId":"c1"}}}}"#,
            ],
        );
        assert!(t.pending_tools.is_empty());
        assert_eq!(t.tool_calls[0].name, "Grep");
        assert_eq!(t.tool_calls[0].arg, "fn main");
        assert_eq!(t.tool_calls[0].duration_ms, 1);
    }

    #[test]
    fn step_approval_retry_and_compaction() {
        let mut t = DshTranscript::default();
        apply(
            &mut t,
            &[
                r#"{"type":"turn/start","seq":1,"time":100,"data":{"turn":1}}"#,
                r#"{"type":"step/start","seq":2,"time":110,"data":{"turn":1,"step":1}}"#,
                r#"{"type":"llm/retry","seq":3,"time":120,"data":{"retry":2,"maxRetries":5,"delayMs":991.3,"failure":{"code":"RATE_LIMIT","message":"429"}}}"#,
            ],
        );
        assert_eq!(t.step_started_ms, 110);
        let retry = t.retry.clone().unwrap();
        assert_eq!(
            (
                retry.code.as_str(),
                retry.attempt,
                retry.max_attempts,
                retry.resume_at_ms
            ),
            ("RATE_LIMIT", 2, 5, 1111)
        );
        apply(
            &mut t,
            &[
                &assistant(130, r#"{"inputTokens":1,"outputTokens":1}"#, ""),
                r#"{"type":"approval/asked","seq":5,"time":140,"data":{"id":"a","toolName":"write","callId":"c"}}"#,
                r#"{"type":"compaction/start","seq":6,"time":150,"data":{}}"#,
            ],
        );
        assert_eq!(t.step_started_ms, 0);
        assert!(t.retry.is_none());
        assert_eq!(t.pending_approval.as_deref(), Some("Write"));
        assert!(t.compacting);
        apply(
            &mut t,
            &[
                r#"{"type":"approval/decided","seq":7,"time":160,"data":{"id":"a","outcome":"allowed-once"}}"#,
                r#"{"type":"compaction/end","seq":8,"time":170,"data":{}}"#,
            ],
        );
        assert!(t.pending_approval.is_none());
        assert!(!t.compacting);
        assert_eq!(t.compaction_count, 1);
    }

    #[test]
    fn turn_end_error_is_kept_until_next_turn() {
        let mut t = DshTranscript::default();
        apply(
            &mut t,
            &[
                r#"{"type":"turn/start","seq":1,"time":1,"data":{"turn":1}}"#,
                r#"{"type":"turn/end","seq":2,"time":2,"data":{"turn":1,"reason":{"kind":"error","error":{"message":"pi-ai stream idle timeout","code":"TIMEOUT"}}}}"#,
            ],
        );
        assert_eq!(
            t.last_turn_error.as_deref(),
            Some("pi-ai stream idle timeout")
        );
        apply(
            &mut t,
            &[r#"{"type":"turn/start","seq":3,"time":3,"data":{"turn":2}}"#],
        );
        assert_eq!(t.last_turn_error, None);
    }

    #[test]
    fn provider_error_envelopes_are_unwrapped() {
        assert_eq!(
            summarize_failure(
                r#"429 {"error":{"source":"lb","type":"rate_limited","message":"Every account is rate-limited"}}"#
            ),
            "429 Every account is rate-limited"
        );
        assert_eq!(
            summarize_failure(
                r#"{"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}"#
            ),
            "Overloaded"
        );
        assert_eq!(summarize_failure("Connection error."), "Connection error.");
        assert_eq!(summarize_failure("bad {json"), "bad {json");
    }

    #[test]
    fn titles_prompts_goal_children_and_label() {
        let mut t = DshTranscript::default();
        apply(
            &mut t,
            &[
                r#"{"type":"user/message","seq":1,"time":1,"data":{"content":[{"type":"text","text":"<system-reminder>x</system-reminder>"}],"source":{"kind":"agent-instructions"}}}"#,
                r#"{"type":"goal/change","seq":2,"time":2,"data":{"goal":{"objective":"Add DSH support"}}}"#,
                r#"{"type":"user/message","seq":3,"time":3,"data":{"content":[{"type":"text","text":"Fix the\nlogin bug"}],"source":{"kind":"user"}}}"#,
                r#"{"type":"session/title","seq":4,"time":4,"data":{"title":"Fix login"}}"#,
                r#"{"type":"session/title","seq":5,"time":5,"data":{"title":""}}"#,
                r#"{"type":"subagent/catalog","seq":6,"time":6,"data":{"childId":"k-1","label":"Write README"}}"#,
                r#"{"type":"team/member","seq":7,"time":7,"data":{"member":{"id":"k-2","name":"dsh-docs","phase":"provisioning"}}}"#,
                r#"{"type":"team/member","seq":8,"time":8,"data":{"member":{"id":"k-2","name":"dsh-docs","phase":"active"}}}"#,
                r#"{"type":"subagent/descriptor","seq":9,"time":9,"data":{"label":"P3 feed layer","agentProvider":"anthropic","agentModel":"claude-opus-5-5","agentReasoningEffort":"max"}}"#,
            ],
        );
        assert_eq!(t.first_prompt, "Fix the login bug");
        assert_eq!(t.goal_objective, "Add DSH support");
        assert_eq!(t.title, "Fix login");
        assert_eq!(t.chat_messages.len(), 1);
        assert_eq!(
            t.children,
            vec![
                ChildRef {
                    id: "k-1".into(),
                    label: "Write README".into()
                },
                ChildRef {
                    id: "k-2".into(),
                    label: "dsh-docs".into()
                },
            ]
        );
        assert_eq!(t.label, "P3 feed layer");
        assert_eq!(t.model, "claude-opus-5-5");
        assert_eq!(t.reasoning_effort, "max");
    }

    #[test]
    fn seeded_prefix_is_not_counted() {
        let mut t = DshTranscript::default();
        apply(
            &mut t,
            &[
                &header(r#","isSeeded":true"#).replace(r#""isSeeded":false,"#, ""),
                &assistant(1, r#"{"inputTokens":1000,"outputTokens":500}"#, ""),
                r#"{"type":"subagent/catalog","seq":2,"time":2,"data":{"childId":"parent-kid","label":"x"}}"#,
                r#"{"type":"session/end-seed","seq":3,"time":3,"data":{"inherited":true}}"#,
            ],
        );
        assert_eq!((t.total_input, t.model_calls), (0, 0));
        assert_eq!(t.last_context_tokens, 1000, "inherited prompt still counts");
        assert!(
            t.children.is_empty(),
            "parent's delegations are not the fork's"
        );
        apply(
            &mut t,
            &[&assistant(4, r#"{"inputTokens":7,"outputTokens":3}"#, "")],
        );
        assert!(t.is_seeded);
        assert_eq!((t.total_input, t.total_output, t.model_calls), (7, 3, 1));
    }

    #[test]
    fn resume_marker_closes_lifecycle_but_keeps_usage() {
        let mut t = DshTranscript::default();
        apply(
            &mut t,
            &[
                &header(""),
                r#"{"type":"turn/start","seq":1,"time":1,"data":{"turn":1}}"#,
                &assistant(2, r#"{"inputTokens":5,"outputTokens":5}"#, ""),
                r#"{"type":"tool/call","seq":3,"time":3,"data":{"callId":"c","name":"bash","arguments":"{}"}}"#,
                r#"{"type":"session/end-seed","seq":4,"time":9,"data":{}}"#,
            ],
        );
        assert!(!t.in_turn);
        assert!(t.pending_tools.is_empty());
        assert_eq!(t.total_input, 5);
    }

    #[test]
    fn huge_usage_saturates_instead_of_overflowing() {
        let mut t = DshTranscript::default();
        let huge = r#"{"inputTokens":1e30,"outputTokens":1e30,"cacheReadTokens":1e30,"cacheWriteTokens":1e30}"#;
        apply(&mut t, &[&assistant(1, huge, ""), &assistant(2, huge, "")]);
        assert_eq!(t.total_input, u64::MAX);
        assert_eq!(t.last_context_tokens, u64::MAX);
        assert_eq!(t.model_calls, 2);
    }

    #[test]
    fn secrets_are_redacted_before_truncation() {
        let mut t = DshTranscript::default();
        let row = format!(
            r#"{{"type":"tool/call","seq":1,"time":1,"data":{{"callId":"c","name":"bash","arguments":"{{\"command\":\"{}sk-ant-api03-SECRETSECRET\"}}"}}}}"#,
            "x".repeat(35)
        );
        t.apply_row(row.as_bytes());
        let arg = &t.tool_calls[0].arg;
        assert!(!arg.contains("sk-an"), "{arg}");
        assert!(arg.starts_with("xxxxx"));
    }

    #[test]
    fn malformed_and_unknown_rows_are_ignored() {
        let mut t = DshTranscript::default();
        apply(
            &mut t,
            &[
                "not json",
                r#"{"type":"assistant/message","seq":1,"time":1,"data":"weird"}"#,
                r#"{"type":"brand/new-event","seq":2,"time":2,"data":{"x":1}}"#,
                r#"{"seq":3}"#,
            ],
        );
        assert_eq!(t.model_calls, 0);
        assert_eq!(t.last_event_ms, 2);
    }

    #[test]
    fn terminal_controls_are_stripped() {
        let mut t = DshTranscript::default();
        apply(
            &mut t,
            &[
                r#"{"type":"session/title","seq":1,"time":1,"data":{"title":"evil\u001b[2Jtitle\u202e"}}"#,
            ],
        );
        assert_eq!(t.title, "evil[2Jtitle");
    }

    #[test]
    fn session_log_rebuilds_after_reset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.v4.jsonl.zstd");
        let body = format!(
            "{}\n{}\n",
            header(""),
            assistant(1, r#"{"inputTokens":10,"outputTokens":1}"#, "")
        );
        std::fs::write(&path, super::super::log::zstd_frame(&body)).unwrap();
        let mut log = DshSessionLog::open(path.clone());
        log.refresh();
        assert_eq!(log.transcript.total_input, 10);
        log.refresh();
        assert_eq!(log.transcript.total_input, 10);

        std::fs::write(
            &path,
            super::super::log::zstd_frame(&format!("{}\n", header(""))),
        )
        .unwrap();
        log.refresh();
        assert_eq!(log.transcript.id, "s-1");
        assert_eq!(log.transcript.total_input, 0);
    }

    /// `ABTOP_DSH_BENCH_LOG=<path> cargo test --release dsh_bench -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn dsh_bench_first_parse() {
        let Ok(path) = std::env::var("ABTOP_DSH_BENCH_LOG") else {
            return;
        };
        let start = std::time::Instant::now();
        let mut log = DshSessionLog::open(PathBuf::from(path));
        log.refresh();
        let t = &log.transcript;
        eprintln!(
            "first parse {:?}: calls={} in={} out={} cr={} cw={} tools={} model={}",
            start.elapsed(),
            t.model_calls,
            t.total_input,
            t.total_output,
            t.total_cache_read,
            t.total_cache_write,
            t.tool_calls.len(),
            t.model
        );
    }
}
