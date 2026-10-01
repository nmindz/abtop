//! Per-PID open-file discovery shared by collectors that map agent processes
//! to on-disk session state (Claude session files, DSH session leases).

use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::fs;
use std::path::PathBuf;
#[cfg(all(
    not(target_os = "linux"),
    not(target_vendor = "apple"),
    not(target_os = "windows")
))]
use std::process::Command;

/// Open paths of one process. `cwd` is only populated where the platform
/// exposes it cheaply (Linux `/proc`, lsof, sysinfo).
#[derive(Debug, Default)]
pub(crate) struct ProcessOpenPaths {
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) paths: Vec<PathBuf>,
}

/// Map each PID to the absolute paths of its open file descriptors.
pub(crate) fn map_pid_to_open_paths(pids: &[u32]) -> HashMap<u32, ProcessOpenPaths> {
    if pids.is_empty() {
        return HashMap::new();
    }

    #[cfg(target_os = "linux")]
    {
        map_pid_to_proc_open_paths(pids)
    }

    #[cfg(target_vendor = "apple")]
    {
        map_pid_to_libproc_open_paths(pids)
    }

    #[cfg(target_os = "windows")]
    {
        map_pid_to_sysinfo_open_paths(pids)
    }

    #[cfg(all(
        not(target_os = "linux"),
        not(target_vendor = "apple"),
        not(target_os = "windows")
    ))]
    {
        map_pid_to_lsof_open_paths(pids)
    }
}

#[cfg(target_os = "linux")]
fn map_pid_to_proc_open_paths(pids: &[u32]) -> HashMap<u32, ProcessOpenPaths> {
    let mut map = HashMap::new();

    for &pid in pids {
        let cwd = fs::read_link(format!("/proc/{}/cwd", pid)).ok();
        let entries = match fs::read_dir(format!("/proc/{}/fd", pid)) {
            Ok(entries) => entries,
            Err(_) => {
                if cwd.is_some() {
                    map.insert(
                        pid,
                        ProcessOpenPaths {
                            cwd,
                            paths: Vec::new(),
                        },
                    );
                }
                continue;
            }
        };

        let paths = entries
            .flatten()
            .filter_map(|entry| fs::read_link(entry.path()).ok())
            .filter(|path| path.is_absolute())
            .collect();
        map.insert(pid, ProcessOpenPaths { cwd, paths });
    }

    map
}

#[cfg(target_vendor = "apple")]
fn map_pid_to_libproc_open_paths(pids: &[u32]) -> HashMap<u32, ProcessOpenPaths> {
    use proc_pidinfo::{
        proc_pidfdinfo, proc_pidinfo_list, Pid, ProcFDInfo, ProcFDType, VnodeFdInfoWithPath,
    };

    let mut map = HashMap::new();

    for &raw_pid in pids {
        let pid = Pid(raw_pid);
        let fds = match proc_pidinfo_list::<ProcFDInfo>(pid) {
            Ok(fds) => fds,
            Err(_) => continue,
        };

        let paths = fds
            .into_iter()
            .filter(|fd| fd.fd_type() == Ok(ProcFDType::VNODE))
            .filter_map(|fd| proc_pidfdinfo::<VnodeFdInfoWithPath>(pid, fd.proc_fd).ok())
            .flatten()
            .filter_map(|vnode| vnode.path().ok().map(PathBuf::from))
            .collect();

        map.insert(raw_pid, ProcessOpenPaths { cwd: None, paths });
    }

    map
}

#[cfg(target_os = "windows")]
fn map_pid_to_sysinfo_open_paths(pids: &[u32]) -> HashMap<u32, ProcessOpenPaths> {
    use std::sync::{Mutex, OnceLock};

    static SYS: OnceLock<Mutex<sysinfo::System>> = OnceLock::new();
    let sys_mutex = SYS.get_or_init(|| Mutex::new(sysinfo::System::new()));
    let mut sys = sys_mutex.lock().expect("open-paths system mutex poisoned");

    let pids_sys: Vec<sysinfo::Pid> = pids
        .iter()
        .copied()
        .map(|p| sysinfo::Pid::from(p as usize))
        .collect();
    sys.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&pids_sys),
        true,
        // cwd is only read when requested; without it `cwd()` is always None.
        sysinfo::ProcessRefreshKind::new()
            .with_memory()
            .with_cwd(sysinfo::UpdateKind::Always),
    );

    // sysinfo 0.32 exposes cwd but not open file descriptors, so the `paths`
    // fallback used by lsof/libproc on other platforms isn't available here.
    // Claude session discovery still works via cwd plus the session-file index.
    let mut map: HashMap<u32, ProcessOpenPaths> = HashMap::new();
    for &pid_u32 in pids {
        let pid = sysinfo::Pid::from(pid_u32 as usize);
        if let Some(proc_) = sys.process(pid) {
            let cwd = proc_.cwd().map(PathBuf::from);
            map.insert(pid_u32, ProcessOpenPaths { cwd, paths: vec![] });
        }
    }
    map
}

#[cfg(all(
    not(target_os = "linux"),
    not(target_vendor = "apple"),
    not(target_os = "windows")
))]
fn map_pid_to_lsof_open_paths(pids: &[u32]) -> HashMap<u32, ProcessOpenPaths> {
    let pid_args: Vec<String> = pids.iter().map(|p| format!("-p{}", p)).collect();
    let mut args = vec!["-F", "ftn"];
    for pa in &pid_args {
        args.push(pa);
    }

    let output = Command::new("lsof").args(&args).output().ok();
    output
        .map(|out| parse_lsof_process_info(&String::from_utf8_lossy(&out.stdout)))
        .unwrap_or_default()
}

#[cfg_attr(
    any(target_os = "linux", target_vendor = "apple", target_os = "windows"),
    allow(dead_code)
)]
pub(crate) fn parse_lsof_process_info(output: &str) -> HashMap<u32, ProcessOpenPaths> {
    let mut map: HashMap<u32, ProcessOpenPaths> = HashMap::new();
    let mut current_pid: Option<u32> = None;
    let mut current_fd = String::new();

    for line in output.lines() {
        if let Some(pid_str) = line.strip_prefix('p') {
            current_pid = pid_str.parse::<u32>().ok();
            if let Some(pid) = current_pid {
                map.entry(pid).or_default();
            }
            current_fd.clear();
        } else if let Some(fd) = line.strip_prefix('f') {
            current_fd = fd.to_string();
        } else if let Some(name) = line.strip_prefix('n') {
            let Some(pid) = current_pid else {
                continue;
            };
            if name.is_empty() || name.starts_with('[') {
                continue;
            }
            let path = PathBuf::from(name);
            let info = map.entry(pid).or_default();
            if current_fd == "cwd" {
                info.cwd = Some(path.clone());
            }
            info.paths.push(path);
        }
    }

    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_parse_lsof_process_info_captures_multiple_pids_and_cwd() {
        let output = "\
p111
fcwd
tDIR
n/Users/alice/project
f15
tDIR
n/Users/alice/.claude-work
p222
fcwd
tDIR
n/Users/bob/project
f20
tREG
n/Users/bob/.claude-alt/projects/-Users-bob-project/session.jsonl
";

        let parsed = parse_lsof_process_info(output);

        assert_eq!(parsed.len(), 2);
        assert_eq!(
            parsed.get(&111).unwrap().cwd.as_deref(),
            Some(Path::new("/Users/alice/project")),
        );
        assert!(parsed.get(&222).unwrap().paths.contains(&PathBuf::from(
            "/Users/bob/.claude-alt/projects/-Users-bob-project/session.jsonl"
        )));
    }
}
