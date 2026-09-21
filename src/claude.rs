use crate::process::ProcessTree;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
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
    #[serde(default)]
    pub spare: bool,
    #[serde(default)]
    pub job_id: Option<String>,
    #[serde(default)]
    pub parked_job_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
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

pub struct SessionInfo {
    pub state: SessionState,
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

        if session.spare {
            continue;
        }

        if check_alive && !tree.is_alive(session.pid) {
            continue;
        }

        sessions.push(session);
    }

    sessions
}

pub struct PanelSession<'sess> {
    pub pane_owner: &'sess ClaudeSession,
    pub displayed: &'sess ClaudeSession,
}

pub fn fold_parked_jobs(sessions: &[ClaudeSession]) -> Vec<PanelSession<'_>> {
    let by_job: HashMap<&str, &ClaudeSession> = sessions
        .iter()
        .filter_map(|session| session.job_id.as_deref().map(|id| (id, session)))
        .collect();

    let mut folded: HashSet<u32> = HashSet::new();
    let mut panels: Vec<PanelSession<'_>> = Vec::new();

    for session in sessions {
        let parked = session
            .parked_job_id
            .as_deref()
            .and_then(|id| by_job.get(id).copied())
            .filter(|job| job.pid != session.pid && !folded.contains(&job.pid));

        if let Some(job) = parked {
            let _ = folded.insert(job.pid);
        }

        panels.push(PanelSession {
            pane_owner: session,
            displayed: parked.unwrap_or(session),
        });
    }

    panels.retain(|panel| !folded.contains(&panel.pane_owner.pid));
    panels
}

pub fn detect_info(session: &ClaudeSession) -> SessionInfo {
    let Some(home) = home_dir() else {
        return SessionInfo {
            state: SessionState::from_status(session.status.as_deref()),
            recap: None,
        };
    };
    detect_info_in(session, &home)
}

pub fn detect_info_in(session: &ClaudeSession, home: &Path) -> SessionInfo {
    let recap = find_jsonl_path_in(session, home)
        .and_then(|path| read_tail_chunk(&path))
        .and_then(|tail| parse_recap(&tail));

    SessionInfo {
        state: SessionState::from_status(session.status.as_deref()),
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

    fn make_job_session(pid: u32, session_id: &str, job_id: &str) -> ClaudeSession {
        let mut session = make_session(session_id, "/home/user");
        session.pid = pid;
        session.job_id = Some(job_id.to_owned());
        session
    }

    fn make_owner_session(pid: u32, session_id: &str, parked_job_id: &str) -> ClaudeSession {
        let mut session = make_session(session_id, "/home/user");
        session.pid = pid;
        session.parked_job_id = Some(parked_job_id.to_owned());
        session
    }

    #[test]
    fn fold_parked_jobs_collapses_job_into_its_pane() {
        let sessions = vec![
            make_owner_session(1, "pane", "job-1"),
            make_job_session(2, "parked", "job-1"),
        ];

        let panels = fold_parked_jobs(&sessions);

        assert_eq!(panels.len(), 1);
        let panel = panels.first().expect("panel");
        assert_eq!(panel.pane_owner.pid, 1);
        assert_eq!(panel.displayed.pid, 2);
    }

    #[test]
    fn fold_parked_jobs_keeps_unmatched_background_session() {
        let sessions = vec![
            make_owner_session(1, "pane", "job-other"),
            make_job_session(2, "loose", "job-1"),
        ];

        let panels = fold_parked_jobs(&sessions);

        assert_eq!(panels.len(), 2);
        assert!(panels.iter().all(|p| p.pane_owner.pid == p.displayed.pid));
    }

    #[test]
    fn fold_parked_jobs_leaves_plain_sessions_alone() {
        let sessions = vec![make_session("a", "/home/user"), make_session("b", "/tmp")];

        let panels = fold_parked_jobs(&sessions);

        assert_eq!(panels.len(), 2);
        assert!(panels.iter().all(|p| p.pane_owner.pid == p.displayed.pid));
    }

    #[test]
    fn fold_parked_jobs_gives_a_job_to_a_single_owner() {
        let sessions = vec![
            make_owner_session(1, "pane-a", "job-1"),
            make_owner_session(2, "pane-b", "job-1"),
            make_job_session(3, "parked", "job-1"),
        ];

        let panels = fold_parked_jobs(&sessions);

        assert_eq!(panels.len(), 2);
        let folded = panels.iter().filter(|p| p.displayed.pid == 3).count();
        assert_eq!(folded, 1);
    }

    #[test]
    fn discover_sessions_skips_spare_sessions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let json = serde_json::json!({
            "pid": 99999,
            "sessionId": "spare-1",
            "cwd": "/home/user/project",
            "startedAt": 1700000000_u64,
            "status": "idle",
            "spare": true
        });
        std::fs::write(dir.path().join("spare.json"), json.to_string()).expect("write");
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
            spare: false,
            job_id: None,
            parked_job_id: None,
            name: None,
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
        let info = detect_info_in(&session, dir.path());
        assert!(matches!(info.state, SessionState::Active));
    }

    #[test]
    fn detect_info_state_follows_busy_status() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = make_session_with_status("sess-busy", "/home/user", Some("busy"));
        let info = detect_info_in(&session, dir.path());
        assert!(matches!(info.state, SessionState::Active));
    }

    #[test]
    fn detect_info_state_defaults_to_active_without_status() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = make_session_with_status("sess-unknown", "/home/user", None);
        let info = detect_info_in(&session, dir.path());
        assert!(matches!(info.state, SessionState::Active));
    }

    #[test]
    fn detect_info_ignores_transcript_when_deciding_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = make_session_with_status("sess-mismatch", "/home/user", Some("idle"));
        let content = r#"{"type":"user","message":{"role":"user"}}"#;
        setup_jsonl(dir.path(), "/home/user", "sess-mismatch", content);

        let info = detect_info_in(&session, dir.path());
        assert!(matches!(info.state, SessionState::Idle));
    }

    #[test]
    fn detect_info_idle_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = make_session_with_status("sess-idle", "/home/user", Some("idle"));
        let content =
            r#"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn"}}"#;
        setup_jsonl(dir.path(), "/home/user", "sess-idle", content);

        let info = detect_info_in(&session, dir.path());
        assert!(matches!(info.state, SessionState::Idle));
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

        let info = detect_info_in(&session, dir.path());
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
            spare: false,
            job_id: None,
            parked_job_id: None,
            name: None,
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
            spare: false,
            job_id: None,
            parked_job_id: None,
            name: None,
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
            spare: false,
            job_id: None,
            parked_job_id: None,
            name: None,
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
