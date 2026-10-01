//! DSH home, session-root layout, and path-encoding ports.

use std::fs;
use std::path::{Path, PathBuf};

pub(crate) const LEASE_FILENAME: &str = "session.lock";

/// `$DSH_HOME` when set and non-blank (with `~` expanded), otherwise `~/.dsh`.
pub(crate) fn dsh_home() -> PathBuf {
    resolve_dsh_home(std::env::var("DSH_HOME").ok().as_deref())
}

fn resolve_dsh_home(env: Option<&str>) -> PathBuf {
    let home = dirs::home_dir().unwrap_or_default();
    match env.map(str::trim) {
        Some(v) if !v.is_empty() => {
            if v == "~" {
                home
            } else if let Some(rest) = v.strip_prefix("~/").or_else(|| v.strip_prefix("~\\")) {
                home.join(rest)
            } else {
                PathBuf::from(v)
            }
        }
        _ => home.join(".dsh"),
    }
}

fn is_safe_unit(unit: u16) -> bool {
    matches!(unit, 0x30..=0x39 | 0x41..=0x5A | 0x61..=0x7A | 0x2E | 0x5F | 0x2D)
}

fn escape_unit(out: &mut String, unit: u16) {
    out.push_str(&format!("~{unit:04X}"));
}

/// Port of DSH `encodeSegment`: one injective, traversal-safe path segment.
pub(crate) fn encode_segment(raw: &str) -> String {
    match raw {
        "." => return "~002E".to_string(),
        ".." => return "~002E~002E".to_string(),
        _ => {}
    }
    let mut out = String::with_capacity(raw.len());
    for unit in raw.encode_utf16() {
        if is_safe_unit(unit) {
            out.push(unit as u8 as char);
        } else {
            escape_unit(&mut out, unit);
        }
    }
    out
}

/// Port of DSH `projectKey`: the readable `--…--` project directory name for a cwd.
pub(crate) fn project_key(cwd: &str) -> String {
    let mut readable = String::with_capacity(cwd.len());
    let mut separator_run = false;
    for unit in cwd.encode_utf16() {
        if matches!(unit, 0x2F | 0x5C | 0x3A) {
            if !separator_run {
                readable.push('-');
            }
            separator_run = true;
        } else {
            if is_safe_unit(unit) {
                readable.push(unit as u8 as char);
            } else {
                escape_unit(&mut readable, unit);
            }
            separator_run = false;
        }
    }
    let slug = readable.trim_start_matches('-');
    let slug = if slug.is_empty() { "root" } else { slug };
    // The slug is ASCII, so byte slicing matches the JS UTF-16 slice.
    format!("--{}--", &slug[..slug.len().min(251)])
}

/// Session format generation named by a canonical log filename
/// (`session.jsonl`, `session.vN.jsonl`, optionally `.zstd`).
fn log_generation(name: &str) -> Option<u32> {
    let base = name.strip_suffix(".zstd").unwrap_or(name);
    let stem = base.strip_prefix("session")?.strip_suffix(".jsonl")?;
    if stem.is_empty() {
        return Some(0);
    }
    let digits = stem.strip_prefix(".v")?;
    let canonical = !digits.is_empty()
        && !digits.starts_with('0')
        && digits.bytes().all(|b| b.is_ascii_digit());
    canonical.then(|| digits.parse().ok()).flatten()
}

/// Newest session log generation inside one session directory. Symlinks and
/// non-canonical names are ignored; on a generation tie the newer mtime wins.
pub(crate) fn latest_log(session_dir: &Path) -> Option<PathBuf> {
    let mut best: Option<(u32, std::time::SystemTime, PathBuf)> = None;
    for entry in fs::read_dir(session_dir).ok()?.flatten() {
        let name = entry.file_name();
        let Some(generation) = name.to_str().and_then(log_generation) else {
            continue;
        };
        let Ok(meta) = fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if !meta.file_type().is_file() {
            continue;
        }
        let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        let better = best
            .as_ref()
            .is_none_or(|(g, m, _)| (generation, mtime) > (*g, *m));
        if better {
            best = Some((generation, mtime, entry.path()));
        }
    }
    best.map(|(_, _, path)| path)
}

/// Session directory owning a `session.lock` lease path, when the path is one.
pub(crate) fn session_dir_from_lease(path: &Path) -> Option<PathBuf> {
    if path.file_name()? != LEASE_FILENAME {
        return None;
    }
    let dir = path.parent()?;
    (!dir.as_os_str().is_empty()).then(|| dir.to_path_buf())
}

/// Newest-modified session dir under `{sessions_root}/{project_key(cwd)}`
/// that holds a log. Used where open fds are not visible (Windows).
pub(crate) fn newest_session_dir_for_cwd(sessions_root: &Path, cwd: &str) -> Option<PathBuf> {
    let project = sessions_root.join(project_key(normalize_os_cwd(cwd)));
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in fs::read_dir(project).ok()?.flatten() {
        let dir = entry.path();
        let Some(log) = latest_log(&dir) else {
            continue;
        };
        let mtime = fs::metadata(&log)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        if best.as_ref().is_none_or(|(m, _)| mtime > *m) {
            best = Some((mtime, dir));
        }
    }
    best.map(|(_, dir)| dir)
}

/// Windows reports a process cwd with a trailing separator (`C:\\proj\\`)
/// while DSH records `process.cwd()` without one; roots keep theirs.
fn normalize_os_cwd(cwd: &str) -> &str {
    let trimmed = cwd.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() || trimmed.ends_with(':') {
        cwd
    } else {
        trimmed
    }
}

/// Current git branch for `cwd` from `.git/HEAD` (worktree `gitdir:` files
/// followed); detached HEAD → 7-char sha; empty when not in a repo.
pub(crate) fn git_branch(cwd: &str) -> String {
    let mut dir = Some(Path::new(cwd));
    while let Some(d) = dir {
        let dot_git = d.join(".git");
        if let Ok(meta) = fs::metadata(&dot_git) {
            let git_dir = if meta.is_dir() {
                Some(dot_git)
            } else {
                fs::read_to_string(&dot_git).ok().and_then(|s| {
                    let target = s.trim().strip_prefix("gitdir:")?.trim().to_string();
                    let target = PathBuf::from(target);
                    Some(if target.is_absolute() {
                        target
                    } else {
                        d.join(target)
                    })
                })
            };
            return git_dir
                .and_then(|g| fs::read_to_string(g.join("HEAD")).ok())
                .map(|head| branch_from_head(&head))
                .unwrap_or_default();
        }
        dir = d.parent();
    }
    String::new()
}

fn branch_from_head(head: &str) -> String {
    let head = head.trim();
    let name = match head.strip_prefix("ref:") {
        Some(r) => {
            let r = r.trim();
            r.strip_prefix("refs/heads/").unwrap_or(r).to_string()
        }
        None => head.chars().take(7).collect(),
    };
    crate::collector::sanitize_terminal_text(&name)
        .chars()
        .take(64)
        .collect()
}

/// `version` of the nearest `package.json` above the host's entry script.
pub(crate) fn host_version(cmd: &str) -> String {
    let Some(script) = cmd
        .split_whitespace()
        .take(3)
        .filter(|tok| tok.contains('/') || tok.contains('\\'))
        .find(|tok| {
            tok.ends_with("dsh")
                || tok.ends_with(".js")
                || tok.ends_with(".mjs")
                || tok.ends_with("dsh.exe")
        })
    else {
        return String::new();
    };
    let Ok(real) = fs::canonicalize(script) else {
        return String::new();
    };
    let mut dir = real.parent();
    while let Some(d) = dir {
        if let Ok(text) = fs::read_to_string(d.join("package.json")) {
            let version = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| v.get("version")?.as_str().map(str::to_string))
                .unwrap_or_default();
            return crate::collector::sanitize_terminal_text(&version)
                .chars()
                .take(32)
                .collect();
        }
        dir = d.parent();
    }
    String::new()
}

/// Profile a DSH host boots: `--profile X`, `--profile=X`, or the first
/// positional argument (`dsh web`). Only plain names are accepted.
pub(crate) fn host_profile(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    let mut positional = None;
    while let Some(tok) = iter.next() {
        let value = if tok == "--profile" {
            iter.next().map(String::as_str)
        } else if let Some(v) = tok.strip_prefix("--profile=") {
            Some(v)
        } else {
            if positional.is_none() && !tok.starts_with('-') {
                positional = Some(tok.as_str());
            }
            continue;
        };
        return value.filter(|v| is_profile_name(v)).map(str::to_string);
    }
    positional
        .filter(|v| is_profile_name(v))
        .map(str::to_string)
}

fn is_profile_name(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 64
        && v != "."
        && v != ".."
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// Whether `{home}/profiles/{profile}/package.json` bundles the Web GUI app.
pub(crate) fn profile_serves_web(home: &Path, profile: &str) -> bool {
    let manifest = home.join("profiles").join(profile).join("package.json");
    let Ok(text) = fs::read_to_string(manifest) else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v.pointer("/dsh/profile/bundles")?.as_array().cloned())
        .is_some_and(|bundles| {
            bundles
                .iter()
                .filter_map(|b| b.as_str())
                .any(|b| b.ends_with("dsh-web-app"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_key_matches_dsh_vectors() {
        assert_eq!(
            project_key("/Users/evandro.camargo/Projects/dsh/abtop"),
            "--Users-evandro.camargo-Projects-dsh-abtop--"
        );
        assert_eq!(
            project_key("/Users/evandro.camargo/Projects/_myself/ffpm"),
            "--Users-evandro.camargo-Projects-_myself-ffpm--"
        );
        assert_eq!(project_key("/private/tmp/"), "--private-tmp---");
        assert_eq!(project_key("/Users/foo/my app"), "--Users-foo-my~0020app--");
        assert_eq!(project_key("C:\\\\work//x"), "--C-work-x--");
        assert_eq!(project_key("/"), "--root--");
        assert_eq!(project_key("/a/~b"), "--a-~007Eb--");
        assert_eq!(project_key("/é"), "--~00E9--");
        assert_eq!(project_key("/😀"), "--~D83D~DE00--");
        let long = format!("/{}", "a".repeat(400));
        assert_eq!(project_key(&long).len(), 2 + 251 + 2);
    }

    #[test]
    fn encode_segment_matches_dsh_vectors() {
        assert_eq!(
            encode_segment("session-ce46af16-e8cf-483b-a311-b66a67202c3b"),
            "session-ce46af16-e8cf-483b-a311-b66a67202c3b"
        );
        assert_eq!(encode_segment("."), "~002E");
        assert_eq!(encode_segment(".."), "~002E~002E");
        assert_eq!(encode_segment("../x"), "..~002Fx");
        assert_eq!(encode_segment("a~b c"), "a~007Eb~0020c");
    }

    #[test]
    fn dsh_home_honors_non_blank_env() {
        let home = dirs::home_dir().unwrap_or_default();
        assert_eq!(resolve_dsh_home(None), home.join(".dsh"));
        assert_eq!(resolve_dsh_home(Some("   ")), home.join(".dsh"));
        assert_eq!(
            resolve_dsh_home(Some("/opt/dsh")),
            PathBuf::from("/opt/dsh")
        );
        assert_eq!(resolve_dsh_home(Some("~/alt")), home.join("alt"));
    }

    #[test]
    fn log_generation_accepts_only_canonical_names() {
        assert_eq!(log_generation("session.jsonl"), Some(0));
        assert_eq!(log_generation("session.jsonl.zstd"), Some(0));
        assert_eq!(log_generation("session.v4.jsonl.zstd"), Some(4));
        assert_eq!(log_generation("session.v12.jsonl"), Some(12));
        for bad in [
            "session.v0.jsonl",
            "session.v04.jsonl",
            "session.V4.jsonl",
            "session.v4.jsonl.tmp",
            "session.lock",
            "other.jsonl",
        ] {
            assert_eq!(log_generation(bad), None, "{bad}");
        }
    }

    #[test]
    fn latest_log_picks_highest_generation_and_skips_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("session.v3.jsonl.zstd"), b"x").unwrap();
        fs::write(dir.path().join("session.v4.jsonl.zstd"), b"x").unwrap();
        fs::write(dir.path().join("session.lock"), b"").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            dir.path().join("session.v4.jsonl.zstd"),
            dir.path().join("session.v9.jsonl.zstd"),
        )
        .unwrap();
        assert_eq!(
            latest_log(dir.path()),
            Some(dir.path().join("session.v4.jsonl.zstd"))
        );
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(latest_log(empty.path()), None);
    }

    #[test]
    fn lease_paths_map_to_session_dirs() {
        assert_eq!(
            session_dir_from_lease(Path::new("/h/sessions/--p--/s-1/session.lock")),
            Some(PathBuf::from("/h/sessions/--p--/s-1"))
        );
        assert_eq!(
            session_dir_from_lease(Path::new("/h/sessions/--p--/s-1/session.v4.jsonl")),
            None
        );
        assert_eq!(session_dir_from_lease(Path::new("session.lock")), None);
    }

    #[test]
    fn newest_session_dir_for_cwd_uses_project_key() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join(project_key("/w/proj"));
        let old = project.join("s-old");
        let new = project.join("s-new");
        fs::create_dir_all(&old).unwrap();
        fs::create_dir_all(&new).unwrap();
        fs::write(old.join("session.v4.jsonl"), b"{}\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(new.join("session.v4.jsonl"), b"{}\n").unwrap();
        assert_eq!(
            newest_session_dir_for_cwd(root.path(), "/w/proj"),
            Some(new)
        );
        assert_eq!(newest_session_dir_for_cwd(root.path(), "/other"), None);
    }

    #[test]
    fn os_cwd_trailing_separators_are_trimmed() {
        assert_eq!(
            normalize_os_cwd("C:\\Users\\u\\proj\\"),
            "C:\\Users\\u\\proj"
        );
        assert_eq!(normalize_os_cwd("/w/proj/"), "/w/proj");
        assert_eq!(normalize_os_cwd("C:\\"), "C:\\");
        assert_eq!(normalize_os_cwd("/"), "/");
        assert_eq!(
            project_key(normalize_os_cwd("C:\\Users\\u\\proj\\")),
            "--C-Users-u-proj--"
        );
    }

    #[test]
    fn git_branch_reads_head_and_worktree_files() {
        let repo = tempfile::tempdir().unwrap();
        fs::create_dir_all(repo.path().join(".git")).unwrap();
        fs::write(repo.path().join(".git/HEAD"), "ref: refs/heads/feat/dsh\n").unwrap();
        let nested = repo.path().join("src/deep");
        fs::create_dir_all(&nested).unwrap();
        assert_eq!(git_branch(nested.to_str().unwrap()), "feat/dsh");

        let wt = tempfile::tempdir().unwrap();
        let gitdir = wt.path().join("gd");
        fs::create_dir_all(&gitdir).unwrap();
        fs::write(gitdir.join("HEAD"), "4b965686aa\n").unwrap();
        fs::write(wt.path().join(".git"), "gitdir: gd\n").unwrap();
        assert_eq!(git_branch(wt.path().to_str().unwrap()), "4b96568");
    }

    #[test]
    fn host_version_reads_nearest_package_json() {
        let root = tempfile::tempdir().unwrap();
        let lib = root.path().join("apps/cli/lib");
        fs::create_dir_all(&lib).unwrap();
        fs::write(
            root.path().join("apps/cli/package.json"),
            r#"{"version":"0.2.0-rc.2"}"#,
        )
        .unwrap();
        let bin = lib.join("bin.js");
        fs::write(&bin, "").unwrap();
        let cmd = format!("node {} --profile web", bin.display());
        assert_eq!(host_version(&cmd), "0.2.0-rc.2");
        assert_eq!(host_version("node --version"), "");
    }

    #[test]
    fn host_profile_forms() {
        let args = |s: &str| s.split_whitespace().map(str::to_string).collect::<Vec<_>>();
        assert_eq!(host_profile(&args("--profile web")).as_deref(), Some("web"));
        assert_eq!(
            host_profile(&args("--profile=tui --resume")).as_deref(),
            Some("tui")
        );
        assert_eq!(host_profile(&args("web")).as_deref(), Some("web"));
        assert_eq!(host_profile(&args("--resume")), None);
        assert_eq!(host_profile(&args("--profile ../etc")), None);
        assert_eq!(host_profile(&args("")), None);
    }

    #[test]
    fn profile_serves_web_reads_bundles() {
        let home = tempfile::tempdir().unwrap();
        for (name, bundle) in [("web", "@deepseek-ai/dsh-web-app"), ("tui", "@x/dsh-tui")] {
            let dir = home.path().join("profiles").join(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join("package.json"),
                format!(
                    r#"{{"dsh":{{"profile":{{"bundles":["@deepseek-ai/dsh-base","{bundle}"]}}}}}}"#
                ),
            )
            .unwrap();
        }
        assert!(profile_serves_web(home.path(), "web"));
        assert!(!profile_serves_web(home.path(), "tui"));
        assert!(!profile_serves_web(home.path(), "missing"));
    }
}
