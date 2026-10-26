use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProcInfo {
    pid: u32,
    ppid: u32,
    start: u64,
}

/// Working directory of the process a terminal (`pid`) is most likely showing: its deepest
/// descendant, the newest one on ties. Falls back up the ancestry when a cwd is unreadable,
/// for example because the process belongs to another user.
pub fn foreground_cwd(pid: u32) -> Option<String> {
    let procs = read_proc_table(Path::new("/proc"));
    descendant_chain(pid, &procs).into_iter().find_map(read_cwd)
}

fn read_cwd(pid: u32) -> Option<String> {
    let link = std::fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
    link.to_str().map(str::to_owned)
}

fn read_proc_table(proc_root: &Path) -> Vec<ProcInfo> {
    let Ok(entries) = std::fs::read_dir(proc_root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_name().to_str().is_some_and(is_pid))
        .filter_map(|e| std::fs::read_to_string(e.path().join("stat")).ok())
        .filter_map(|stat| parse_stat(&stat))
        .collect()
}

fn is_pid(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit())
}

/// `comm` (field 2) may contain spaces and parentheses, so fields are counted from the
/// last `)`. After it come state (3), ppid (4), ..., starttime (22).
fn parse_stat(stat: &str) -> Option<ProcInfo> {
    let (head, rest) = stat.rsplit_once(')')?;
    let pid = head.split_once(" (")?.0.trim().parse().ok()?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    Some(ProcInfo {
        pid,
        ppid: fields.get(1)?.parse().ok()?,
        start: fields.get(19)?.parse().ok()?,
    })
}

/// The chosen descendant first, then its ancestors, stopping before `root`.
fn descendant_chain(root: u32, procs: &[ProcInfo]) -> Vec<u32> {
    let parents: HashMap<u32, u32> = procs.iter().map(|p| (p.pid, p.ppid)).collect();
    let chosen = procs
        .iter()
        .filter_map(|p| {
            let chain = ancestry_below(root, p.pid, &parents)?;
            Some((chain.len(), p.start, p.pid, chain))
        })
        .max_by(|a, b| (a.0, a.1, a.2).cmp(&(b.0, b.1, b.2)));
    chosen.map(|(.., chain)| chain).unwrap_or_default()
}

/// `pid` and its ancestors up to but excluding `root`, or `None` when `pid` does not
/// descend from `root`.
fn ancestry_below(root: u32, pid: u32, parents: &HashMap<u32, u32>) -> Option<Vec<u32>> {
    let mut chain = Vec::new();
    let mut current = pid;
    while current != root {
        if chain.len() > parents.len() {
            return None;
        }
        chain.push(current);
        current = *parents.get(&current)?;
    }
    (!chain.is_empty()).then_some(chain)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command};
    use std::time::{Duration, Instant};

    fn proc(pid: u32, ppid: u32, start: u64) -> ProcInfo {
        ProcInfo { pid, ppid, start }
    }

    fn stat_line(pid: u32, comm: &str, ppid: u32, start: u64) -> String {
        let mut fields = vec!["S".to_string(), ppid.to_string()];
        fields.extend((5..22).map(|n| n.to_string()));
        fields.push(start.to_string());
        fields.extend(["1000".into(), "200".into()]);
        format!("{pid} ({comm}) {}\n", fields.join(" "))
    }

    #[test]
    fn parse_stat_reads_ppid_and_start_time() {
        let parsed = parse_stat(&stat_line(42, "zsh", 7, 123456)).unwrap();
        assert_eq!(parsed, proc(42, 7, 123456));
    }

    #[test]
    fn parse_stat_handles_spaces_and_parens_in_comm() {
        let parsed = parse_stat(&stat_line(42, "evil) S 1 (x", 7, 99)).unwrap();
        assert_eq!(parsed, proc(42, 7, 99));
    }

    #[test]
    fn parse_stat_rejects_garbage() {
        assert_eq!(parse_stat(""), None);
        assert_eq!(parse_stat("12 (x) S"), None);
    }

    #[test]
    fn chain_prefers_deepest_then_newest() {
        let procs = [
            proc(10, 1, 0),
            proc(11, 10, 5),
            proc(12, 10, 9),
            proc(13, 11, 6),
            proc(14, 12, 10),
            proc(15, 14, 11),
            proc(16, 13, 7),
            proc(99, 1, 50),
        ];
        assert_eq!(descendant_chain(10, &procs), [15, 14, 12]);
        let tied = [proc(10, 1, 0), proc(11, 10, 5), proc(12, 10, 9)];
        assert_eq!(descendant_chain(10, &tied), [12]);
    }

    #[test]
    fn chain_is_empty_without_descendants() {
        assert!(descendant_chain(10, &[proc(10, 1, 0), proc(11, 1, 3)]).is_empty());
    }

    struct KillOnDrop(Child);

    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn foreground_cwd_of_spawned_shell() {
        let dir = tempfile::tempdir().unwrap();
        let expected = dir.path().canonicalize().unwrap();
        let _child = KillOnDrop(
            Command::new("sh")
                .args(["-c", r#"cd "$1" && sleep 5"#, "sh"])
                .arg(dir.path())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut seen = None;
        while Instant::now() < deadline {
            seen = foreground_cwd(std::process::id());
            if seen.as_deref() == expected.to_str() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("expected {expected:?}, last saw {seen:?}");
    }
}
