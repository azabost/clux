use crate::tmux::PaneInfo;
use std::collections::HashMap;

#[cfg(target_os = "linux")]
pub struct ProcessTree {
    proc_available: bool,
}

#[cfg(target_os = "linux")]
impl ProcessTree {
    pub fn build() -> Self {
        Self {
            proc_available: std::path::Path::new("/proc").is_dir(),
        }
    }

    pub fn is_alive(&self, pid: u32) -> bool {
        self.proc_available && std::path::Path::new(&format!("/proc/{pid}")).exists()
    }
}

#[cfg(target_os = "linux")]
fn get_ppid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(')')?.1;
    after_comm.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(target_os = "linux")]
pub fn find_tmux_pane<'pane>(
    pid: u32,
    pane_map: &'pane HashMap<u32, PaneInfo>,
    _tree: &ProcessTree,
) -> Option<&'pane PaneInfo> {
    let mut current = pid;
    for _ in 0..20 {
        let ppid = get_ppid(current)?;
        if let Some(value) = pane_map.get(&ppid) {
            return Some(value);
        }
        if ppid <= 1 {
            return None;
        }
        current = ppid;
    }
    None
}

#[cfg(target_os = "macos")]
pub struct ProcessTree {
    parents: HashMap<u32, u32>,
}

#[cfg(target_os = "macos")]
impl ProcessTree {
    pub fn build() -> Self {
        let Ok(output) = std::process::Command::new("ps")
            .args(["-eo", "pid,ppid"])
            .output()
        else {
            return Self::from_ps_output("");
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        Self::from_ps_output(&stdout)
    }

    fn from_ps_output(output: &str) -> Self {
        Self {
            parents: parse_ps_output(output),
        }
    }

    pub fn is_alive(&self, pid: u32) -> bool {
        self.parents.contains_key(&pid)
    }
}

#[cfg(any(target_os = "macos", test))]
fn parse_ps_output(output: &str) -> HashMap<u32, u32> {
    let mut parents = HashMap::new();
    for line in output.lines().skip(1) {
        let mut fields = line.split_whitespace();
        if let (Some(pid_str), Some(ppid_str)) = (fields.next(), fields.next())
            && let (Ok(pid), Ok(ppid)) = (pid_str.parse::<u32>(), ppid_str.parse::<u32>())
        {
            let _ = parents.insert(pid, ppid);
        }
    }
    parents
}

#[cfg(target_os = "macos")]
pub fn find_tmux_pane<'pane>(
    pid: u32,
    pane_map: &'pane HashMap<u32, PaneInfo>,
    tree: &ProcessTree,
) -> Option<&'pane PaneInfo> {
    let mut current = pid;
    for _ in 0..20 {
        let ppid = tree.parents.get(&current)?;
        if let Some(value) = pane_map.get(ppid) {
            return Some(value);
        }
        if *ppid <= 1 {
            return None;
        }
        current = *ppid;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ps_output_basic() {
        let output = "  PID  PPID\n  100     1\n  200   100\n  300   100\n";
        let parents = parse_ps_output(output);
        assert_eq!(parents.get(&100), Some(&1));
        assert_eq!(parents.get(&200), Some(&100));
        assert_eq!(parents.get(&300), Some(&100));
    }

    #[test]
    fn parse_ps_output_empty() {
        assert!(parse_ps_output("").is_empty());
    }

    #[test]
    fn parse_ps_output_header_only() {
        assert!(parse_ps_output("  PID  PPID\n").is_empty());
    }

    #[test]
    fn parse_ps_output_malformed_lines() {
        let output = "  PID  PPID\n  notapid  1\n  100  also_bad\n  200  1\n";
        let parents = parse_ps_output(output);
        assert_eq!(parents.len(), 1);
        assert_eq!(parents.get(&200), Some(&1));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn find_tmux_pane_direct_parent() {
        let mut pane_map = HashMap::new();
        let current_pid = std::process::id();
        let ppid = get_ppid(current_pid).expect("should have a parent");

        let _ = pane_map.insert(
            ppid,
            PaneInfo {
                session_name: "test".to_owned(),
                target: "test:0.0".to_owned(),
                title: String::new(),
            },
        );
        let tree = ProcessTree::build();

        let result = find_tmux_pane(current_pid, &pane_map, &tree);
        assert!(result.is_some());
        assert_eq!(result.expect("pane").session_name, "test");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn find_tmux_pane_not_found() {
        let pane_map = HashMap::new();
        let tree = ProcessTree::build();
        let result = find_tmux_pane(std::process::id(), &pane_map, &tree);
        assert!(result.is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn find_tmux_pane_stops_at_init() {
        let pane_map = HashMap::new();
        let tree = ProcessTree::build();
        let result = find_tmux_pane(1, &pane_map, &tree);
        assert!(result.is_none());
    }
}
