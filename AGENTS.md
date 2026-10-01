# abtop

AI agent monitor for your terminal. Like btop++, but for AI coding agents.

Supports Claude Code, Codex CLI, OpenCode, and DeepSeek Harness (DSH) sessions.

## Language Policy

English is mandatory for all project-facing work and communication.

- Write all source code, comments, tests, fixtures, documentation, examples, configuration text, scripts, and user-facing strings in English.
- Use English for every GitHub artifact: issue titles and bodies, issue comments, pull request titles and descriptions, review comments, commit messages, branch names, release notes, changelogs, discussions, labels, milestones, and workflow or CI messages.
- Do not use non-English text in repository content or GitHub communication unless it is an exact external identifier, a required protocol value, or a direct quote needed for context.
- When quoting or preserving non-English input, add an English explanation and keep the non-English text as short as possible.
- If a contributor opens an issue, comment, or review in another language, respond in English and continue the thread in English.

## Architecture

```
src/
├── main.rs                 # Entry, terminal setup, event loop, --setup flag
├── app.rs                  # App state, tick logic, key handling, summary generation
├── setup.rs                # StatusLine hook installation (abtop --setup)
├── ui/
│   └── mod.rs              # All panels in single file: header, context, quota,
│                           # tokens, projects, ports, sessions, footer
├── collector/
│   ├── mod.rs              # MultiCollector orchestration, orphan port detection
│   ├── claude.rs           # Claude Code: session discovery, transcript parsing
│   ├── codex.rs            # Codex CLI: session discovery via ps+lsof, JSONL parsing
│   ├── opencode.rs         # OpenCode: session discovery via ps + SQLite DB parsing
│   ├── dsh/                # DeepSeek Harness (DSH)
│   │   ├── mod.rs          # DshCollector: lease discovery, subagent folding, status,
│   │   │                   # primary row per host PID
│   │   ├── paths.rs        # DSH home, projectKey/encodeSegment ports, latest log, leases
│   │   ├── log.rs          # Incremental zstd-frame / plaintext JSONL reader
│   │   └── transcript.rs   # DshTranscript event reducer + DshSessionLog
│   ├── open_paths.rs       # Shared PID → open paths (/proc, libproc, sysinfo, lsof)
│   ├── process.rs          # Child process tree (ps) + open ports (lsof) + git stats
│   └── rate_limit.rs       # Rate limit file reading (~/.claude/abtop-rate-limits.json)
└── model/
    ├── mod.rs              # Re-exports
    └── session.rs          # AgentSession, SessionStatus, RateLimitInfo,
                            # ChildProcess, OrphanPort, SubAgent
```

## Layout

```
┌─ ¹context (token rate sparkline + per-session context bars) ─────────┐
│  ▁▃▅▇█▇▅▃▁▃▅▇██                       S1 abtop       ████████ 82%  │
│  token rate (200pt history)            S2 prediction  █████████91%⚠ │
│                                        S3 api-server  ███      22%  │
└──────────────────────────────────────────────────────────────────────┘
┌─ ²quota ─────┐┌─ ³tokens ───┐┌─ projects ───┐┌─ ⁴ports ──────────┐
│ CLAUDE       ││ Total  1.2M ││ abtop        ││ PORT  SESSION  CMD │
│ 5h ████ 35%  ││ Input  402k ││  main +3 ~18 ││ :3000 api-srv node│
│   resets 2h  ││ Output  89k ││              ││ :8080 predict crgo│
│ 7d ██ 12%    ││ Cache  710k ││ prediction   ││                    │
│              ││ ▁▃▅▇█▇▅▃▁▃▅││  feat/x +1~2 ││ ORPHAN PORTS       │
│ CODEX        ││ Turns: 48   ││              ││ :4000 old-prj node│
│ 5h █ 9%     ││ Avg: 25k/t  ││ api-server   ││                    │
│ 7d ██ 14%    ││             ││  main ✓clean ││                    │
└──────────────┘└─────────────┘└──────────────┘└────────────────────┘
┌─ ⁵sessions ─────────────────────────────────────────────────────────┐
│ ►*CC 7336 abtop  ● Work opus  82% 1.2M  48  Edit src/pay.rs       │
│  >CD 8840 pred   ◌ Wait sonn  91% 340k  12  waiting                │
│ ─────────────────────────────────────────────────────────────────── │
│  SESSION 7336 · /Users/graykode/abtop                               │
│  Stripe payment integration...                                      │
│  └─ Edit src/pay.rs                                                 │
│  CHILDREN: 7401 cargo build                                         │
│  SUBAGENTS: explore-data ✓12k · run-tests ●8k                      │
│  MEM 4f · 12/200 │ v2.1.86 · 47m                                   │
└──────────────────────────────────────────────────────────────────────┘
```

Panel rendering priority (top to bottom):
1. **Sessions** — always visible, gets priority allocation (min 5 rows, ideal = 2/session + 7)
2. **Mid-tier** (quota, tokens, projects, ports) — split equally, shown if space allows
3. **Context** — only renders when sessions have ideal height AND surplus >= 5 rows
4. **Header** (1 row) + **Footer** (1 row) — always present

Panel descriptions:
- **¹context**: Left = token rate braille sparkline (200-point history). Right = per-session context % bars with yellow/red warning.
- **²quota**: Claude + Codex rate limit gauges side-by-side (5h and 7d windows with reset countdown). Quota is intentionally limited to Claude and Codex; do not add an OpenCode or DSH row unless that agent exposes a reliable account-level provider rate-limit source.
- **³tokens**: Total token breakdown (in/out/cache) + per-turn sparkline for selected session.
- **projects** (always visible): Per-project git branch + added/modified file counts.
- **⁴ports**: Agent-spawned open ports + orphan ports (from dead sessions). Conflict detection.
- **⁵sessions**: Full-width panel below mid row. Session list table (top) + selected session detail (bottom), separated by divider. DSH rows are labeled `~DS` in DeepSeek blue `#4D6BFE`.

## Data Sources

All read-only from local filesystem + `ps` + `lsof`. No API calls, no auth.

### 1. Claude Code session discovery: process + config-root mapping

Discovery strategy:
1. Find running `claude` processes via `ps`
2. Map PID → open files/directories via `lsof`
3. Infer Claude config roots from open paths that contain `sessions/` and `projects/`
4. Read `{config-root}/sessions/{PID}.json`, falling back to scanning session files for the matching embedded PID
5. Parse `{config-root}/projects/{encoded-path}/{sessionId}.jsonl`

Fallback config roots are still scanned: `~/.claude`, direct home profile roots matching `~/.claude-*` when they contain both `sessions/` and `projects/`, `claude_config_dirs` from `~/.config/abtop/config.toml`, abtop's own `CLAUDE_CONFIG_DIR`, and on Linux any `CLAUDE_CONFIG_DIR` read from `/proc/{pid}/environ`.

Session file format:
```json
{ "pid": 7336, "sessionId": "2f029acc-...", "cwd": "/Users/graykode/abtop", "startedAt": 1774715116826, "kind": "interactive", "entrypoint": "cli" }
```
- ~170 bytes. Created on start, deleted on exit.
- Verify PID alive with shared `ps` data containing a `claude` binary.
- Skip sessions whose PID descends from abtop's own `claude --print` summary children without hiding user-spawned non-interactive sessions.

### 2. Claude Code transcript: `{config-root}/projects/{encoded-path}/{sessionId}.jsonl`
Path encoding: `/Users/foo/bar` → `-Users-foo-bar`

Key line types:

**`assistant`** (tokens, model, tools):
```json
{
  "type": "assistant",
  "timestamp": "2026-03-28T15:25:55.123Z",
  "message": {
    "model": "claude-opus-4-6",
    "stop_reason": "end_turn",
    "usage": {
      "input_tokens": 2,
      "output_tokens": 5,
      "cache_read_input_tokens": 11313,
      "cache_creation_input_tokens": 4350
    },
    "content": [
      { "type": "text", "text": "..." },
      { "type": "tool_use", "name": "Edit", "input": { "file_path": "src/main.rs", ... } }
    ]
  }
}
```

**`user`** (prompts, version):
```json
{ "type": "user", "timestamp": "...", "version": "2.1.86", "gitBranch": "main", "message": { "role": "user", "content": "..." } }
```

**`last-prompt`** (session tail marker):
```json
{ "type": "last-prompt", "lastPrompt": "...", "sessionId": "..." }
```

- **Size: 1KB–18MB**. Append-only, new line per message.
- **Reading strategy**: On first discovery, scan full file to build cumulative token totals. Then watch file size — on growth, read only new bytes appended since last read (track file offset). This gives both lifetime totals and real-time updates without re-reading.
- **Partial line handling**: new bytes may end mid-JSON-line. Buffer incomplete lines until next read.
- **File rotation**: if file shrinks (session restart), reset offset to 0 and re-scan.

### 3. Codex CLI sessions: `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`

Discovery strategy:
1. Find running `codex` processes via `ps`
2. Map PID → open `rollout-*.jsonl` file via `lsof`
3. Parse JSONL for `session_meta`, `token_count` (includes rate_limits), `agent_message` events
4. Detect finished sessions: scan today's directory for JSONL < 5 min old not owned by running process

Rate limits extracted from `token_count` events:
```json
{
  "rate_limits": {
    "limit_id": "codex",
    "primary": { "used_percent": 9.0, "window_minutes": 300, "resets_at": 1774686045 },
    "secondary": { "used_percent": 14.0, "window_minutes": 10080, "resets_at": 1775186466 },
    "plan_type": "plus"
  }
}
```

### 4. OpenCode sessions: `~/.local/share/opencode/opencode.db`
- Discover running `opencode` processes via shared `ps` data.
- Read recent sessions from OpenCode's SQLite DB through `sqlite3 -readonly -json`.
- Match live PIDs to DB sessions by process cwd. OpenCode does not expose a PID/session mapping, so when multiple DB rows share one cwd, only live PIDs should be assigned and older rows should not be shown as live duplicates.
- OpenCode contributes session/token/project/port data, but not quota data. Quota remains Claude + Codex only.

### 5. DSH sessions: `$DSH_HOME/sessions/{projectKey}/{sessionId}/session.vN.jsonl.zstd`

Home = `$DSH_HOME` when set and non-blank, else `~/.dsh`; sessions root = `{home}/sessions`. Layout `{root}/{projectKey(cwd)}/{encodeSegment(id)}/`; sessions without a cwd live under `{root}/_no-cwd/`.

Discovery strategy (leases, not file scans):
1. Find DSH hosts in shared `ps` data (`is_dsh_command`). The program must be in executable position: the first token, or the first non-flag argument of an interpreter (`node`, `bun`, `deno`, `tsx`; value-taking flags like `--require X` and a `tsx/dist/cli.mjs` runner are skipped; double quotes group Windows paths). Programs: a `dsh` binary, an entry script ending in `dsh/lib/bin.js`, `apps/cli/lib/bin.js`, `dsh-cli/lib/bin.js`, or `apps/cli/src/bin.ts`, or `dsh-desktop-host/lib/index.js` (desktop host → `LaunchSurface::App`). Editors, pagers, and `grep` touching those paths never match.
2. Map PID → open paths via `collector/open_paths.rs`. Every open `session.lock` whose parent dir holds a session log is one live session: the host holds that flock lease while the session is attached. Linux/macOS re-map every tick; lsof platforms cache and refresh on the slow tick or when the PID set changes.
3. Windows (no fd list): per PID, `cwd` → `{root}/{projectKey(cwd)}` → newest-modified session dir → one session per PID.
4. Log file = highest generation among canonical names: `session.v{N}.jsonl.zstd` (default), `session.v{N}.jsonl`, legacy v0 `session.jsonl[.zstd]`. Older generations can sit next to the newest; the newest is complete and authoritative. Symlinks are ignored (fail closed). Resolved per dir; re-resolved on the slow tick or when missing.
5. Rows exist only while the lease is held; there is no Done state for DSH. Cached logs of dirs neither live nor referenced for 30 ticks are evicted.

Path encoding (ports of DSH `format.ts`, compared per UTF-16 unit):
- `projectKey(cwd)`: runs of `/` `\` `:` → one `-`; `[A-Za-z0-9._-]` literal; anything else (including `~`) → `~XXXX` (4 uppercase hex digits); strip leading `-`; empty → `root`; result `--{slug[..251]}--`. Example: `/Users/foo/my app` → `--Users-foo-my~0020app--`.
- `encodeSegment(id)`: `.` → `~002E`, `..` → `~002E~002E`; `[A-Za-z0-9._-]` literal; anything else → `~XXXX`. UUID ids pass through unchanged.

Frame format:
- zstd log = concatenated, independently decodable, checksummed zstd frames; each frame is one JSONL batch of complete lines. A torn final frame may exist until DSH repairs it: leave it for the next read. Skippable frames are skipped; a complete frame that fails to decode or whose XXH64 checksum mismatches is skipped; an invalid structure stops reading at that offset until reset. Reserved descriptor bits (`0x18`) are invalid, as in DSH.
- Reader keeps `offset` = end of the last fully consumed frame (zstd) or last `\n` (plaintext) and reads only `[offset, len)` per tick, in 4 MiB chunks. When a read cannot consume everything (torn tail, invalid structure), the file length is remembered and nothing is re-read until it changes. Shrink or file identity change (unix `dev`+`ino`, else created time) → rewind and rebuild the transcript from scratch; the opened file must match the `lstat` identity (no symlink swap). I/O errors keep the previous state.
- One zstd decoder per thread (thread-local), not per log: frames are independent and each decoder holds a multi-MB window.
- Rows are parsed through a borrowed envelope (`type`, `time`, raw `data`); only the `data` of the types below is deserialized. Assistant rows carry multi-MB `stream` arrays that are never materialized.

Row shapes (row 1 is the header, every other row is an event):
```json
{ "type": "session", "version": 4, "id": "36b5d4fa-...", "createdAt": 1790802960304, "cwd": "/Users/foo/bar", "parentSession": "session-ce46af16-...", "isSeeded": false, "origin": "subagent", "delegationDepth": 1, "agentPreset": "standard" }
{ "type": "assistant/message", "seq": 42, "time": 1790802971234, "data": { "usage": { "inputTokens": 4, "outputTokens": 812, "cacheReadTokens": 79007, "cacheWriteTokens": 649 }, "message": { "content": [ ... ] } } }
```
Event envelope: `{type, seq, time(ms), data, surfaceOp?, sourceEventSeqs?}`.

Event vocabulary (all other types ignored):

| type | data used | effect |
|---|---|---|
| `model/selection` | `provider`, `model`, `reasoningEffort` | model route; effort |
| `request/context` | `provider`, `model`, `contextWindow` | model route; `context_window` |
| `subagent/descriptor` | `label`, `agentProvider`, `agentModel`, `agentReasoningEffort` | own label; route fallback |
| `subagent/catalog` | `childId`, `label` | child list (deduped by id) |
| `team/member` | `member.id`, `member.name` | child list (teammates; deduped by id) |
| `goal/change` | `goal.objective` | first objective, title fallback when no human prompt exists |
| `turn/start` | `turn` | in turn; clears last turn error |
| `step/start` / `step/end` | — | model call in flight (thinking timer) / done |
| `assistant/message` | `usage`, `message.content[]` (`text`/`reasoning`/`tool-call`), `message.source.model` | token totals + histories; `model_calls += 1`; clears step + retry; first assistant text; chat |
| `tool/call` | `callId`, `name`, `arguments` (JSON **string**) | pending tool; `ToolCall`; file audit |
| `tool/result` | `message.toolCallId` (fallback `message.source.callId`) | closes pending tool; duration = `max(1, time - started)` |
| `approval/asked` / `approval/decided` | `toolName` / — | pending approval set / cleared |
| `llm/retry` | `retry`, `maxRetries`, `delayMs`, `failure.code` | retry state; `resume_at_ms = time + delayMs` |
| `compaction/start` / `compaction/end` | — | compacting / `compaction_count += 1` |
| `turn/end` | `reason.kind`, `reason.error.message` | leaves turn; closes pending tools with their elapsed time; clears step, approval, retry, compacting; `error`/`aborted` → last turn error (provider JSON envelopes unwrapped: `429 {"error":{"message":"x"}}` → `429 x`) |
| `session/title` | `title` | session title (latest non-empty wins) |
| `user/message` | `source.kind == "user"` only; `content[]` text | first prompt; chat |
| `session/end-seed` | `inherited` | closes in-flight lifecycle like `turn/end`; seeded-prefix reset (below) |

Tool display names: `bash`→`Bash`, `read`→`Read`, `edit`→`Edit`, `write`→`Write`, `grep`→`Grep`, `glob`→`Glob`, `skill`→`Skill`, `subagent`/`subagent_fork`/`spawn_teammate`→`Agent`; others unchanged. Arg = first present of `file_path` (last 2 segments), `command` (first line, redacted, 40 chars), `pattern`, `path`, `url`, `query`, `name`. File audit from `file_path` for Read/Edit/Write. Tool results, file contents, and reasoning text are never stored.

Usage and context:
- Usage counts are DISJOINT: `inputTokens` is uncached input; `cacheReadTokens` and `cacheWriteTokens` are separate. They map 1:1 to `total_input_tokens`, `total_cache_read`, `total_cache_create`, `total_output_tokens`.
- Current context = latest call's prompt = `inputTokens + cacheReadTokens + cacheWriteTokens` (no double count, unlike Claude). Window = latest `request/context.contextWindow` when > 0, else `context_window_for_model`. `turn_count` = completed model calls.
- Seeded-prefix rule: a forked session's header has `isSeeded: true` and its log starts with rows copied from the parent, ending at `session/end-seed {inherited: true}`. At that cut, usage totals, histories, `model_calls`, `compaction_count`, and the child list reset so the parent's tokens and delegations are not counted twice. Context size is kept: the inherited prefix is still part of the fork's prompt. `session/end-seed {}` is a resume boundary: it only closes the in-flight lifecycle.

Row fields: `session_id` = header id without a leading `session-` (display only; folding uses full ids); `started_at` = `createdAt`; title = `session/title`, else first prompt, else goal objective, else own label; `git_branch` read from `.git/HEAD` walking up from cwd (handles `gitdir:` files; detached → 7-char sha); `version` = nearest `package.json` above the canonicalized entry script; `config_root` = abbreviated DSH home plus `:{profile}` (from `--profile X` or a positional such as `dsh web`); memory fields 0.

Subagents: a live session whose `parentSession` is also live is not a row; it becomes a `SubAgent` of its nearest ancestor that is a row (grandchildren climb through folded parents; cycles are dropped), `working` while in turn, else `done`. Children = catalog + team members + live sessions naming the parent, most recent 20. Children that are not live are resolved at `{project dir}/{encodeSegment(childId)}`, parsed once (cached), and shown as `done`; at most 16 new child logs are parsed per tick, so subagent token counts fill in over the first few ticks. Name = child label → catalog label → first 8 chars of the display id.

Cost: first parses dominate (a 15 MB decompressed log takes ~50 ms release). Logs with > 256 KiB unread are refreshed on a scoped pool of ≤ 8 threads, largest first; steady-state ticks only stat each log and decode appended frames.

### 6. Claude Code subagents: `~/.claude/projects/{path}/{sessionId}/subagents/`
- `agent-{hash}.jsonl` — same JSONL format as main transcript
- `agent-{hash}.meta.json` — `{ "agentType": "general-purpose", "description": "..." }`

### 7. Process tree: `ps` + `lsof`
```bash
ps -eo pid,ppid,rss,%cpu,command    # All processes
lsof -i -P -n -sTCP:LISTEN         # Open ports
```
- Build parent→children map from ppid
- Map listening PID → parent agent PID → session

### 8. Git status per project
```bash
git -C {cwd} status --porcelain     # added/modified file counts
```

### 9. Memory status
- Path: `~/.claude/projects/{encoded-path}/memory/`
- Count files in directory + lines in `MEMORY.md`

### 10. Rate limit (Claude Code)

NOT in transcript JSONL. Collected via StatusLine mechanism.

`abtop --setup` automates this: creates a script at `~/.claude/abtop-statusline.sh` that writes rate limit JSON to `~/.claude/abtop-rate-limits.json`, and registers it in `~/.claude/settings.json`.

File format read by abtop:
```json
{
  "source": "claude",
  "five_hour": { "used_percentage": 35.0, "resets_at": 1774715000 },
  "seven_day": { "used_percentage": 12.0, "resets_at": 1775320000 },
  "updated_at": 1774714400
}
```
- Rejects stale data (> 10 minutes old).
- `rate_limits` only present for Pro/Max subscribers.
- Account-level metric, shared across all sessions.
- Show "—" when not configured or data unavailable.

### 11. Other files
- `~/.claude/stats-cache.json` — daily aggregates. Only updated on `/stats`, NOT real-time.
- `~/.claude/history.jsonl` — prompt history with sessionId.

## Session Status Detection

```
● Working  = PID alive + transcript mtime < 30s ago
◌ Waiting  = PID alive + transcript mtime > 30s ago
✗ Error    = PID alive + last assistant has error content
✓ Done     = PID dead (detected via kill(pid, 0) failure)
```

**Done detection**: session files are deleted on normal exit, but may linger briefly or survive crashes. When PID is dead but file exists, show as Done and clean up on next tick.

**PID reuse risk**: verify PID is still the expected agent process (Claude, Codex, OpenCode, or DSH) by checking `ps -p {pid} -o command=`. Don't trust PID alone.

Current task (2nd line under each session):
- Working → last `tool_use` name + first arg (e.g. `Edit src/main.rs`)
- Waiting → "waiting for user input"
- Error → last error message (truncated)
- Done → "finished {duration} ago"

**Known limitations** (all heuristic):
- Cannot distinguish model-thinking vs tool-executing vs rate-limit-waiting vs permission-prompt
- "Waiting" may be wrong if a long-running tool (cargo build, npm test) is running
- Status is best-effort, not authoritative

**DSH status is event-driven**, not mtime-based: it is derived from the reduced log state, so thinking vs tool vs retry vs approval are distinguishable. First match wins:

| Condition | Status | Current task |
|---|---|---|
| pending approval | Waiting | `awaiting approval: {tool}` |
| in turn + pending tools | Executing | `{name} {arg}` (up to 3) |
| in turn + retry, now < `resume_at_ms` + 30s | RateLimited if `failure.code == "RATE_LIMIT"`, else Thinking | `retry {n}/{max} ({code})` |
| in turn + compacting | Thinking | `compacting context` |
| in turn | Thinking | `thinking...` |
| otherwise | Waiting | `waiting for input`, or `last turn failed: {err}` (60 chars) |

`pending_since_ms` = oldest pending tool start (0 when none); `thinking_since_ms` = `step/start` time (fallback `turn/start`) only while Thinking. A released lease removes the row, so DSH never shows Done.

## Session Summary Generation

Each session gets a one-line summary title generated via `claude --print`:
- Spawned as background process with 10s timeout
- Rejects generic/empty output; falls back to sanitized first prompt (28 chars)
- Cached to `~/.cache/abtop/summaries.json` (persists across runs)
- Max 3 concurrent summary jobs, max 2 retries per session
- Skipped for DSH (`agent_cli == "dsh"`): DSH writes its own `session/title`, which is used as-is, so no Claude quota is spent on DSH sessions

## Context Window Calculation

Not provided in data files. Derive:
- **Window size**: hardcode by model name
  - `claude-opus-4-6` → 200,000 (default)
  - `claude-opus-4-6[1m]` → 1,000,000
  - `claude-sonnet-4-6` → 200,000
  - `claude-haiku-4-5` → 200,000
- **Current usage**: last `assistant` line's `input_tokens + cache_read_input_tokens`. `cache_creation_input_tokens` is intentionally excluded — on compaction turns the same tokens can be reported as both `cache_creation` *and* `cache_read`, and summing all three double-counts (#54). Matches Claude Code's own statusline and the Codex collector.
- **DSH**: window from `request/context.contextWindow` (model table only as fallback); usage = latest `inputTokens + cacheReadTokens + cacheWriteTokens`, which is safe to sum because DSH counts are disjoint.
- **Percentage**: current_usage / window_size * 100
- **Warning**: yellow at 80%, red at 90%, ⚠ icon at 90%+

## Orphan Port Detection

Tracks child processes that have open ports. When a parent session dies but the child process remains alive and listening:
- Added to `orphan_ports` list automatically
- Displayed in ports panel under "ORPHAN PORTS" section
- Can be killed via `X` (Shift+X) with safety checks (fresh port scan + PID command verification before SIGKILL)

## Key Bindings

| Key | Action |
|-----|--------|
| `↑`/`↓` or `k`/`j` | Select session in list |
| `Enter` | Jump to session terminal (cmux / tmux / iTerm2) |
| `x` | Kill selected session (SIGKILL) |
| `X` | Kill all orphan ports |
| `q` | Quit |
| `r` | Force refresh |

## Tech Stack

- **Rust** (2021 edition)
- **ratatui** + **crossterm** for TUI
- **serde** + **serde_json** for JSON/JSONL parsing
- **chrono** for timestamp formatting
- **dirs** for home directory resolution
- **Polling intervals** (staggered to avoid freezes):
  - Session scan + transcript tail: every 2s
  - Process tree (ps): every 2s
  - Port scan (lsof) + git status + rate limits: every 10s (5 ticks)

## Commit Convention

```
<type>: <description>
```
Types: `feat`, `fix`, `refactor`, `docs`, `chore`

## Commands

`make help` lists every target. The Makefile builds into the shared `~/.rust/target` (override with `CARGO_TARGET=...`) and installs into `~/.local/bin` (override with `PREFIX=...`).

```bash
make prereqs                   # Verify cargo, rustc >= rust-version, target and install dirs
make build                     # Release build (runs prereqs first)
make rebuild                   # clean + build, sequentially even under -j
make install                   # Copy into BINDIR; reports version, sha256, signature, PATH shadowing
make check                     # clippy --all-targets -D warnings + tests (pre-push gate)
make ci                        # The exact steps of .github/workflows/ci.yml
make cross-check               # Linux clippy + tests and Windows-target clippy in Docker (rust:latest)
make fmt-check                 # rustfmt check; not in `check` because upstream main is not rustfmt-clean
make once / make demo          # Live / demo snapshot
make run ARGS="--theme nord"   # TUI from source
```

Plain cargo:

```bash
cargo build                    # Build
cargo run                      # Run TUI
cargo run -- --once            # Print snapshot and exit
cargo run -- --setup           # Install StatusLine hook for rate limit collection
cargo run -- --exit-on-jump    # Quit after Enter-jumping to a session terminal (for popup overlays)
cargo test                     # Tests
cargo clippy                   # Lint
```

## Release Process

1. Pick the target semver version and update both `Cargo.toml` and `Cargo.lock`.
2. Verify the package locally:
   ```bash
   cargo test
   cargo clippy -- -D warnings
   cargo build --release
   cargo publish --dry-run
   ```
3. Commit and merge or push the version bump to `main`:
   ```bash
   git add Cargo.toml Cargo.lock
   git commit -m "chore: bump version to X.Y.Z"
   git push origin main
   ```
4. From a clean, up-to-date `main`, create and push an annotated release tag:
   ```bash
   git tag -a vX.Y.Z -m "vX.Y.Z"
   git push origin vX.Y.Z
   ```
5. Watch the tag-triggered workflows:
   ```bash
   gh run list --workflow Release --limit 5
   gh run list --workflow "Publish to crates.io" --limit 5
   ```
6. `release.yml` builds platform binaries, creates the GitHub Release, and updates the Homebrew formula.
7. `publish.yml` runs `cargo publish` to crates.io automatically.

**Do NOT run `cargo publish` or `gh release create` manually** — the CI workflows handle both.
**Do NOT push the tag before the version bump is on `main`.**
**Do NOT reuse a release tag after a failed publish; bump to a new patch version instead.**

## Non-Goals (v0.1)

- Gemini/Cursor support
- Cost estimation
- Remote/SSH monitoring
- Notifications/alerts

## Terminal Jump (`Enter`)

`Enter` focuses the terminal running the selected session's agent process.
The logic lives in `src/jump/` as a registry of `TerminalJumper` adapters
(one file per backend). `jumpers()` is the single ordered source of truth;
`resolve()` walks it and the first applicable adapter wins.

Each adapter returns a three-way `JumpAttempt`:
- `NotApplicable` — not this backend's terminal; try the next adapter.
- `Jumped` — focused successfully; stop.
- `Failed(msg)` — this backend owns the process but the focus command errored;
  stop and surface `"<backend>: <msg>"` in the status line.

Order (most specific first), mutually exclusive by controlling tty:

1. **cmux** (`jump/cmux.rs`) — reads `CMUX_WORKSPACE_ID` (a UUID cmux exports
   into every surface, inherited by the agent) from the process environment via
   `ps eww`, then `cmux select-workspace --workspace <uuid>`.
2. **tmux** (`jump/tmux.rs`) — only when abtop itself runs inside tmux (`$TMUX`).
   Maps PID → pane via `tmux list-panes -a -F '#{pane_pid} #{session_name}:#{window_index}.#{pane_index}'`
   + process-tree descent, then `switch-client` / `select-window` / `select-pane`.
   PID in no pane → `NotApplicable` (lets another backend try).
3. **iTerm2** (`jump/iterm2.rs`) — resolves the PID's controlling tty (`ps -o tty=`),
   then AppleScript selects the session whose `tty` matches and brings its
   window/app to the front. First call triggers a one-time macOS Automation
   permission prompt; until granted, `osascript` exits non-zero → `Failed`.

Parsing/registry logic is unit-tested in `jump/mod.rs`; the thin `ps`/`osascript`/
`tmux` I/O wrappers are verified manually.

## Privacy

abtop reads transcripts, prompts, tool inputs, and memory files. These may contain secrets.
- **`--once` output**: redact file contents from tool_use inputs. Show tool name + file path only, not content.
- **TUI mode**: show tool name + first arg (file path), never show file contents or prompt text in session list.
- **No network**: abtop never sends data anywhere. All local reads.
- **Exception**: summary generation calls `claude --print` locally (no network by abtop itself, but claude may use its API). Never for DSH sessions.

## Gotchas

- **Transcript size**: 1KB–18MB. On first load, full scan for totals. After that, track file offset and read only new bytes. Buffer partial lines.
- **Session file deletion**: files disappear when Claude exits. Handle `NotFound` between scan and read.
- **stats-cache.json is stale**: only updated on `/stats` command. Don't use for live data.
- **Context window not in data**: must hardcode per model. Will break if Anthropic/OpenAI add new models.
- **Rate limit is account-level**: shared across all sessions. Don't show per-session.
- **Path encoding**: `/Users/foo/bar` → `-Users-foo-bar`. Used for transcript directory names.
- **Path encoding collision**: `-Users-foo-bar-baz` could be `/Users/foo/bar-baz` or `/Users/foo-bar/baz`. Use session JSON's `cwd` as source of truth.
- **lsof can be slow**: on macOS with many open files. Cache results, poll every 10s.
- **Child process tree**: `pgrep -P` only gets direct children. Build full tree from `ps -eo ppid`.
- **Port detection race**: a port can close between lsof and display. Show stale data gracefully.
- **Subagent directory may not exist**: only created when Agent tool is used. Check existence before scanning.
- **Undocumented internals**: all data sources are Claude Code/Codex/OpenCode/DSH implementation details, not stable APIs. Schema may change without notice. Defensive parsing with `serde(default)` everywhere.
- **Terminal size**: minimum 80x24. Panels degrade gracefully when small (context panel hidden first).
- **PID reuse in port cache**: invalidate cached ports when the set of tracked PIDs changes.
- **Rate limit staleness**: reject rate limit data older than 10 minutes.
- **`/clear` + multi-PID same cwd**: after `/clear`, Claude Code mints a new `sessionId` + `.jsonl` without rewriting `sessions/{PID}.json`. abtop overrides the stale sid by picking the newest transcript in the project dir, but this heuristic can't disambiguate ownership when two live `claude` PIDs share a cwd — so the override is disabled in that case and both sessions keep their original sid until exit. Use separate worktrees if live tracking is needed on both simultaneously.
- **DSH multi-session hosts**: one `dsh --profile web` or desktop host can hold dozens of leases, so many rows share one PID (a TUI host holds one). Per host PID, the row with the greatest last event time (tie → newest `createdAt`) is primary and gets `mem_mb` (host RSS) and `children` with ports; other rows of that host get `mem_mb = 0` and no children. `AgentAggregate` sums `mem_mb`, so without this rule one host's memory would be counted once per session.
- **DSH kill guard**: `x` refuses when the selected PID hosts more than one listed session (`PID {pid} hosts {n} sessions; not killing`), and on any DSH Web or desktop host even with one session (`PID {pid} is a DSH Web/desktop host; not killing`), because SIGKILL would end the GUI and everything it could attach. A Web host is one whose profile (`--profile X`, `--profile=X`, or the first positional as in `dsh web`) lists a bundle ending in `dsh-web-app` in `$DSH_HOME/profiles/{profile}/package.json` (`dsh.profile.bundles`). Single-session TUI hosts stay killable.
- **DSH titles**: DSH writes its own `session/title`, used as the session title (first prompt as fallback). `claude --print` summaries are never spawned for `agent_cli == "dsh"`.
- **DSH on Windows**: no open-fd list, so leases are invisible. Fallback is `cwd` → project dir → newest-modified session dir, one session per PID; a multi-session host shows a single row there. `sysinfo` must refresh with `with_cwd(UpdateKind::Always)` or `cwd()` is always `None`, and the PEB cwd carries a trailing `\` that is trimmed before `projectKey` (DSH records `process.cwd()` without it).
- **DSH is read-only**: abtop never writes, locks, deletes, or repairs anything under `$DSH_HOME`. It only observes leases held by DSH hosts and tails the logs; a torn final frame is left for DSH to repair.
- **DSH quota**: intentionally not shown. DSH exposes no account-level rate-limit source; `llm/retry` with `RATE_LIMIT` only drives the per-session RateLimited status.
- **DSH seeded forks**: a forked log replays the parent's prefix. Always honor the `session/end-seed {inherited: true}` reset, or the parent's tokens are double-counted in totals and aggregates.
