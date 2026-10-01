//! DeepSeek Harness (DSH) sessions: live leases mapped from `dsh` host
//! processes to append-only session logs under `$DSH_HOME/sessions`.

mod log;
mod paths;
mod transcript;

use super::open_paths::{map_pid_to_open_paths, ProcessOpenPaths};
use super::process::{self, ProcInfo};
use super::SharedProcessData;
use crate::model::{AgentSession, ChildProcess, LaunchSurface, SessionStatus, SubAgent};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use transcript::{truncate, DshSessionLog, DshTranscript};

/// Cached logs of dirs neither live nor referenced for this many ticks are dropped.
const EVICT_AFTER_TICKS: u64 = 30;
/// Most recent delegated children resolved per parent.
const MAX_CHILDREN: usize = 20;
/// Finished child logs parsed for the first time per tick, so a host with
/// many delegations does not stall the first frame.
const NEW_CHILD_PARSES_PER_TICK: usize = 16;
/// A retry counts as current until this long after its scheduled resume.
const RETRY_GRACE_MS: u64 = 30_000;
/// Unread bytes above which a log's parse is worth a worker thread.
const PARALLEL_PARSE_BYTES: u64 = 256 * 1024;
const MAX_PARSE_WORKERS: usize = 8;
/// Platforms whose per-PID fd listing is cheap enough to run every tick.
const FAST_FD_LISTING: bool = cfg!(any(
    target_os = "linux",
    target_vendor = "apple",
    target_os = "windows"
));

pub struct DshCollector {
    sessions_root: PathBuf,
    logs: HashMap<PathBuf, CachedLog>,
    tick: u64,
    open_paths: HashMap<u32, ProcessOpenPaths>,
    open_paths_pids: Vec<u32>,
    caches: RowCaches,
}

/// Per-cwd git branch and per-host package version, both cheap file reads
/// that would otherwise repeat for every row of a multi-session host.
#[derive(Default)]
struct RowCaches {
    git_branches: HashMap<String, String>,
    /// Host PID → (command, package version); a changed command means PID reuse.
    versions: HashMap<u32, (String, String)>,
}

impl RowCaches {
    fn start_tick(&mut self, slow_tick: bool, hosts: &[Host]) {
        if slow_tick {
            self.git_branches.clear();
        }
        self.versions
            .retain(|pid, _| hosts.iter().any(|h| h.pid == *pid));
    }

    fn git_branch(&mut self, cwd: &str) -> String {
        if cwd.is_empty() {
            return String::new();
        }
        self.git_branches
            .entry(cwd.to_string())
            .or_insert_with(|| paths::git_branch(cwd))
            .clone()
    }

    fn version(&mut self, pid: u32, cmd: &str) -> String {
        match self.versions.get(&pid) {
            Some((cached_cmd, version)) if cached_cmd == cmd => version.clone(),
            _ => {
                let version = paths::host_version(cmd);
                self.versions
                    .insert(pid, (cmd.to_string(), version.clone()));
                version
            }
        }
    }
}

struct CachedLog {
    log: Option<DshSessionLog>,
    last_seen: u64,
}

#[derive(Debug, Clone, PartialEq)]
struct Host {
    pid: u32,
    surface: LaunchSurface,
}

impl DshCollector {
    pub fn new() -> Self {
        Self::with_sessions_root(paths::dsh_home().join("sessions"))
    }

    fn with_sessions_root(sessions_root: PathBuf) -> Self {
        Self {
            sessions_root,
            logs: HashMap::new(),
            tick: 0,
            open_paths: HashMap::new(),
            open_paths_pids: Vec::new(),
            caches: RowCaches::default(),
        }
    }

    fn refresh_open_paths(&mut self, pids: &[u32], slow_tick: bool) {
        if FAST_FD_LISTING || slow_tick || self.open_paths_pids != pids {
            self.open_paths = map_pid_to_open_paths(pids);
            self.open_paths_pids = pids.to_vec();
        }
    }

    fn collect_with(&mut self, shared: &SharedProcessData, now_ms: u64) -> Vec<AgentSession> {
        let hosts = find_hosts(&shared.process_info);
        if !hosts.is_empty() {
            let pids: Vec<u32> = hosts.iter().map(|h| h.pid).collect();
            self.refresh_open_paths(&pids, shared.slow_tick);
        }
        self.collect_from_open(shared, &hosts, now_ms)
    }

    /// Build rows from `hosts` and the current `open_paths` view.
    fn collect_from_open(
        &mut self,
        shared: &SharedProcessData,
        hosts: &[Host],
        now_ms: u64,
    ) -> Vec<AgentSession> {
        self.tick += 1;
        self.caches.start_tick(shared.slow_tick, hosts);
        if hosts.is_empty() {
            self.evict();
            return Vec::new();
        }

        let slow = shared.slow_tick;
        let mut live = self.discover_live(shared, hosts);
        live.retain(|(_, dir)| self.prepare(dir, slow));
        let live_dirs: Vec<PathBuf> = live.iter().map(|(_, d)| d.clone()).collect();
        self.refresh_dirs(&live_dirs);
        let live_ids: HashSet<String> = live
            .iter()
            .filter_map(|(_, dir)| self.transcript(dir).map(|t| t.id.clone()))
            .collect();
        self.resolve_finished_children(&live, &live_ids, slow);
        self.evict();

        let by_id: HashMap<&str, (&DshTranscript, bool)> = self
            .logs
            .iter()
            .filter_map(|(dir, cached)| {
                let t = &cached.log.as_ref()?.transcript;
                let is_live = live.iter().any(|(_, d)| d == dir);
                (!t.id.is_empty()).then_some((t.id.as_str(), (t, is_live)))
            })
            .collect();
        let rows: Vec<(u32, &Path, &DshTranscript)> = live
            .iter()
            .filter_map(|(pid, dir)| {
                let t = self.logs.get(dir)?.log.as_ref().map(|l| &l.transcript)?;
                (!t.id.is_empty() && !is_folded(t, &live_ids)).then_some((*pid, dir.as_path(), t))
            })
            .collect();
        let primary = primary_rows(
            &rows
                .iter()
                .map(|(pid, _, t)| (*pid, t.last_event_ms, t.created_at_ms))
                .collect::<Vec<_>>(),
        );

        let mut sessions: Vec<AgentSession> = rows
            .iter()
            .enumerate()
            .map(|(i, &(pid, dir, t))| {
                let cmd = shared
                    .process_info
                    .get(&pid)
                    .map(|p| p.command.as_str())
                    .unwrap_or_default();
                let mut row = build_row(RowInput {
                    pid,
                    session_dir: dir,
                    t,
                    surface: hosts
                        .iter()
                        .find(|h| h.pid == pid)
                        .map_or(LaunchSurface::Cli, |h| h.surface),
                    cmd,
                    subagents: subagents_for(t, &by_id),
                    now_ms,
                });
                row.git_branch = self.caches.git_branch(&row.cwd);
                row.version = self.caches.version(pid, cmd);
                if primary.contains(&i) {
                    attach_host_resources(&mut row, shared);
                }
                row
            })
            .collect();
        sessions.sort_by_key(|s| std::cmp::Reverse(s.started_at));
        sessions
    }

    /// Live `(host pid, session dir)` pairs from leases, plus the Windows
    /// cwd fallback for hosts whose fds are not visible.
    fn discover_live(&self, shared: &SharedProcessData, hosts: &[Host]) -> Vec<(u32, PathBuf)> {
        let mut live = live_sessions(hosts, &self.open_paths, &shared.process_info);
        if cfg!(target_os = "windows") {
            for host in hosts {
                if live.iter().any(|(pid, _)| *pid == host.pid) {
                    continue;
                }
                let cwd = self.open_paths.get(&host.pid).and_then(|o| o.cwd.clone());
                if let Some(dir) = cwd.and_then(|c| {
                    paths::newest_session_dir_for_cwd(&self.sessions_root, &c.to_string_lossy())
                }) {
                    live.push((host.pid, dir));
                }
            }
        }
        live
    }

    /// Load delegated children that are no longer live, so finished subagents
    /// keep their name and tokens. New parses are budgeted per tick.
    fn resolve_finished_children(
        &mut self,
        live: &[(u32, PathBuf)],
        live_ids: &HashSet<String>,
        slow: bool,
    ) {
        let mut child_dirs = Vec::new();
        for (_, dir) in live {
            let (Some(t), Some(project)) = (self.transcript(dir), dir.parent()) else {
                continue;
            };
            for child in t.children.iter().rev().take(MAX_CHILDREN) {
                if !live_ids.contains(&child.id) {
                    child_dirs.push(project.join(paths::encode_segment(&child.id)));
                }
            }
        }
        let mut budget = NEW_CHILD_PARSES_PER_TICK;
        child_dirs.retain(|dir| {
            if !self.logs.contains_key(dir) {
                if budget == 0 || !dir.is_dir() {
                    return false;
                }
                budget -= 1;
            }
            self.prepare(dir, slow)
        });
        self.refresh_dirs(&child_dirs);
    }

    /// Ensure `dir` has a cached log bound to its newest generation; false
    /// when it holds no log.
    fn prepare(&mut self, dir: &Path, slow_tick: bool) -> bool {
        let tick = self.tick;
        let entry = self.logs.entry(dir.to_path_buf()).or_insert(CachedLog {
            log: None,
            last_seen: tick,
        });
        entry.last_seen = tick;
        let stale = entry.log.as_ref().is_none_or(|l| !l.log_path().is_file());
        if stale || slow_tick {
            match paths::latest_log(dir) {
                Some(path) if entry.log.as_ref().is_none_or(|l| l.log_path() != path) => {
                    entry.log = Some(DshSessionLog::open(path));
                }
                Some(_) => {}
                None => entry.log = None,
            }
        }
        entry.log.is_some()
    }

    /// Tail every log in `dirs`. First parses of large logs dominate, so they
    /// run on a small scoped worker pool, biggest first.
    fn refresh_dirs(&mut self, dirs: &[PathBuf]) {
        let wanted: HashSet<&PathBuf> = dirs.iter().collect();
        let mut pending: Vec<(u64, &mut DshSessionLog)> = self
            .logs
            .iter_mut()
            .filter(|(dir, _)| wanted.contains(dir))
            .filter_map(|(_, cached)| cached.log.as_mut())
            .map(|log| (log.pending_bytes(), log))
            .collect();
        pending.sort_by_key(|(bytes, _)| *bytes);
        let heavy = pending
            .iter()
            .filter(|(b, _)| *b > PARALLEL_PARSE_BYTES)
            .count();
        let workers = std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .min(MAX_PARSE_WORKERS)
            .min(heavy);
        if workers <= 1 {
            for (_, log) in pending {
                log.refresh();
            }
            return;
        }
        let queue = std::sync::Mutex::new(pending);
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| loop {
                    let next = queue.lock().ok().and_then(|mut q| q.pop());
                    match next {
                        Some((_, log)) => log.refresh(),
                        None => break,
                    }
                });
            }
        });
    }

    fn transcript(&self, dir: &Path) -> Option<&DshTranscript> {
        Some(&self.logs.get(dir)?.log.as_ref()?.transcript)
    }

    fn evict(&mut self) {
        let tick = self.tick;
        self.logs
            .retain(|_, cached| cached.last_seen + EVICT_AFTER_TICKS >= tick);
    }
}

impl Default for DshCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl super::AgentCollector for DshCollector {
    fn collect(&mut self, shared: &SharedProcessData) -> Vec<AgentSession> {
        self.collect_with(shared, now_ms())
    }
}

/// True when `cmd` runs a DSH host (CLI/TUI/Web or the desktop host).
pub(crate) fn is_dsh_command(cmd: &str) -> bool {
    host_surface(cmd).is_some()
}

/// True when SIGKILL on this host would end more than the selected session:
/// the desktop host, or a host whose profile serves the Web GUI.
pub(crate) fn host_serves_many_sessions(cmd: &str) -> bool {
    match host_launch(cmd) {
        Some((LaunchSurface::App, _)) => true,
        Some((_, args)) => paths::host_profile(&args)
            .is_some_and(|p| paths::profile_serves_web(&paths::dsh_home(), &p)),
        None => false,
    }
}

const ENTRY_SCRIPTS: &[&str] = &[
    "dsh/lib/bin.js",
    "apps/cli/lib/bin.js",
    "dsh-cli/lib/bin.js",
    "apps/cli/src/bin.ts",
];
const DESKTOP_ENTRY: &str = "dsh-desktop-host/lib/index.js";
const INTERPRETERS: &[&str] = &["node", "nodejs", "bun", "deno", "tsx"];
/// Interpreter flags whose value is the next token.
const VALUE_FLAGS: &[&str] = &[
    "-r",
    "--require",
    "--import",
    "--loader",
    "--experimental-loader",
    "--env-file",
    "-C",
    "--conditions",
    "--title",
    "--inspect-port",
];

fn host_surface(cmd: &str) -> Option<LaunchSurface> {
    host_launch(cmd).map(|(surface, _)| surface)
}

/// Surface and the arguments after the DSH program, when `cmd` launches DSH.
/// The program must sit in executable position (the first token, or the
/// first non-flag argument of an interpreter), so editors or pagers opening
/// DSH sources never match.
fn host_launch(cmd: &str) -> Option<(LaunchSurface, Vec<String>)> {
    // Cheap prefilter; every program form below names `dsh` or an `apps/cli` entry.
    if !cmd.contains("dsh") && !cmd.contains("cli") {
        return None;
    }
    let tokens = command_tokens(cmd);
    let (idx, program) = program_token(&tokens)?;
    let surface = if is_suffix_path(program, DESKTOP_ENTRY) {
        LaunchSurface::App
    } else if is_dsh_binary(program) || ENTRY_SCRIPTS.iter().any(|s| is_suffix_path(program, s)) {
        LaunchSurface::Cli
    } else {
        return None;
    };
    Some((surface, tokens[idx + 1..].to_vec()))
}

/// Whitespace split that keeps double-quoted runs together, with `\` → `/`.
fn command_tokens(cmd: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for ch in cmd.chars() {
        match ch {
            '"' => quoted = !quoted,
            '\\' => current.push('/'),
            c if c.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Index and value of the program an interpreter runs, or the first token.
fn program_token(tokens: &[String]) -> Option<(usize, &str)> {
    let first = tokens.first()?;
    if !is_interpreter(first) {
        return Some((0, first));
    }
    let mut i = 1;
    while let Some(tok) = tokens.get(i) {
        if tok.starts_with('-') {
            i += if VALUE_FLAGS.contains(&tok.as_str()) {
                2
            } else {
                1
            };
        } else if is_ts_runner(tok) {
            i += 1;
        } else {
            return Some((i, tok));
        }
    }
    None
}

fn is_interpreter(tok: &str) -> bool {
    INTERPRETERS.contains(&basename(tok).trim_end_matches(".exe"))
}

/// `tsx` launched through node: `node …/tsx/dist/cli.mjs script.ts`.
fn is_ts_runner(tok: &str) -> bool {
    is_suffix_path(tok, "tsx/dist/cli.mjs") || is_suffix_path(tok, "tsx/dist/cli.js")
}

/// `path` equals `suffix` or ends with `/{suffix}`.
fn is_suffix_path(path: &str, suffix: &str) -> bool {
    path.strip_suffix(suffix)
        .is_some_and(|head| head.is_empty() || head.ends_with('/'))
}

fn basename(tok: &str) -> &str {
    tok.rsplit('/').next().unwrap_or(tok)
}

fn is_dsh_binary(tok: &str) -> bool {
    matches!(basename(tok), "dsh" | "dsh.exe" | "dsh.cmd" | "dsh.js")
}

fn find_hosts(process_info: &HashMap<u32, ProcInfo>) -> Vec<Host> {
    let mut hosts: Vec<Host> = process_info
        .iter()
        .filter_map(|(&pid, info)| host_surface(&info.command).map(|surface| Host { pid, surface }))
        .collect();
    hosts.sort_by_key(|h| h.pid);
    hosts
}

/// Live session dirs from held `session.lock` leases. When two hosts report
/// the same lease (inherited fd), the descendant process owns it.
fn live_sessions(
    hosts: &[Host],
    open: &HashMap<u32, ProcessOpenPaths>,
    process_info: &HashMap<u32, ProcInfo>,
) -> Vec<(u32, PathBuf)> {
    let mut owners: Vec<(u32, PathBuf)> = Vec::new();
    for host in hosts {
        let Some(info) = open.get(&host.pid) else {
            continue;
        };
        for path in &info.paths {
            let Some(dir) = paths::session_dir_from_lease(path) else {
                continue;
            };
            match owners.iter_mut().find(|(_, d)| *d == dir) {
                Some((owner, _)) => {
                    if process::is_descendant_of(host.pid, *owner, process_info) {
                        *owner = host.pid;
                    }
                }
                None => owners.push((host.pid, dir)),
            }
        }
    }
    owners
}

/// Nearest ancestor of a folded session that is itself a row: climb through
/// live ancestors that are folded too, so grandchildren stay visible.
fn row_ancestor<'a>(
    t: &'a DshTranscript,
    by_id: &HashMap<&str, (&'a DshTranscript, bool)>,
) -> Option<&'a str> {
    let mut current = t.parent_session.as_deref()?;
    let mut seen = HashSet::new();
    loop {
        if !seen.insert(current) {
            return None;
        }
        let (ancestor, live) = by_id.get(current)?;
        if !*live {
            return None;
        }
        match ancestor.parent_session.as_deref() {
            Some(up) if by_id.get(up).is_some_and(|(_, live)| *live) => current = up,
            _ => return Some(current),
        }
    }
}

/// A live session whose parent is also live is shown as the parent's subagent.
fn is_folded(t: &DshTranscript, live_ids: &HashSet<String>) -> bool {
    t.parent_session
        .as_ref()
        .is_some_and(|p| p != &t.id && live_ids.contains(p))
}

/// Indices of the rows that carry host-level memory and child processes:
/// per PID, the most recently active row (tie → newest session).
fn primary_rows(rows: &[(u32, u64, u64)]) -> HashSet<usize> {
    let mut best: HashMap<u32, usize> = HashMap::new();
    for (i, &(pid, last_event, created)) in rows.iter().enumerate() {
        best.entry(pid)
            .and_modify(|j| {
                let (_, le, cr) = rows[*j];
                if (last_event, created) > (le, cr) {
                    *j = i;
                }
            })
            .or_insert(i);
    }
    best.into_values().collect()
}

/// Catalog/team children plus live sessions naming this one as parent.
fn subagents_for(
    parent: &DshTranscript,
    by_id: &HashMap<&str, (&DshTranscript, bool)>,
) -> Vec<SubAgent> {
    let mut refs: Vec<(String, String)> = parent
        .children
        .iter()
        .map(|c| (c.id.clone(), c.label.clone()))
        .collect();
    let mut extra: Vec<&DshTranscript> = by_id
        .values()
        .filter(|(t, live)| {
            *live
                && row_ancestor(t, by_id) == Some(parent.id.as_str())
                && !refs.iter().any(|(id, _)| id == &t.id)
        })
        .map(|(t, _)| *t)
        .collect();
    extra.sort_by_key(|t| t.created_at_ms);
    refs.extend(extra.into_iter().map(|t| (t.id.clone(), String::new())));
    let skip = refs.len().saturating_sub(MAX_CHILDREN);

    refs.into_iter()
        .skip(skip)
        .map(|(id, catalog_label)| {
            let child = by_id.get(id.as_str());
            let name = child
                .map(|(t, _)| t.label.clone())
                .filter(|l| !l.is_empty())
                .or_else(|| (!catalog_label.is_empty()).then_some(catalog_label))
                .unwrap_or_else(|| display_id(&id).chars().take(8).collect());
            let working = child.is_some_and(|(t, live)| *live && t.in_turn);
            let tokens = child.map_or(0, |(t, _)| {
                t.total_input + t.total_output + t.total_cache_read + t.total_cache_write
            });
            SubAgent {
                name: truncate(&name, 30),
                status: if working { "working" } else { "done" }.to_string(),
                tokens,
            }
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
struct StatusView {
    status: SessionStatus,
    tasks: Vec<String>,
    pending_since_ms: u64,
    thinking_since_ms: u64,
}

fn derive_status(t: &DshTranscript, now_ms: u64) -> StatusView {
    let pending_since_ms = if t.in_turn {
        t.pending_tools
            .iter()
            .map(|p| p.started_ms)
            .min()
            .unwrap_or(0)
    } else {
        0
    };
    let view = |status, tasks: Vec<String>| StatusView {
        thinking_since_ms: if status == SessionStatus::Thinking {
            if t.step_started_ms > 0 {
                t.step_started_ms
            } else {
                t.turn_started_ms
            }
        } else {
            0
        },
        status,
        tasks,
        pending_since_ms,
    };

    if let Some(tool) = &t.pending_approval {
        let task = if tool.is_empty() {
            "awaiting approval".to_string()
        } else {
            format!("awaiting approval: {tool}")
        };
        return view(SessionStatus::Waiting, vec![task]);
    }
    if !t.in_turn {
        let task = match &t.last_turn_error {
            Some(err) => format!("last turn failed: {}", truncate(err, 60)),
            None => "waiting for input".to_string(),
        };
        return view(SessionStatus::Waiting, vec![task]);
    }
    if !t.pending_tools.is_empty() {
        let skip = t.pending_tools.len().saturating_sub(3);
        let tasks = t
            .pending_tools
            .iter()
            .skip(skip)
            .map(|p| format!("{} {}", p.name, p.arg).trim().to_string())
            .collect();
        return view(SessionStatus::Executing, tasks);
    }
    if let Some(r) = t
        .retry
        .as_ref()
        .filter(|r| now_ms < r.resume_at_ms.saturating_add(RETRY_GRACE_MS))
    {
        let status = if r.code == "RATE_LIMIT" {
            SessionStatus::RateLimited
        } else {
            SessionStatus::Thinking
        };
        let task = format!("retry {}/{} ({})", r.attempt, r.max_attempts, r.code);
        return view(status, vec![task]);
    }
    if t.compacting {
        return view(
            SessionStatus::Thinking,
            vec!["compacting context".to_string()],
        );
    }
    view(SessionStatus::Thinking, vec!["thinking...".to_string()])
}

struct RowInput<'a> {
    pid: u32,
    session_dir: &'a Path,
    t: &'a DshTranscript,
    surface: LaunchSurface,
    cmd: &'a str,
    subagents: Vec<SubAgent>,
    now_ms: u64,
}

fn build_row(input: RowInput<'_>) -> AgentSession {
    let t = input.t;
    let status = derive_status(t, input.now_ms);
    let cwd = t.cwd.clone().unwrap_or_default();
    let project_name = process::last_path_segment(&cwd)
        .filter(|s| !s.is_empty())
        .unwrap_or("?")
        .to_string();
    let model = if t.model.is_empty() {
        "-".to_string()
    } else {
        t.model.clone()
    };
    let context_window = if t.context_window > 0 {
        t.context_window
    } else {
        super::context_window_for_model(&model, "", t.max_context_tokens)
    };
    let context_percent = if context_window > 0 {
        t.last_context_tokens as f64 / context_window as f64 * 100.0
    } else {
        0.0
    };
    let initial_prompt = [&t.title, &t.first_prompt, &t.goal_objective, &t.label]
        .into_iter()
        .find(|s| !s.is_empty())
        .cloned()
        .unwrap_or_default();

    AgentSession {
        agent_cli: "dsh",
        launch_surface: input.surface,
        pid: input.pid,
        session_id: display_id(&t.id).to_string(),
        cwd,
        project_name,
        started_at: t.created_at_ms,
        status: status.status,
        model,
        effort: t.reasoning_effort.clone(),
        context_percent,
        total_input_tokens: t.total_input,
        total_output_tokens: t.total_output,
        total_cache_read: t.total_cache_read,
        total_cache_create: t.total_cache_write,
        turn_count: t.model_calls,
        current_tasks: status.tasks,
        mem_mb: 0,
        version: String::new(),
        git_branch: String::new(),
        git_added: 0,
        git_modified: 0,
        token_history: t.token_history.clone(),
        context_history: t.context_history.clone(),
        compaction_count: t.compaction_count,
        context_window,
        subagents: input.subagents,
        mem_file_count: 0,
        mem_line_count: 0,
        children: Vec::new(),
        initial_prompt,
        first_assistant_text: t.first_assistant_text.clone(),
        chat_messages: t.chat_messages.clone(),
        tool_calls: t.tool_calls.clone(),
        pending_since_ms: status.pending_since_ms,
        thinking_since_ms: status.thinking_since_ms,
        file_accesses: t.file_accesses.clone(),
        config_root: config_root(input.session_dir, input.cmd),
    }
}

/// Abbreviated DSH home (or custom sessions root) plus `:{profile}`.
fn config_root(session_dir: &Path, cmd: &str) -> String {
    let root = session_dir.parent().and_then(Path::parent);
    let base = match root {
        Some(r) if r.file_name().is_some_and(|n| n == "sessions") => {
            r.parent().unwrap_or(r).to_path_buf()
        }
        Some(r) => r.to_path_buf(),
        None => session_dir.to_path_buf(),
    };
    let mut label = super::sanitize_terminal_text(&super::abbrev_path(&base));
    let args = host_launch(cmd).map(|(_, args)| args).unwrap_or_default();
    if let Some(profile) = paths::host_profile(&args) {
        label.push(':');
        label.push_str(&profile);
    }
    label
}

/// Give the primary row of a host its RSS and descendant processes.
fn attach_host_resources(row: &mut AgentSession, shared: &SharedProcessData) {
    row.mem_mb = shared
        .process_info
        .get(&row.pid)
        .map_or(0, |p| p.rss_kb / 1024);
    row.children = host_children(row.pid, shared);
}

fn host_children(pid: u32, shared: &SharedProcessData) -> Vec<ChildProcess> {
    let mut children = Vec::new();
    let mut stack: Vec<u32> = shared.children_map.get(&pid).cloned().unwrap_or_default();
    let mut visited = HashSet::new();
    while let Some(cpid) = stack.pop() {
        if !visited.insert(cpid) {
            continue;
        }
        if let Some(c) = shared.process_info.get(&cpid) {
            children.push(ChildProcess {
                pid: cpid,
                command: c.command.clone(),
                mem_kb: c.rss_kb,
                port: shared.ports.get(&cpid).and_then(|v| v.first().copied()),
            });
        }
        if let Some(grandchildren) = shared.children_map.get(&cpid) {
            stack.extend(grandchildren);
        }
    }
    children
}

/// DSH ids may carry a `session-` prefix; drop it for display.
fn display_id(id: &str) -> &str {
    id.strip_prefix("session-").unwrap_or(id)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::transcript::{ChildRef, PendingTool, RetryState};
    use super::*;
    use std::fs;

    fn proc(pid: u32, ppid: u32, cmd: &str) -> ProcInfo {
        ProcInfo {
            pid,
            ppid,
            rss_kb: 204_800,
            cpu_pct: 0.0,
            command: cmd.to_string(),
        }
    }

    fn shared(procs: Vec<ProcInfo>) -> SharedProcessData {
        let process_info: HashMap<u32, ProcInfo> = procs.into_iter().map(|p| (p.pid, p)).collect();
        let children_map = process::get_children_map(&process_info);
        SharedProcessData {
            process_info,
            children_map,
            ports: HashMap::from([(301, vec![5173])]),
            slow_tick: true,
            mcp_server_pids: HashSet::new(),
            mcp_owned_rollouts: HashSet::new(),
            mcp_suppress: true,
            desktop_rollout_fd_map: HashMap::new(),
        }
    }

    #[test]
    fn host_detection() {
        for cmd in [
            "node /Users/u/.local/bin/dsh --profile web",
            "/usr/local/bin/dsh",
            "node --enable-source-maps /opt/dsh/node_modules/@deepseek-ai/dsh/lib/bin.js",
            "node /repo/deepseek-harness/apps/cli/lib/bin.js --profile tui",
            "node /x/tsx/dist/cli.mjs apps/cli/src/bin.ts",
            r#""C:\Program Files\nodejs\node.exe" C:\Users\u\AppData\Roaming\npm\node_modules\@deepseek-ai\dsh\lib\bin.js"#,
        ] {
            assert!(is_dsh_command(cmd), "{cmd}");
            assert_eq!(host_surface(cmd), Some(LaunchSurface::Cli), "{cmd}");
        }
        let desktop = "/Applications/DSH.app/Contents/Resources/node --expose-internals /r/node_modules/@deepseek-ai/dsh-desktop-host/lib/index.js /r /p";
        assert_eq!(host_surface(desktop), Some(LaunchSurface::App));
        assert!(host_serves_many_sessions(desktop));
        for cmd in [
            "stupid-comments hook dsh",
            "grep dsh",
            "vim dsh",
            "node /x/mcp-proxy.mjs",
            "node /Users/u/.dsh/profiles/web/node_modules/dsh-supermemory/lib/mcp-proxy.mjs",
            "/usr/local/bin/dshx",
            "vim apps/cli/src/bin.ts",
            "less /repo/node_modules/@deepseek-ai/dsh/lib/bin.js",
            "code /repo/apps/desktop-host/node_modules/@deepseek-ai/dsh-desktop-host/lib/index.js",
            "node --require /x/preflight.cjs --import file:///x/loader.mjs scripts/run-gates.ts",
            "node /x/tsx/dist/cli.mjs scripts/verify-doc-refs.ts",
            "",
        ] {
            assert!(!is_dsh_command(cmd), "{cmd}");
        }
    }

    #[test]
    fn leases_map_to_live_sessions_and_descendant_wins() {
        let hosts = vec![
            Host {
                pid: 10,
                surface: LaunchSurface::Cli,
            },
            Host {
                pid: 20,
                surface: LaunchSurface::Cli,
            },
        ];
        let lease = PathBuf::from("/h/sessions/--p--/s-1/session.lock");
        let open = HashMap::from([
            (
                10,
                ProcessOpenPaths {
                    cwd: None,
                    paths: vec![
                        lease.clone(),
                        PathBuf::from("/h/sessions/--p--/s-2/session.lock"),
                        PathBuf::from("/h/sessions/--p--/s-2/session.v4.jsonl.zstd"),
                    ],
                },
            ),
            (
                20,
                ProcessOpenPaths {
                    cwd: None,
                    paths: vec![lease],
                },
            ),
        ]);
        let procs = HashMap::from([(10, proc(10, 1, "dsh")), (20, proc(20, 10, "dsh"))]);
        let live = live_sessions(&hosts, &open, &procs);
        assert_eq!(
            live,
            vec![
                (20, PathBuf::from("/h/sessions/--p--/s-1")),
                (10, PathBuf::from("/h/sessions/--p--/s-2")),
            ]
        );
    }

    #[test]
    fn primary_row_is_most_recent_per_pid() {
        let rows = [(1, 100, 5), (1, 300, 1), (2, 50, 1), (1, 300, 9)];
        let primary = primary_rows(&rows);
        assert_eq!(primary, HashSet::from([3, 2]));
    }

    fn transcript() -> DshTranscript {
        let mut t = DshTranscript::default();
        t.id = "s".into();
        t
    }

    #[test]
    fn status_mapping() {
        let now = 1_000_000;
        let mut t = transcript();
        assert_eq!(derive_status(&t, now).status, SessionStatus::Waiting);
        assert_eq!(derive_status(&t, now).tasks, vec!["waiting for input"]);

        t.last_turn_error = Some("pi-ai stream idle timeout".into());
        assert_eq!(
            derive_status(&t, now).tasks,
            vec!["last turn failed: pi-ai stream idle timeout"]
        );

        t.in_turn = true;
        t.turn_started_ms = 900;
        let v = derive_status(&t, now);
        assert_eq!(
            (v.status, v.thinking_since_ms),
            (SessionStatus::Thinking, 900)
        );
        t.step_started_ms = 950;
        assert_eq!(derive_status(&t, now).thinking_since_ms, 950);

        t.compacting = true;
        assert_eq!(derive_status(&t, now).tasks, vec!["compacting context"]);

        t.retry = Some(RetryState {
            code: "RATE_LIMIT".into(),
            attempt: 2,
            max_attempts: 5,
            resume_at_ms: now - 1_000,
        });
        let v = derive_status(&t, now);
        assert_eq!(v.status, SessionStatus::RateLimited);
        assert_eq!(v.tasks, vec!["retry 2/5 (RATE_LIMIT)"]);
        assert_eq!(v.thinking_since_ms, 0);
        t.retry.as_mut().unwrap().code = "SERVER".into();
        assert_eq!(derive_status(&t, now).status, SessionStatus::Thinking);
        t.retry.as_mut().unwrap().resume_at_ms = now - RETRY_GRACE_MS - 1;
        assert_eq!(derive_status(&t, now).tasks, vec!["compacting context"]);

        t.pending_tools = (0..4)
            .map(|i| PendingTool {
                call_id: format!("c{i}"),
                name: "Bash".into(),
                arg: format!("step {i}"),
                started_ms: 960 + i,
            })
            .collect();
        let v = derive_status(&t, now);
        assert_eq!(v.status, SessionStatus::Executing);
        assert_eq!(v.tasks, vec!["Bash step 1", "Bash step 2", "Bash step 3"]);
        assert_eq!((v.pending_since_ms, v.thinking_since_ms), (960, 0));

        t.pending_approval = Some("Write".into());
        let v = derive_status(&t, now);
        assert_eq!(v.status, SessionStatus::Waiting);
        assert_eq!(v.tasks, vec!["awaiting approval: Write"]);
    }

    #[test]
    fn subagents_merge_catalog_team_and_live_children() {
        let mut parent = transcript();
        parent.id = "p".into();
        parent.children = vec![
            ChildRef {
                id: "c-done".into(),
                label: "Write README".into(),
            },
            ChildRef {
                id: "session-c0ffee99-1".into(),
                label: String::new(),
            },
        ];
        let mut done = transcript();
        done.id = "c-done".into();
        done.total_input = 10;
        done.total_output = 5;
        let mut working = transcript();
        working.id = "c-live".into();
        working.parent_session = Some("p".into());
        working.label = "dsh-parser".into();
        working.in_turn = true;
        working.total_cache_read = 100;
        let by_id: HashMap<&str, (&DshTranscript, bool)> = HashMap::from([
            ("p", (&parent, true)),
            ("c-done", (&done, false)),
            ("c-live", (&working, true)),
        ]);
        let subs = subagents_for(&parent, &by_id);
        let view: Vec<(&str, &str, u64)> = subs
            .iter()
            .map(|s| (s.name.as_str(), s.status.as_str(), s.tokens))
            .collect();
        assert_eq!(
            view,
            vec![
                ("Write README", "done", 15),
                ("c0ffee99", "done", 0),
                ("dsh-parser", "working", 100),
            ]
        );
    }

    #[test]
    fn live_grandchildren_attach_to_the_visible_ancestor() {
        let mut lead = transcript();
        lead.id = "p".into();
        let mut child = transcript();
        child.id = "c".into();
        child.parent_session = Some("p".into());
        child.label = "worker".into();
        let mut grandchild = transcript();
        grandchild.id = "g".into();
        grandchild.parent_session = Some("c".into());
        grandchild.label = "helper".into();
        grandchild.in_turn = true;
        let by_id: HashMap<&str, (&DshTranscript, bool)> = HashMap::from([
            ("p", (&lead, true)),
            ("c", (&child, true)),
            ("g", (&grandchild, true)),
        ]);
        assert_eq!(row_ancestor(&grandchild, &by_id), Some("p"));
        let names: Vec<(String, String)> = subagents_for(&lead, &by_id)
            .into_iter()
            .map(|s| (s.name, s.status))
            .collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&("helper".into(), "working".into())));
        assert!(subagents_for(&child, &by_id).is_empty());

        let mut loop_a = transcript();
        loop_a.id = "a".into();
        loop_a.parent_session = Some("b".into());
        let mut loop_b = transcript();
        loop_b.id = "b".into();
        loop_b.parent_session = Some("a".into());
        let cyclic: HashMap<&str, (&DshTranscript, bool)> =
            HashMap::from([("a", (&loop_a, true)), ("b", (&loop_b, true))]);
        assert_eq!(row_ancestor(&loop_a, &cyclic), None);
    }

    #[test]
    fn folding_requires_a_live_parent() {
        let mut child = transcript();
        child.id = "c".into();
        child.parent_session = Some("p".into());
        assert!(is_folded(&child, &HashSet::from(["p".to_string()])));
        assert!(!is_folded(&child, &HashSet::new()));
        assert!(!is_folded(&transcript(), &HashSet::from(["p".to_string()])));
    }

    #[test]
    fn config_root_labels_home_and_profile() {
        let home = dirs::home_dir().unwrap_or_default();
        let dir = home.join(".dsh/sessions/--p--/s-1");
        assert_eq!(config_root(&dir, "node /x/dsh --profile web"), "~/.dsh:web");
        assert_eq!(
            config_root(Path::new("/srv/custom/--p--/s-1"), "dsh"),
            "/srv/custom"
        );
    }

    fn write_log(dir: &Path, rows: &[String]) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("session.v4.jsonl"), rows.join("\n") + "\n").unwrap();
        fs::write(dir.join("session.lock"), b"").unwrap();
    }

    fn header(id: &str, cwd: &str, created: u64, parent: Option<&str>) -> String {
        let parent = parent
            .map(|p| format!(r#","parentSession":"{p}","origin":"subagent""#))
            .unwrap_or_default();
        format!(
            r#"{{"type":"session","version":4,"id":"{id}","createdAt":{created},"cwd":"{cwd}","isSeeded":false,"delegationDepth":0{parent}}}"#
        )
    }

    #[test]
    fn end_to_end_collect_from_leases() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("sessions");
        let project = root.join(paths::project_key("/w/proj"));
        let lead = project.join("session-lead");
        let child = project.join("child-1");
        let other = project.join("other");
        let done_child = project.join("done-child");
        write_log(
            &lead,
            &[
                header("session-lead", "/w/proj", 1_000, None),
                r#"{"type":"request/context","seq":1,"time":1100,"data":{"provider":"anthropic","model":"claude-opus-5-5","contextWindow":1000000}}"#.into(),
                r#"{"type":"session/title","seq":2,"time":1200,"data":{"title":"Add DSH support"}}"#.into(),
                r#"{"type":"subagent/catalog","seq":3,"time":1250,"data":{"childId":"done-child","label":"Write docs"}}"#.into(),
                r#"{"type":"turn/start","seq":4,"time":1300,"data":{"turn":1}}"#.into(),
                r#"{"type":"assistant/message","seq":5,"time":1400,"data":{"usage":{"inputTokens":10,"outputTokens":20,"cacheReadTokens":200000,"cacheWriteTokens":90},"message":{"content":[{"type":"text","text":"On it"}]}}}"#.into(),
                r#"{"type":"tool/call","seq":6,"time":1500,"data":{"callId":"c1","name":"bash","arguments":"{\"command\":\"cargo test\"}"}}"#.into(),
            ],
        );
        write_log(
            &child,
            &[
                header("child-1", "/w/proj", 1_050, Some("session-lead")),
                r#"{"type":"subagent/descriptor","seq":0,"time":1060,"data":{"label":"dsh-parser"}}"#.into(),
                r#"{"type":"assistant/message","seq":1,"time":1600,"data":{"usage":{"inputTokens":1,"outputTokens":2}}}"#.into(),
            ],
        );
        write_log(
            &other,
            &[
                header("other", "/w/proj", 2_000, None),
                r#"{"type":"user/message","seq":1,"time":2100,"data":{"content":[{"type":"text","text":"hello"}],"source":{"kind":"user"}}}"#.into(),
            ],
        );
        write_log(
            &done_child,
            &[
                header("done-child", "/w/proj", 1_020, Some("session-lead")),
                r#"{"type":"assistant/message","seq":1,"time":1030,"data":{"usage":{"inputTokens":7,"outputTokens":3}}}"#.into(),
            ],
        );
        fs::remove_file(done_child.join("session.lock")).unwrap();

        let mut collector = DshCollector::with_sessions_root(root.clone());
        let lease = |d: &Path| d.join("session.lock");
        collector.open_paths = HashMap::from([
            (
                100,
                ProcessOpenPaths {
                    cwd: None,
                    paths: vec![lease(&lead), lease(&child), lease(&other)],
                },
            ),
            (
                200,
                ProcessOpenPaths {
                    cwd: None,
                    paths: vec![],
                },
            ),
        ]);
        let mut data = shared(vec![
            proc(100, 1, "node /Users/u/.local/bin/dsh --profile web"),
            proc(200, 1, "node /Users/u/.local/bin/dsh --profile tui"),
            proc(301, 100, "node vite"),
        ]);
        data.slow_tick = false;

        let sessions = collector.collect_rows_for_test(&data, 1_700);
        assert_eq!(sessions.len(), 2, "child is folded");

        let other_row = &sessions[0];
        assert_eq!(other_row.session_id, "other");
        assert_eq!(other_row.initial_prompt, "hello");
        assert_eq!(other_row.status, SessionStatus::Waiting);
        assert_eq!(
            other_row.mem_mb, 200,
            "most recent row on the host is primary"
        );
        assert_eq!(other_row.children.len(), 1);
        assert_eq!(other_row.children[0].port, Some(5173));
        assert_eq!(
            other_row.config_root,
            format!("{}:web", super::super::abbrev_path(home.path()))
        );

        let lead_row = &sessions[1];
        assert_eq!(
            lead_row.session_id, "lead",
            "display id drops the session- prefix"
        );
        assert_eq!(lead_row.agent_cli, "dsh");
        assert_eq!(lead_row.pid, 100);
        assert_eq!(lead_row.project_name, "proj");
        assert_eq!(lead_row.initial_prompt, "Add DSH support");
        assert_eq!(lead_row.status, SessionStatus::Executing);
        assert_eq!(lead_row.current_tasks, vec!["Bash cargo test"]);
        assert_eq!(lead_row.pending_since_ms, 1500);
        assert_eq!(lead_row.context_window, 1_000_000);
        assert!((lead_row.context_percent - 20.01).abs() < 0.001);
        assert_eq!(lead_row.total_cache_read, 200_000);
        assert_eq!((lead_row.mem_mb, lead_row.children.len()), (0, 0));
        let subs: Vec<(&str, &str, u64)> = lead_row
            .subagents
            .iter()
            .map(|s| (s.name.as_str(), s.status.as_str(), s.tokens))
            .collect();
        assert_eq!(
            subs,
            vec![("Write docs", "done", 10), ("dsh-parser", "done", 3)]
        );

        // Appended rows are picked up incrementally on the next tick.
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(lead.join("session.v4.jsonl"))
            .unwrap();
        use std::io::Write;
        writeln!(
            f,
            r#"{{"type":"tool/result","seq":7,"time":1800,"data":{{"message":{{"toolCallId":"c1"}}}}}}"#
        )
        .unwrap();
        let sessions = collector.collect_rows_for_test(&data, 1_900);
        let lead_row = sessions.iter().find(|s| s.session_id == "lead").unwrap();
        assert_eq!(lead_row.status, SessionStatus::Thinking);
        assert_eq!(lead_row.tool_calls[0].duration_ms, 300);

        // A released lease removes the row; unreferenced logs are evicted later.
        collector.open_paths.get_mut(&100).unwrap().paths = vec![lease(&lead)];
        let sessions = collector.collect_rows_for_test(&data, 2_000);
        assert_eq!(sessions.len(), 1);
        for _ in 0..=EVICT_AFTER_TICKS {
            collector.collect_rows_for_test(&data, 3_000);
        }
        assert!(!collector.logs.contains_key(&other));
        assert!(collector.logs.contains_key(&lead));
    }

    /// `ABTOP_DSH_LIVE=1 cargo test --release dsh_live -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn dsh_live_collect_timing() {
        if std::env::var("ABTOP_DSH_LIVE").is_err() {
            return;
        }
        let data = SharedProcessData::fetch(None, true);
        for host in find_hosts(&data.process_info) {
            let cmd = &data.process_info[&host.pid].command;
            eprintln!(
                "host {} {:?} serves_many={}",
                host.pid,
                host.surface,
                host_serves_many_sessions(cmd)
            );
        }
        let mut collector = DshCollector::new();
        let start = std::time::Instant::now();
        let rows = collector.collect_with(&data, now_ms());
        let first = start.elapsed();
        let start = std::time::Instant::now();
        let later = collector.collect_with(&data, now_ms());
        for row in later.iter().filter(|r| !r.subagents.is_empty()) {
            let subs: Vec<String> = row
                .subagents
                .iter()
                .map(|s| format!("{}:{}:{}", s.name, s.status, s.tokens))
                .collect();
            eprintln!("  {} {} -> {:?}", row.session_id, row.project_name, subs);
        }
        eprintln!(
            "dsh rows={} logs={} first={:?} steady={:?}",
            rows.len(),
            collector.logs.len(),
            first,
            start.elapsed()
        );
    }

    impl DshCollector {
        /// Collect against injected leases instead of listing real fds.
        fn collect_rows_for_test(
            &mut self,
            shared: &SharedProcessData,
            now: u64,
        ) -> Vec<AgentSession> {
            let hosts = find_hosts(&shared.process_info);
            self.collect_from_open(shared, &hosts, now)
        }
    }
}
