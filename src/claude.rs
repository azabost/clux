use crate::process::ProcessTree;
use serde::Deserialize;
use std::io::{Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeSession {
    pub pid: u32,
    pub session_id: String,
    pub cwd: String,
    pub started_at: u64,
    #[serde(default)]
    pub status: Option<String>,
}

pub enum SessionState {
    Active,
    Idle,
}

impl SessionState {
    fn from_status(status: Option<&str>) -> Self {
        match status {
            Some("idle") => Self::Idle,
            _ => Self::Active,
        }
    }
}

pub enum SessionMode {
    Default,
    AcceptEdits,
    BypassPermissions,
    Plan,
}

pub struct SessionInfo {
    pub state: SessionState,
    pub mode: SessionMode,
    pub active_tasks: u32,
    pub active_agents: u32,
    pub recap: Option<String>,
}

pub fn discover_sessions(tree: &ProcessTree) -> Vec<ClaudeSession> {
    let sessions_dir = match home_dir().map(|home| home.join(".claude").join("sessions")) {
        Some(dir) if dir.is_dir() => dir,
        _ => return Vec::new(),
    };
    discover_sessions_in(&sessions_dir, true, tree)
}

pub fn discover_sessions_in(
    sessions_dir: &Path,
    check_alive: bool,
    tree: &ProcessTree,
) -> Vec<ClaudeSession> {
    let mut sessions = Vec::new();

    let Ok(entries) = std::fs::read_dir(sessions_dir) else {
        return sessions;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }

        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };

        let Ok(session) = serde_json::from_str::<ClaudeSession>(&contents) else {
            continue;
        };

        if check_alive && !tree.is_alive(session.pid) {
            continue;
        }

        sessions.push(session);
    }

    sessions
}

pub fn detect_info(session: &ClaudeSession, tree: &ProcessTree) -> SessionInfo {
    let Some(home) = home_dir() else {
        return SessionInfo {
            state: SessionState::from_status(session.status.as_deref()),
            mode: SessionMode::Default,
            active_tasks: 0,
            active_agents: 0,
            recap: None,
        };
    };
    detect_info_in(session, &home, tree)
}

pub fn detect_info_in(session: &ClaudeSession, home: &Path, tree: &ProcessTree) -> SessionInfo {
    let tail = find_jsonl_path_in(session, home).and_then(|path| read_tail_chunk(&path));
    let (mode, recap) = tail
        .as_deref()
        .map_or((SessionMode::Default, None), |contents| {
            (parse_permission_mode(contents), parse_recap(contents))
        });
    let (tasks, agents) = count_active_background(session, tree);

    SessionInfo {
        state: SessionState::from_status(session.status.as_deref()),
        mode,
        active_tasks: tasks,
        active_agents: agents,
        recap,
    }
}

pub fn parse_recap(tail: &str) -> Option<String> {
    for line in tail.lines().rev() {
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };

        let entry_type = entry.get("type").and_then(|t| t.as_str());

        if entry_type == Some("user") || entry_type == Some("assistant") {
            return None;
        }

        if entry_type == Some("system")
            && entry.get("subtype").and_then(|s| s.as_str()) == Some("away_summary")
        {
            let content = entry.get("content").and_then(|c| c.as_str())?;
            return Some(strip_recap_hint(content));
        }
    }

    None
}

fn strip_recap_hint(content: &str) -> String {
    content
        .trim()
        .strip_suffix("(disable recaps in /config)")
        .unwrap_or(content)
        .trim()
        .to_owned()
}

pub fn parse_permission_mode(tail: &str) -> SessionMode {
    for line in tail.lines().rev() {
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };

        if let Some(permission_mode) = entry.get("permissionMode").and_then(|m| m.as_str()) {
            return match permission_mode {
                "plan" => SessionMode::Plan,
                "bypassPermissions" => SessionMode::BypassPermissions,
                "acceptEdits" => SessionMode::AcceptEdits,
                _ => SessionMode::Default,
            };
        }
    }

    SessionMode::Default
}

fn count_active_background(session: &ClaudeSession, tree: &ProcessTree) -> (u32, u32) {
    let uid = current_uid();
    let encoded_cwd = encode_cwd(&session.cwd);
    let tasks_dir = PathBuf::from(format!(
        "/tmp/claude-{uid}/{encoded_cwd}/{}/tasks",
        session.session_id
    ));

    let Ok(entries) = std::fs::read_dir(&tasks_dir) else {
        return (0, 0);
    };

    let open_files = collect_open_files(session.pid, tree);

    let mut tasks = 0;
    let mut agents = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("output") {
            continue;
        }
        let is_active = path
            .canonicalize()
            .ok()
            .is_some_and(|canonical| open_files.contains(&canonical));
        if !is_active {
            continue;
        }
        if path.is_symlink() {
            agents += 1;
        } else {
            tasks += 1;
        }
    }
    (tasks, agents)
}

#[cfg(target_os = "linux")]
fn collect_open_files(parent_pid: u32, tree: &ProcessTree) -> std::collections::HashSet<PathBuf> {
    let mut files = std::collections::HashSet::new();
    let child_pids = tree.descendants_of(parent_pid);
    for pid in child_pids {
        let fd_dir = format!("/proc/{pid}/fd");
        if let Ok(fds) = std::fs::read_dir(&fd_dir) {
            for fd in fds.flatten() {
                if let Ok(target) = std::fs::read_link(fd.path()) {
                    let _ = files.insert(target);
                }
            }
        }
    }
    files
}

#[cfg(target_os = "macos")]
fn collect_open_files(parent_pid: u32, tree: &ProcessTree) -> std::collections::HashSet<PathBuf> {
    let child_pids = tree.descendants_of(parent_pid);
    let mut files = std::collections::HashSet::new();
    if child_pids.is_empty() {
        return files;
    }
    let pid_arg: String = child_pids
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    if let Ok(lsof) = std::process::Command::new("lsof")
        .args(["-p", &pid_arg, "-Fn"])
        .output()
    {
        for line in String::from_utf8_lossy(&lsof.stdout).lines() {
            if let Some(path) = line.strip_prefix('n') {
                let _ = files.insert(PathBuf::from(path));
            }
        }
    }
    files
}

pub fn find_jsonl_path_in(session: &ClaudeSession, home: &Path) -> Option<PathBuf> {
    let projects_dir = home.join(".claude").join("projects");
    let file_name = format!("{}.jsonl", session.session_id);

    let by_cwd = projects_dir.join(encode_cwd(&session.cwd)).join(&file_name);
    if by_cwd.exists() {
        return Some(by_cwd);
    }

    find_jsonl_by_session_id(&projects_dir, &file_name)
}

fn find_jsonl_by_session_id(projects_dir: &Path, file_name: &str) -> Option<PathBuf> {
    std::fs::read_dir(projects_dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path().join(file_name))
        .find(|candidate| candidate.is_file())
}

pub fn encode_cwd(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

pub fn read_tail_chunk(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let file_len = file.metadata().ok()?.len();
    if file_len == 0 {
        return None;
    }

    let chunk_size: u64 = 64 * 1024;
    let start = file_len.saturating_sub(chunk_size);
    let _ = file.seek(SeekFrom::Start(start)).ok()?;

    let mut buf = std::io::read_to_string(&mut file).ok()?;

    if start > 0
        && let Some(newline_pos) = buf.find('\n')
    {
        let _ = buf.drain(..=newline_pos);
    }

    Some(buf)
}

#[cfg(target_os = "linux")]
fn current_uid() -> u32 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|content| {
            content
                .lines()
                .find(|line| line.starts_with("Uid:"))
                .and_then(|line| line.split_whitespace().nth(1))
                .and_then(|uid| uid.parse().ok())
        })
        .unwrap_or(1000)
}

#[cfg(target_os = "macos")]
fn current_uid() -> u32 {
    static UID: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *UID.get_or_init(|| {
        std::process::Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(501)
    })
}

fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_cwd_basic() {
        assert_eq!(encode_cwd("/home/user/project"), "-home-user-project");
    }

    #[test]
    fn encode_cwd_root() {
        assert_eq!(encode_cwd("/"), "-");
    }

    #[test]
    fn encode_cwd_trailing_slash() {
        assert_eq!(encode_cwd("/home/user/"), "-home-user-");
    }

    #[test]
    fn encode_cwd_dotted_path() {
        assert_eq!(
            encode_cwd("/home/user/.config/nvim"),
            "-home-user--config-nvim"
        );
    }

    #[test]
    fn encode_cwd_worktree_with_plus() {
        assert_eq!(
            encode_cwd("/home/user/repo/.claude/worktrees/task+ABC-123"),
            "-home-user-repo--claude-worktrees-task-ABC-123"
        );
    }

    #[test]
    fn encode_cwd_underscore_and_space() {
        assert_eq!(
            encode_cwd("/home/user/my_project dir"),
            "-home-user-my-project-dir"
        );
    }

    #[test]
    fn discover_sessions_empty_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tree = ProcessTree::build();
        let sessions = discover_sessions_in(dir.path(), false, &tree);
        assert!(sessions.is_empty());
    }

    #[test]
    fn discover_sessions_reads_status_field() {
        let dir = tempfile::tempdir().expect("tempdir");
        let json = serde_json::json!({
            "pid": 99999,
            "sessionId": "abc-123",
            "cwd": "/home/user/project",
            "startedAt": 1700000000_u64,
            "status": "idle"
        });
        std::fs::write(dir.path().join("sess.json"), json.to_string()).expect("write");
        let tree = ProcessTree::build();
        let sessions = discover_sessions_in(dir.path(), false, &tree);
        assert_eq!(
            sessions.first().expect("session").status.as_deref(),
            Some("idle")
        );
    }

    #[test]
    fn discover_sessions_invalid_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("bad.json"), "not json").expect("write");
        let tree = ProcessTree::build();
        let sessions = discover_sessions_in(dir.path(), false, &tree);
        assert!(sessions.is_empty());
    }

    #[test]
    fn discover_sessions_non_json_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("notes.txt"), "hello").expect("write");
        std::fs::write(dir.path().join("data.log"), "log line").expect("write");
        let tree = ProcessTree::build();
        let sessions = discover_sessions_in(dir.path(), false, &tree);
        assert!(sessions.is_empty());
    }

    #[test]
    fn discover_sessions_valid_no_pid_check() {
        let dir = tempfile::tempdir().expect("tempdir");
        let json = serde_json::json!({
            "pid": 99999,
            "sessionId": "abc-123",
            "cwd": "/home/user/project",
            "startedAt": 1700000000_u64
        });
        std::fs::write(dir.path().join("sess.json"), json.to_string()).expect("write");
        let tree = ProcessTree::build();
        let sessions = discover_sessions_in(dir.path(), false, &tree);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions.first().expect("session").session_id, "abc-123");
        assert_eq!(sessions.first().expect("session").cwd, "/home/user/project");
    }

    #[test]
    fn discover_sessions_skips_dead_pids() {
        let dir = tempfile::tempdir().expect("tempdir");
        let json = serde_json::json!({
            "pid": 4294967295_u64,
            "sessionId": "dead-session",
            "cwd": "/tmp",
            "startedAt": 1700000000_u64
        });
        std::fs::write(dir.path().join("dead.json"), json.to_string()).expect("write");
        let tree = ProcessTree::build();
        let sessions = discover_sessions_in(dir.path(), true, &tree);
        assert!(sessions.is_empty());
    }

    #[test]
    fn read_tail_chunk_small_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("small.jsonl");
        std::fs::write(&path, "line1\nline2\nline3\n").expect("write");
        let result = read_tail_chunk(&path);
        assert!(result.is_some());
        let content = result.expect("content");
        assert!(content.contains("line1"));
        assert!(content.contains("line3"));
    }

    #[test]
    fn read_tail_chunk_empty_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("empty.jsonl");
        std::fs::write(&path, "").expect("write");
        assert!(read_tail_chunk(&path).is_none());
    }

    #[test]
    fn read_tail_chunk_large_file_drops_partial_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("large.jsonl");
        let line = "a".repeat(1000);
        let mut content = String::new();
        for _ in 0..100 {
            content.push_str(&line);
            content.push('\n');
        }
        std::fs::write(&path, &content).expect("write");
        let result = read_tail_chunk(&path);
        assert!(result.is_some());
        let tail = result.expect("tail");
        for l in tail.lines() {
            assert!(l.len() == 1000 || l.is_empty());
        }
    }

    #[test]
    fn parse_permission_mode_empty() {
        assert!(matches!(parse_permission_mode(""), SessionMode::Default));
    }

    #[test]
    fn parse_permission_mode_plan() {
        let tail = r#"{"permissionMode":"plan"}
{"type":"user","message":{"role":"user"}}"#;
        let mode = parse_permission_mode(tail);
        assert!(matches!(mode, SessionMode::Plan));
    }

    #[test]
    fn parse_permission_mode_bypass() {
        let tail = r#"{"permissionMode":"bypassPermissions"}
{"type":"user","message":{"role":"user"}}"#;
        let mode = parse_permission_mode(tail);
        assert!(matches!(mode, SessionMode::BypassPermissions));
    }

    #[test]
    fn parse_permission_mode_accept_edits() {
        let tail = r#"{"permissionMode":"acceptEdits"}
{"type":"user","message":{"role":"user"}}"#;
        let mode = parse_permission_mode(tail);
        assert!(matches!(mode, SessionMode::AcceptEdits));
    }

    #[test]
    fn parse_permission_mode_default() {
        let tail = r#"{"permissionMode":"default"}
{"type":"user","message":{"role":"user"}}"#;
        let mode = parse_permission_mode(tail);
        assert!(matches!(mode, SessionMode::Default));
    }

    #[test]
    fn parse_permission_mode_unknown() {
        let tail = r#"{"permissionMode":"somethingNew"}
{"type":"user","message":{"role":"user"}}"#;
        let mode = parse_permission_mode(tail);
        assert!(matches!(mode, SessionMode::Default));
    }

    fn make_session(session_id: &str, cwd: &str) -> ClaudeSession {
        make_session_with_status(session_id, cwd, None)
    }

    fn make_session_with_status(
        session_id: &str,
        cwd: &str,
        status: Option<&str>,
    ) -> ClaudeSession {
        ClaudeSession {
            pid: 1,
            session_id: session_id.to_owned(),
            cwd: cwd.to_owned(),
            started_at: 0,
            status: status.map(str::to_owned),
        }
    }

    fn setup_jsonl(home: &Path, cwd: &str, session_id: &str, content: &str) {
        let encoded = encode_cwd(cwd);
        let project_dir = home.join(".claude").join("projects").join(&encoded);
        std::fs::create_dir_all(&project_dir).expect("mkdir");
        std::fs::write(project_dir.join(format!("{session_id}.jsonl")), content).expect("write");
    }

    #[test]
    fn detect_info_no_jsonl_returns_active() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = make_session("no-file", "/home/user");
        let tree = ProcessTree::build();
        let info = detect_info_in(&session, dir.path(), &tree);
        assert!(matches!(info.state, SessionState::Active));
        assert!(matches!(info.mode, SessionMode::Default));
        assert_eq!(info.active_tasks, 0);
        assert_eq!(info.active_agents, 0);
    }

    #[test]
    fn detect_info_state_follows_busy_status() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = make_session_with_status("sess-busy", "/home/user", Some("busy"));
        let tree = ProcessTree::build();
        let info = detect_info_in(&session, dir.path(), &tree);
        assert!(matches!(info.state, SessionState::Active));
    }

    #[test]
    fn detect_info_state_defaults_to_active_without_status() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = make_session_with_status("sess-unknown", "/home/user", None);
        let tree = ProcessTree::build();
        let info = detect_info_in(&session, dir.path(), &tree);
        assert!(matches!(info.state, SessionState::Active));
    }

    #[test]
    fn detect_info_ignores_transcript_when_deciding_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = make_session_with_status("sess-mismatch", "/home/user", Some("idle"));
        let content = r#"{"type":"user","message":{"role":"user"}}"#;
        setup_jsonl(dir.path(), "/home/user", "sess-mismatch", content);

        let tree = ProcessTree::build();
        let info = detect_info_in(&session, dir.path(), &tree);
        assert!(matches!(info.state, SessionState::Idle));
    }

    #[test]
    fn detect_info_reads_mode_without_status() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = make_session_with_status("sess-mode-only", "/home/user", None);
        setup_jsonl(
            dir.path(),
            "/home/user",
            "sess-mode-only",
            r#"{"permissionMode":"plan"}"#,
        );

        let tree = ProcessTree::build();
        let info = detect_info_in(&session, dir.path(), &tree);
        assert!(matches!(info.mode, SessionMode::Plan));
    }

    #[test]
    fn detect_info_idle_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = make_session_with_status("sess-idle", "/home/user", Some("idle"));
        let content =
            r#"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn"}}"#;
        setup_jsonl(dir.path(), "/home/user", "sess-idle", content);

        let tree = ProcessTree::build();
        let info = detect_info_in(&session, dir.path(), &tree);
        assert!(matches!(info.state, SessionState::Idle));
    }

    #[test]
    fn detect_info_plan_mode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = make_session("sess-plan", "/home/user");
        let content =
            "{\"permissionMode\":\"plan\"}\n{\"type\":\"user\",\"message\":{\"role\":\"user\"}}";
        setup_jsonl(dir.path(), "/home/user", "sess-plan", content);

        let tree = ProcessTree::build();
        let info = detect_info_in(&session, dir.path(), &tree);
        assert!(matches!(info.mode, SessionMode::Plan));
    }

    #[test]
    fn detect_info_bypass_mode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = make_session_with_status("sess-yolo", "/home/user", Some("idle"));
        let content = "{\"permissionMode\":\"bypassPermissions\"}\n{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"stop_reason\":\"end_turn\"}}";
        setup_jsonl(dir.path(), "/home/user", "sess-yolo", content);

        let tree = ProcessTree::build();
        let info = detect_info_in(&session, dir.path(), &tree);
        assert!(matches!(info.mode, SessionMode::BypassPermissions));
        assert!(matches!(info.state, SessionState::Idle));
    }

    #[test]
    fn detect_info_accept_edits_mode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = make_session("sess-edits", "/home/user");
        let content = "{\"permissionMode\":\"acceptEdits\"}\n{\"type\":\"user\",\"message\":{\"role\":\"user\"}}";
        setup_jsonl(dir.path(), "/home/user", "sess-edits", content);

        let tree = ProcessTree::build();
        let info = detect_info_in(&session, dir.path(), &tree);
        assert!(matches!(info.mode, SessionMode::AcceptEdits));
    }

    #[test]
    fn parse_permission_mode_only_garbage() {
        let tail = "garbage line 1\n{broken\nmore garbage";
        assert!(matches!(parse_permission_mode(tail), SessionMode::Default));
    }

    #[test]
    fn parse_permission_mode_scans_past_conversation_entries() {
        let tail = "{\"permissionMode\":\"plan\"}\n{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"stop_reason\":\"end_turn\"}}";
        assert!(matches!(parse_permission_mode(tail), SessionMode::Plan));
    }

    #[test]
    fn discover_sessions_multiple_valid() {
        let dir = tempfile::tempdir().expect("tempdir");
        for i in 1..=3 {
            let json = serde_json::json!({
                "pid": 99990 + i,
                "sessionId": format!("sess-{i}"),
                "cwd": "/home/user/project",
                "startedAt": 1700000000_u64
            });
            std::fs::write(dir.path().join(format!("sess-{i}.json")), json.to_string())
                .expect("write");
        }
        let tree = ProcessTree::build();
        let sessions = discover_sessions_in(dir.path(), false, &tree);
        assert_eq!(sessions.len(), 3);
    }

    #[test]
    fn detect_info_empty_jsonl_returns_active() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = make_session("sess-empty", "/home/user");
        setup_jsonl(dir.path(), "/home/user", "sess-empty", "");

        let tree = ProcessTree::build();
        let info = detect_info_in(&session, dir.path(), &tree);
        assert!(matches!(info.state, SessionState::Active));
    }

    #[test]
    fn find_jsonl_path_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let projects_dir = dir
            .path()
            .join(".claude")
            .join("projects")
            .join("-home-user");
        std::fs::create_dir_all(&projects_dir).expect("mkdir");
        std::fs::write(projects_dir.join("sess-1.jsonl"), "data").expect("write");

        let session = ClaudeSession {
            pid: 1,
            session_id: "sess-1".to_owned(),
            cwd: "/home/user".to_owned(),
            started_at: 0,
            status: None,
        };

        let result = find_jsonl_path_in(&session, dir.path());
        assert!(result.is_some());
    }

    #[test]
    fn find_jsonl_path_falls_back_to_session_id_after_cwd_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project_dir = dir
            .path()
            .join(".claude")
            .join("projects")
            .join("-home-user-where-the-session-started");
        std::fs::create_dir_all(&project_dir).expect("create project dir");
        std::fs::write(project_dir.join("sess-moved.jsonl"), "").expect("write");

        let session = ClaudeSession {
            pid: 1,
            session_id: "sess-moved".to_owned(),
            cwd: "/home/user/somewhere/else".to_owned(),
            started_at: 0,
            status: None,
        };

        let result = find_jsonl_path_in(&session, dir.path());
        assert_eq!(result, Some(project_dir.join("sess-moved.jsonl")));
    }

    #[test]
    fn find_jsonl_path_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = ClaudeSession {
            pid: 1,
            session_id: "nonaction".to_owned(),
            cwd: "/home/user".to_owned(),
            started_at: 0,
            status: None,
        };
        let result = find_jsonl_path_in(&session, dir.path());
        assert!(result.is_none());
    }

    #[test]
    fn parse_recap_picks_last_away_summary() {
        let tail = r#"{"type":"system","subtype":"away_summary","content":"Old recap (disable recaps in /config)"}
{"type":"system","subtype":"away_summary","content":"Latest recap here (disable recaps in /config)"}"#;
        assert_eq!(parse_recap(tail), Some("Latest recap here".to_owned()));
    }

    #[test]
    fn parse_recap_skipped_when_conversation_continued() {
        let tail = r#"{"type":"system","subtype":"away_summary","content":"Stale recap (disable recaps in /config)"}
{"type":"user","message":{"role":"user"}}"#;
        assert_eq!(parse_recap(tail), None);
    }

    #[test]
    fn parse_recap_strips_disable_hint() {
        let content = "Did stuff. Next: things. (disable recaps in /config)";
        let json = serde_json::json!({
            "type": "system",
            "subtype": "away_summary",
            "content": content
        })
        .to_string();
        assert_eq!(
            parse_recap(&json),
            Some("Did stuff. Next: things.".to_owned())
        );
    }

    #[test]
    fn parse_recap_none_without_away_summary() {
        let tail = r#"{"type":"system","subtype":"turn_duration"}
{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn"}}"#;
        assert_eq!(parse_recap(tail), None);
    }

    #[test]
    fn parse_recap_none_on_empty() {
        assert_eq!(parse_recap(""), None);
    }
}
