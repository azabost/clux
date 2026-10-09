pub(crate) mod claude;
pub(crate) mod history;
pub(crate) mod process;
pub(crate) mod recent;
pub(crate) mod tmux;

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

struct SessionCounts {
    active: u32,
    idle: u32,
}

struct ListEntry {
    target: String,
    session_id: String,
    state: &'static str,
    claude_session: String,
    summary: String,
    cwd: String,
    session_name: String,
    timestamp: u64,
}

#[derive(Clone, Copy, Default)]
pub enum SortOrder {
    #[default]
    Recent,
    TimestampDesc,
    TimestampAsc,
    Status,
    StatusRev,
}

impl SortOrder {
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s {
            "timestamp-desc" => Self::TimestampDesc,
            "timestamp-asc" => Self::TimestampAsc,
            "status" => Self::Status,
            "status-rev" => Self::StatusRev,
            _ => Self::Recent,
        }
    }

    const fn tiebreak_timestamp(self) -> Ordering {
        match self {
            Self::StatusRev => Ordering::Less,
            Self::Recent | Self::TimestampDesc | Self::TimestampAsc | Self::Status => {
                Ordering::Greater
            }
        }
    }
}

const DEFAULT_FORMAT: &str = " | \u{1F916} {total} ({detail})";

fn format_info(format_str: &str, counts: &SessionCounts) -> String {
    let total = counts.active + counts.idle;
    let detail = match (counts.active, counts.idle) {
        (active, 0) => format!("{active} active"),
        (0, idle) => format!("{idle} idle"),
        (active, idle) => format!("{active} active, {idle} idle"),
    };

    format_str
        .replace("{total}", &total.to_string())
        .replace("{active}", &counts.active.to_string())
        .replace("{idle}", &counts.idle.to_string())
        .replace("{detail}", &detail)
}

fn is_visible(filter: &str, counts: Option<&SessionCounts>) -> bool {
    match filter {
        "has-claude" => counts.is_some(),
        "active" => counts.is_some_and(|ct| ct.active > 0),
        "idle" => counts.is_some_and(|ct| ct.idle > 0),
        _ => true,
    }
}

fn shorten_cwd(cwd: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if cwd.starts_with(&home) => {
            let mut short = String::from("~");
            short.push_str(cwd.get(home.len()..).unwrap_or_default());
            short
        }
        _ => cwd.to_owned(),
    }
}

fn claude_session_name(session: &claude::ClaudeSession) -> String {
    session
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("(unnamed)")
        .to_owned()
}

fn truncate_at(text: &str, max_chars: usize) -> String {
    let char_count = text.chars().count();
    if char_count <= max_chars {
        return text.to_owned();
    }
    let truncated: String = text.chars().take(max_chars.saturating_sub(3)).collect();
    format!("{truncated}...")
}

fn command_exists(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(name).is_file()))
}

fn sort_entries(entries: &mut [ListEntry], order: SortOrder) {
    entries.sort_by(|a, b| {
        let primary = match order {
            SortOrder::Recent | SortOrder::TimestampDesc => b.timestamp.cmp(&a.timestamp),
            SortOrder::TimestampAsc => a.timestamp.cmp(&b.timestamp),
            SortOrder::Status => b.state.cmp(a.state),
            SortOrder::StatusRev => a.state.cmp(b.state),
        };
        if primary != Ordering::Equal {
            return primary;
        }
        match order.tiebreak_timestamp() {
            Ordering::Less => a.timestamp.cmp(&b.timestamp),
            Ordering::Equal | Ordering::Greater => b.timestamp.cmp(&a.timestamp),
        }
    });
}

fn sort_entries_recent(
    entries: &mut [ListEntry],
    recent_entries: &[recent::RecentEntry],
    current_target: Option<&str>,
) {
    let recent_map: HashMap<&str, u64> = recent_entries
        .iter()
        .map(|re| (re.session_id.as_str(), re.switched_at))
        .collect();

    entries.sort_by(|a, b| {
        let a_current = current_target.is_some_and(|ct| ct == a.target);
        let b_current = current_target.is_some_and(|ct| ct == b.target);

        if a_current != b_current {
            return a_current.cmp(&b_current);
        }

        let a_recent = recent_map.get(a.session_id.as_str());
        let b_recent = recent_map.get(b.session_id.as_str());

        match (a_recent, b_recent) {
            (Some(a_ts), Some(b_ts)) => b_ts.cmp(a_ts),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => b.timestamp.cmp(&a.timestamp),
        }
    });
}

fn pane_title_matches(title: &str, session: &claude::ClaudeSession) -> bool {
    let Some(name) = session
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    else {
        return false;
    };

    let stripped = title
        .trim_start_matches(|c: char| !c.is_alphanumeric())
        .trim();

    !stripped.is_empty() && (stripped == name || name.starts_with(stripped))
}

struct PaneRow<'panel, 'pane> {
    panel: &'panel claude::PanelSession<'panel>,
    pane: &'pane tmux::PaneInfo,
    hidden: Vec<&'panel claude::PanelSession<'panel>>,
}

fn panel_shown_in_pane(panel: &claude::PanelSession, pane: &tmux::PaneInfo) -> bool {
    pane_title_matches(&pane.title, panel.displayed)
        || pane_title_matches(&pane.title, panel.pane_owner)
}

fn assign_panes<'panel, 'pane>(
    panels: &'panel [claude::PanelSession<'panel>],
    pane_map: &'pane HashMap<u32, tmux::PaneInfo>,
    tree: &process::ProcessTree,
) -> Vec<PaneRow<'panel, 'pane>> {
    let mut resolved: Vec<(&claude::PanelSession<'_>, &tmux::PaneInfo)> = Vec::new();
    let mut pending: Vec<&claude::PanelSession<'_>> = Vec::new();
    let mut claimed: HashSet<&str> = HashSet::new();

    for panel in panels {
        if let Some(pane) = process::find_tmux_pane(panel.pane_owner.pid, pane_map, tree) {
            let _ = claimed.insert(pane.target.as_str());
            resolved.push((panel, pane));
        } else {
            pending.push(panel);
        }
    }

    for panel in pending {
        let mut candidates = pane_map.values().filter(|pane| {
            !claimed.contains(pane.target.as_str()) && panel_shown_in_pane(panel, pane)
        });

        if let Some(pane) = candidates.next()
            && candidates.next().is_none()
        {
            let _ = claimed.insert(pane.target.as_str());
            resolved.push((panel, pane));
        }
    }

    collapse_to_one_row_per_pane(resolved)
}

fn collapse_to_one_row_per_pane<'panel, 'pane>(
    resolved: Vec<(&'panel claude::PanelSession<'panel>, &'pane tmux::PaneInfo)>,
) -> Vec<PaneRow<'panel, 'pane>> {
    let mut rows: Vec<PaneRow<'panel, 'pane>> = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();

    for (panel, pane) in resolved {
        let Some(row) = index
            .get(pane.target.as_str())
            .and_then(|position| rows.get_mut(*position))
        else {
            let _ = index.insert(pane.target.as_str(), rows.len());
            rows.push(PaneRow {
                panel,
                pane,
                hidden: Vec::new(),
            });
            continue;
        };

        if panel_shown_in_pane(panel, pane) && !panel_shown_in_pane(row.panel, pane) {
            row.hidden.push(row.panel);
            row.panel = panel;
        } else {
            row.hidden.push(panel);
        }
    }

    rows
}

fn pane_row_info(row: &PaneRow) -> claude::SessionInfo {
    let mut info = claude::detect_info(row.panel.displayed);
    let mut counted: HashSet<u32> = HashSet::new();
    let _ = counted.insert(row.panel.displayed.pid);

    let behind = std::iter::once(row.panel)
        .chain(row.hidden.iter().copied())
        .flat_map(|panel| [panel.pane_owner, panel.displayed]);

    for session in behind {
        if !counted.insert(session.pid) {
            continue;
        }
        if matches!(
            claude::detect_info(session).state,
            claude::SessionState::Active
        ) {
            info.state = claude::SessionState::Active;
        }
    }

    info
}

fn gather_list_entries(order: SortOrder) -> anyhow::Result<Vec<ListEntry>> {
    let proc_tree = process::ProcessTree::build();
    let sessions = claude::discover_sessions(&proc_tree);
    let pane_map = tmux::list_pane_targets()?;
    let summaries = history::load_summaries(&sessions);
    let recaps_enabled = tmux::get_global_option("@clux-recaps")?.as_deref() != Some("off");

    let panels = claude::fold_parked_jobs(&sessions);
    let with_panes = assign_panes(&panels, &pane_map, &proc_tree);

    let mut entries: Vec<ListEntry> = with_panes
        .iter()
        .map(|row| {
            let (session, pane) = (row.panel.displayed, row.pane);
            let info = pane_row_info(row);
            let state_str = match info.state {
                claude::SessionState::Active => "active",
                claude::SessionState::Idle => "idle",
            };
            let summary_text = summaries
                .get(&session.pid)
                .map_or("(no summary)", |smry| smry.display.as_str());
            let timestamp = summaries
                .get(&session.pid)
                .map_or(session.started_at, |smry| smry.timestamp);

            let display_summary = info
                .recap
                .as_deref()
                .filter(|_| recaps_enabled)
                .unwrap_or(summary_text);

            ListEntry {
                target: pane.target.clone(),
                session_id: session.session_id.clone(),
                state: state_str,
                claude_session: claude_session_name(session),
                summary: display_summary.to_owned(),
                cwd: shorten_cwd(&session.cwd),
                session_name: pane.session_name.clone(),
                timestamp,
            }
        })
        .collect();

    if matches!(order, SortOrder::Recent) {
        let recent_data = recent::load();
        let current = tmux::current_pane_target()?;
        sort_entries_recent(&mut entries, &recent_data, current.as_deref());
    } else {
        sort_entries(&mut entries, order);
    }

    Ok(entries)
}

fn resolve_sort_order(cli_sort: Option<&str>) -> anyhow::Result<SortOrder> {
    if let Some(s) = cli_sort {
        return Ok(SortOrder::parse(s));
    }
    let tmux_sort = tmux::get_global_option("@clux-sort")?;
    Ok(tmux_sort
        .as_deref()
        .map_or_else(SortOrder::default, SortOrder::parse))
}

/// # Errors
/// Returns an error if tmux is not running or session discovery fails.
pub fn run_list(sort: Option<&str>) -> anyhow::Result<()> {
    let order = resolve_sort_order(sort)?;
    let entries = gather_list_entries(order)?;
    for entry in &entries {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            entry.target,
            entry.state,
            entry.claude_session,
            entry.summary,
            entry.cwd,
            entry.session_name
        );
    }
    Ok(())
}

/// # Errors
/// Returns an error if tmux is not running or session options cannot be set.
pub fn run_update(filter: &str) -> anyhow::Result<()> {
    let proc_tree = process::ProcessTree::build();
    let sessions = claude::discover_sessions(&proc_tree);
    let pane_map = tmux::list_pane_targets()?;
    let all_tmux_sessions = tmux::list_sessions()?;
    let custom_format = tmux::get_global_option("@clux-format")?;
    let format_str = custom_format.as_deref().unwrap_or(DEFAULT_FORMAT);

    let mut counts: HashMap<String, SessionCounts> = HashMap::new();

    let panels = claude::fold_parked_jobs(&sessions);

    for row in assign_panes(&panels, &pane_map, &proc_tree) {
        let info = pane_row_info(&row);
        let entry = counts
            .entry(row.pane.session_name.clone())
            .or_insert(SessionCounts { active: 0, idle: 0 });
        match info.state {
            claude::SessionState::Active => entry.active += 1,
            claude::SessionState::Idle => entry.idle += 1,
        }
    }

    for tmux_session in &all_tmux_sessions {
        let session_counts = counts.get(tmux_session);
        let visible = is_visible(filter, session_counts);
        tmux::set_session_option(
            tmux_session,
            "@clux_visible",
            if visible { "1" } else { "0" },
        )?;

        match session_counts {
            Some(ct) => {
                let info = format_info(format_str, ct);
                tmux::set_session_option(tmux_session, "@clux_info", &info)?;
            }
            None => {
                tmux::unset_session_option(tmux_session, "@clux_info")?;
            }
        }
    }

    Ok(())
}

/// # Errors
/// Returns an error if tmux is not running or the choose-tree UI fails to open.
pub fn run_select(filter: &str) -> anyhow::Result<()> {
    run_update(filter)?;
    tmux::choose_tree(filter)?;
    Ok(())
}

/// # Errors
/// Returns an error if tmux is not running or the picker UI fails to open.
pub fn run_pick(sort: Option<&str>) -> anyhow::Result<()> {
    let order = resolve_sort_order(sort)?;
    let entries = gather_list_entries(order)?;
    if entries.is_empty() {
        tmux::display_message("clux: no Claude sessions found")?;
        return Ok(());
    }

    let fzf_option = tmux::get_global_option("@clux-fzf")?;
    let fzf_disabled = fzf_option.as_deref() == Some("off");

    if !fzf_disabled && command_exists("fzf-tmux") {
        pick_with_fzf(&entries)?;
    } else {
        pick_with_menu(&entries)?;
    }

    Ok(())
}

/// Column widths for the picker, fitted to the popup.
///
/// The bounded columns are sized to their longest value so a session name, a
/// working directory and a tmux session are shown whole -- truncating them is
/// what makes rows sharing a prefix indistinguishable, and fzf only matches
/// what it displays, so a truncated tail cannot be searched for either. The
/// summary, which has no natural bound, takes whatever is left.
struct Columns {
    /// Shared by the Claude session and the tmux session, so the two name
    /// columns line up and neither is cut to fit the other.
    name: usize,
    summary: usize,
    cwd: usize,
}

const COL_STATE: usize = 7;
const COL_GAP: usize = 2;
const MIN_SUMMARY: usize = 20;
const MIN_NAME: usize = 12;
const MIN_CWD: usize = 12;
const MAX_NAME: usize = 40;
const MAX_CWD: usize = 70;
const HEADER_NAME: &str = "CLAUDE SESSION";
const HEADER_CWD: &str = "CWD";
/// Columns of the popup fzf does not give to the line. On tmux 3.3 and later
/// fzf-tmux opens the popup without a tmux border and has fzf draw its own,
/// which takes one column on each side plus one of padding inside each, with
/// fzf's scrollbar on the border itself; fzf's pointer and marker take two
/// more on the left. A line that does not fit is cut with an ellipsis.
const POPUP_CHROME: usize = 6;
/// The layout `POPUP_CHROME` is measured against, passed on fzf's command
/// line, where it wins over `FZF_DEFAULT_OPTS`.
const FZF_LAYOUT: &[&str] = &[
    "--border=rounded",
    "--margin=0",
    "--padding=0",
    "--pointer=▌",
    "--marker=┃",
];
/// The same for the section borders fzf 0.58 added, which `--style=full` turns
/// on. A border around the input also moves the scrollbar off fzf's border
/// and into the line. An older fzf rejects these as unknown options and exits
/// without showing the picker.
const FZF_SECTION_LAYOUT: &[&str] = &[
    "--list-border=none",
    "--input-border=none",
    "--header-border=none",
];
const FZF_SECTION_BORDERS_SINCE: (u32, u32) = (0, 58);

fn longest<'entry>(
    entries: &'entry [ListEntry],
    field: impl Fn(&'entry ListEntry) -> &'entry str,
) -> usize {
    entries
        .iter()
        .map(|entry| field(entry).chars().count())
        .max()
        .unwrap_or(0)
}

fn fit_columns(entries: &[ListEntry], line_width: usize) -> Columns {
    // A column narrower than its header pushes every header to its right out
    // of line with the rows below it.
    let mut name = longest(entries, |entry| &entry.claude_session)
        .max(longest(entries, |entry| &entry.session_name))
        .max(HEADER_NAME.len())
        .min(MAX_NAME);
    let mut cwd = longest(entries, |entry| &entry.cwd)
        .max(HEADER_CWD.len())
        .min(MAX_CWD);

    // What the row needs beyond the line once the summary is down to its
    // floor. Widening alone would push the last column off a narrow screen, so
    // claw the excess back: from the working directory first, since its rows
    // share a long prefix, then from the names, which carry two columns each.
    let overflow = |names: usize, dir: usize| {
        (COL_STATE + names + dir + names + COL_GAP * 4 + MIN_SUMMARY).saturating_sub(line_width)
    };

    let over_cwd = overflow(name, cwd);
    if over_cwd > 0 {
        cwd -= over_cwd.min(cwd.saturating_sub(MIN_CWD));
    }

    let over_names = overflow(name, cwd);
    if over_names > 0 {
        name -= over_names.div_ceil(2).min(name.saturating_sub(MIN_NAME));
    }

    let fixed = COL_STATE + name + cwd + name + COL_GAP * 4;
    let summary = line_width.saturating_sub(fixed).max(MIN_SUMMARY);

    Columns { name, summary, cwd }
}

fn popup_width() -> usize {
    const FALLBACK: usize = 120;
    const BORDER: usize = 4;
    const PERCENT: usize = 80;

    tmux::client_width()
        .ok()
        .flatten()
        .map_or(FALLBACK, |width| {
            (width * PERCENT / 100).saturating_sub(BORDER)
        })
}

/// The major and minor version from `fzf --version`, e.g. `0.74.4 (Homebrew)`.
fn parse_fzf_version(output: &str) -> Option<(u32, u32)> {
    let mut parts = output.split_whitespace().next()?.split('.');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

fn fzf_version() -> Option<(u32, u32)> {
    // fzf reads its default options even for --version, and one it rejects
    // would hide the version.
    let out = std::process::Command::new("fzf")
        .arg("--version")
        .env_remove("FZF_DEFAULT_OPTS")
        .env_remove("FZF_DEFAULT_OPTS_FILE")
        .output()
        .ok()?;
    parse_fzf_version(&String::from_utf8_lossy(&out.stdout))
}

fn fzf_section_layout() -> &'static [&'static str] {
    if fzf_version().is_some_and(|version| version >= FZF_SECTION_BORDERS_SINCE) {
        FZF_SECTION_LAYOUT
    } else {
        &[]
    }
}

fn pick_with_fzf(entries: &[ListEntry]) -> anyhow::Result<()> {
    use std::io::Write as _;

    let width = popup_width();
    let cols = fit_columns(entries, width.saturating_sub(POPUP_CHROME));

    let header = format!(
        "{:<state$}  {:<name$}  {:<summary$}  {:<cwd$}  {:<name$}",
        "STATE",
        truncate_at(HEADER_NAME, cols.name),
        "SUMMARY",
        HEADER_CWD,
        "SESSION",
        state = COL_STATE,
        name = cols.name,
        summary = cols.summary,
        cwd = cols.cwd,
    );

    let rows: Vec<String> = entries
        .iter()
        .map(|entry| {
            format!(
                "{}\t{:<state$}  {:<name$}  {:<summary$}  {:<cwd$}  {:<name$}",
                entry.target,
                entry.state,
                truncate_at(&entry.claude_session, cols.name),
                truncate_at(&entry.summary, cols.summary),
                truncate_at(&entry.cwd, cols.cwd),
                truncate_at(&entry.session_name, cols.name),
                state = COL_STATE,
                name = cols.name,
                summary = cols.summary,
                cwd = cols.cwd,
            )
        })
        .collect();

    let input = rows.join("\n");
    let popup_size = format!("{width},50%");

    let mut child = std::process::Command::new("fzf-tmux")
        .args([
            "-p",
            &popup_size,
            "--delimiter",
            "\t",
            "--with-nth",
            "2..",
            "--header",
            &header,
            "--no-preview",
            "--reverse",
        ])
        .args(FZF_LAYOUT)
        .args(fzf_section_layout())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()?;

    if let Some(mut pipe) = child.stdin.take() {
        pipe.write_all(input.as_bytes())?;
    }

    let fzf_output = child.wait_with_output()?;

    if !fzf_output.status.success() {
        return Ok(());
    }

    let selected = String::from_utf8_lossy(&fzf_output.stdout);
    let trimmed = selected.trim();
    if let Some(chosen_target) = trimmed.split('\t').next()
        && !chosen_target.is_empty()
    {
        if let Some(entry) = entries.iter().find(|en| en.target == chosen_target) {
            drop(recent::record_switch(&entry.session_id));
        }
        tmux::switch_client(chosen_target)?;
    }

    Ok(())
}

fn pick_with_menu(entries: &[ListEntry]) -> anyhow::Result<()> {
    let items: Vec<(String, String)> = entries
        .iter()
        .map(|entry| {
            let label = format!(
                "{} | {} | {} | {} ({})",
                entry.state, entry.claude_session, entry.summary, entry.cwd, entry.session_name
            );
            (truncate_at(&label, 70), entry.target.clone())
        })
        .collect();

    tmux::display_menu("Claude Sessions", &items)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_info_all_active() {
        let counts = SessionCounts { active: 3, idle: 0 };
        let result = format_info(DEFAULT_FORMAT, &counts);
        assert!(result.contains("3"));
        assert!(result.contains("3 active"));
    }

    #[test]
    fn format_info_all_idle() {
        let counts = SessionCounts { active: 0, idle: 2 };
        let result = format_info(DEFAULT_FORMAT, &counts);
        assert!(result.contains("2"));
        assert!(result.contains("2 idle"));
    }

    #[test]
    fn format_info_mixed() {
        let counts = SessionCounts { active: 1, idle: 2 };
        let result = format_info(DEFAULT_FORMAT, &counts);
        assert!(result.contains("3"));
        assert!(result.contains("1 active, 2 idle"));
    }

    #[test]
    fn format_info_custom_template() {
        let counts = SessionCounts { active: 2, idle: 3 };
        let result = format_info("T={total} A={active} I={idle} D={detail}", &counts);
        assert_eq!(result, "T=5 A=2 I=3 D=2 active, 3 idle");
    }

    #[test]
    fn is_visible_all_filter_always_true() {
        assert!(is_visible("all", None));
        assert!(is_visible(
            "all",
            Some(&SessionCounts { active: 0, idle: 0 })
        ));
    }

    #[test]
    fn is_visible_has_claude_some() {
        assert!(is_visible(
            "has-claude",
            Some(&SessionCounts { active: 0, idle: 0 })
        ));
    }

    #[test]
    fn is_visible_has_claude_none() {
        assert!(!is_visible("has-claude", None));
    }

    #[test]
    fn is_visible_active_filter() {
        assert!(is_visible(
            "active",
            Some(&SessionCounts { active: 1, idle: 0 })
        ));
        assert!(!is_visible(
            "active",
            Some(&SessionCounts { active: 0, idle: 1 })
        ));
        assert!(!is_visible("active", None));
    }

    #[test]
    fn is_visible_idle_filter() {
        assert!(is_visible(
            "idle",
            Some(&SessionCounts { active: 0, idle: 1 })
        ));
        assert!(!is_visible(
            "idle",
            Some(&SessionCounts { active: 1, idle: 0 })
        ));
        assert!(!is_visible("idle", None));
    }

    #[test]
    fn is_visible_unknown_filter_defaults_true() {
        assert!(is_visible("unknown", None));
    }

    #[test]
    fn shorten_cwd_with_home() {
        let home = std::env::var("HOME").expect("HOME must be set");
        let path = format!("{home}/projects/test");
        assert_eq!(shorten_cwd(&path), "~/projects/test");
    }

    #[test]
    fn shorten_cwd_without_home() {
        assert_eq!(shorten_cwd("/tmp/other"), "/tmp/other");
    }

    #[test]
    fn shorten_cwd_exact_home() {
        let home = std::env::var("HOME").expect("HOME must be set");
        assert_eq!(shorten_cwd(&home), "~");
    }

    #[test]
    fn format_info_zero_counts() {
        let counts = SessionCounts { active: 0, idle: 0 };
        let result = format_info(DEFAULT_FORMAT, &counts);
        assert!(result.contains("0"));
        assert!(result.contains("0 active"));
    }

    #[test]
    fn command_exists_known_good() {
        assert!(command_exists("env"));
    }

    #[test]
    fn command_exists_known_bad() {
        assert!(!command_exists(
            "this_command_definitely_does_not_exist_xyz"
        ));
    }

    #[test]
    fn truncate_at_short() {
        assert_eq!(truncate_at("hello", 10), "hello");
    }

    #[test]
    fn truncate_at_exact() {
        assert_eq!(truncate_at("hello", 5), "hello");
    }

    #[test]
    fn truncate_at_long() {
        let result = truncate_at("hello world!", 8);
        assert_eq!(result, "hello...");
    }

    #[test]
    fn truncate_at_unicode() {
        let result = truncate_at("hellooo\u{1F916}world", 10);
        assert!(result.ends_with("..."));
        assert!(result.chars().count() <= 10);
    }

    fn named_session(name: Option<&str>) -> claude::ClaudeSession {
        claude::ClaudeSession {
            pid: 1,
            session_id: "f448c44a-ec32-4f86-a97b-f53461f94861".to_owned(),
            cwd: "/home/user".to_owned(),
            started_at: 0,
            status: None,
            spare: false,
            job_id: None,
            parked_job_id: None,
            name: name.map(str::to_owned),
        }
    }

    #[test]
    fn pane_title_matches_plain_name() {
        let session = named_session(Some("Snackbar fix"));
        assert!(pane_title_matches("Snackbar fix", &session));
    }

    #[test]
    fn pane_title_matches_name_behind_status_glyph() {
        let session = named_session(Some("Snackbar fix"));
        assert!(pane_title_matches("\u{2733} Snackbar fix", &session));
    }

    #[test]
    fn pane_title_matches_truncated_title() {
        let session = named_session(Some("Snackbar brak w nowym flow tworzenia folderu"));
        assert!(pane_title_matches("Snackbar brak w nowym", &session));
    }

    #[test]
    fn pane_title_rejects_other_name() {
        let session = named_session(Some("Snackbar fix"));
        assert!(!pane_title_matches("some other session", &session));
    }

    #[test]
    fn pane_title_rejects_session_without_name() {
        let session = named_session(None);
        assert!(!pane_title_matches("Snackbar fix", &session));
    }

    #[test]
    fn pane_title_rejects_empty_title() {
        let session = named_session(Some("Snackbar fix"));
        assert!(!pane_title_matches("", &session));
        assert!(!pane_title_matches("   ", &session));
    }

    fn pane(target: &str, title: &str) -> tmux::PaneInfo {
        tmux::PaneInfo {
            session_name: target.split(':').next().unwrap_or(target).to_owned(),
            target: target.to_owned(),
            title: title.to_owned(),
        }
    }

    #[test]
    fn assign_panes_binds_unresolved_session_by_title() {
        let session = named_session(Some("Snackbar fix"));
        let panels = vec![claude::PanelSession {
            pane_owner: &session,
            displayed: &session,
        }];
        let mut pane_map = HashMap::new();
        let _ = pane_map.insert(999_001_u32, pane("work:1.1", "\u{2733} Snackbar fix"));
        let _ = pane_map.insert(999_002_u32, pane("other:1.1", "unrelated"));

        let assigned = assign_panes(&panels, &pane_map, &process::ProcessTree::build());

        assert_eq!(assigned.len(), 1);
        assert_eq!(
            assigned.first().map(|row| row.pane.target.as_str()),
            Some("work:1.1")
        );
    }

    #[test]
    fn assign_panes_skips_ambiguous_title() {
        let session = named_session(Some("Snackbar fix"));
        let panels = vec![claude::PanelSession {
            pane_owner: &session,
            displayed: &session,
        }];
        let mut pane_map = HashMap::new();
        let _ = pane_map.insert(999_001_u32, pane("work:1.1", "Snackbar fix"));
        let _ = pane_map.insert(999_002_u32, pane("work:1.2", "Snackbar fix"));

        let assigned = assign_panes(&panels, &pane_map, &process::ProcessTree::build());

        assert!(assigned.is_empty());
    }

    #[test]
    fn assign_panes_keeps_one_row_per_pane_and_lets_the_title_decide() {
        let hidden = named_session(Some("Snackbar brak w nowym flow"));
        let shown = named_session(Some("MKL-710-process-death"));
        let panels = vec![
            claude::PanelSession {
                pane_owner: &hidden,
                displayed: &hidden,
            },
            claude::PanelSession {
                pane_owner: &shown,
                displayed: &shown,
            },
        ];
        let mut pane_map = HashMap::new();
        let _ = pane_map.insert(
            999_001_u32,
            pane("work:1.1", "\u{2733} MKL-710-process-death"),
        );

        let rows = collapse_to_one_row_per_pane(
            panels
                .iter()
                .map(|panel| (panel, pane_map.get(&999_001_u32).expect("pane")))
                .collect(),
        );

        assert_eq!(rows.len(), 1);
        let row = rows.first().expect("row");
        assert_eq!(
            row.panel.displayed.name.as_deref(),
            Some("MKL-710-process-death")
        );
        assert_eq!(row.hidden.len(), 1);
    }

    #[test]
    fn assign_panes_keeps_the_first_row_when_the_title_matches_nobody() {
        let first = named_session(Some("first"));
        let second = named_session(Some("second"));
        let panels = vec![
            claude::PanelSession {
                pane_owner: &first,
                displayed: &first,
            },
            claude::PanelSession {
                pane_owner: &second,
                displayed: &second,
            },
        ];
        let mut pane_map = HashMap::new();
        let _ = pane_map.insert(999_001_u32, pane("work:1.1", "claude agents"));

        let rows = collapse_to_one_row_per_pane(
            panels
                .iter()
                .map(|panel| (panel, pane_map.get(&999_001_u32).expect("pane")))
                .collect(),
        );

        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows.first()
                .and_then(|row| row.panel.displayed.name.as_deref()),
            Some("first")
        );
    }

    fn entry_with(claude_session: &str, summary: &str, cwd: &str, session: &str) -> ListEntry {
        ListEntry {
            target: String::new(),
            session_id: String::new(),
            state: "idle",
            claude_session: claude_session.to_owned(),
            summary: summary.to_owned(),
            cwd: cwd.to_owned(),
            session_name: session.to_owned(),
            timestamp: 0,
        }
    }

    #[test]
    fn fit_columns_shows_bounded_fields_whole() {
        let entries = vec![
            entry_with(
                "MKL-1008-analyst-WS-A-02",
                "x",
                "~/projects/a/very/long/worktree",
                "work",
            ),
            entry_with("short", "y", "~/p", "w"),
        ];

        let cols = fit_columns(&entries, 248);

        assert_eq!(cols.name, "MKL-1008-analyst-WS-A-02".len());
        assert_eq!(cols.cwd, "~/projects/a/very/long/worktree".len());
    }

    #[test]
    fn fit_columns_sizes_both_name_columns_to_the_longer_of_the_two() {
        let entries = vec![entry_with(
            "short-claude",
            "s",
            "~/p",
            "a-much-longer-tmux-session",
        )];

        let cols = fit_columns(&entries, 248);

        assert_eq!(cols.name, "a-much-longer-tmux-session".len());
    }

    #[test]
    fn fit_columns_gives_the_rest_to_the_summary() {
        let entries = vec![entry_with("name", "s", "cwd", "sess")];

        let cols = fit_columns(&entries, 200);

        let fixed = COL_STATE + cols.name + cols.cwd + cols.name + COL_GAP * 4;
        assert_eq!(cols.summary, 200 - fixed);
    }

    #[test]
    fn fit_columns_keeps_a_usable_summary_on_a_narrow_popup() {
        let entries = vec![entry_with(
            "a-rather-long-session-name",
            "s",
            "~/some/deep/path/that/goes/on",
            "sess",
        )];

        let cols = fit_columns(&entries, 40);

        assert_eq!(cols.summary, MIN_SUMMARY);
    }

    #[test]
    fn fit_columns_takes_the_overflow_from_the_working_directory_first() {
        let entries = vec![entry_with(
            "MKL-1008-analyst-WS-A-02",
            "s",
            "~/projects/mudita/Launcher-2/.claude/worktrees/task+MKL-1008",
            "MKL-1008-analyst",
        )];

        let wide = fit_columns(&entries, 248);
        let narrow = fit_columns(&entries, 120);

        assert_eq!(narrow.name, wide.name);
        assert!(narrow.cwd < wide.cwd);
    }

    #[test]
    fn fit_columns_fits_the_row_into_a_laptop_popup() {
        let entries = vec![entry_with(
            "MKL-1008-analyst-WS-A-02",
            "s",
            "~/projects/mudita/Launcher-2/.claude/worktrees/task+MKL-1008",
            "MKL-1008-analyst",
        )];

        for popup in [248_usize, 160, 120, 100, 80] {
            let cols = fit_columns(&entries, popup);
            let total = COL_STATE + cols.name + cols.cwd + cols.name + COL_GAP * 4 + cols.summary;
            assert!(
                total <= popup || cols.cwd == MIN_CWD && cols.name == MIN_NAME,
                "popup {popup} overflowed to {total}"
            );
        }
    }

    #[test]
    fn fit_columns_never_shrinks_below_the_minimums() {
        let entries = vec![entry_with(
            &"n".repeat(60),
            "s",
            &"c".repeat(90),
            &"t".repeat(60),
        )];

        let cols = fit_columns(&entries, 30);

        assert_eq!(cols.name, MIN_NAME);
        assert_eq!(cols.cwd, MIN_CWD);
        assert_eq!(cols.summary, MIN_SUMMARY);
    }

    #[test]
    fn fit_columns_keeps_the_headers_over_their_columns() {
        let entries = vec![entry_with("tmux-e9", "s", "~", "config")];

        let cols = fit_columns(&entries, 242);

        assert_eq!(cols.name, HEADER_NAME.len());
        assert_eq!(cols.cwd, HEADER_CWD.len());
    }

    #[test]
    fn parse_fzf_version_reads_major_and_minor() {
        assert_eq!(parse_fzf_version("0.74.4 (Homebrew)\n"), Some((0, 74)));
        assert_eq!(parse_fzf_version("0.44.1 (debian)"), Some((0, 44)));
        assert_eq!(parse_fzf_version("0.58.0"), Some((0, 58)));
    }

    #[test]
    fn parse_fzf_version_rejects_what_it_cannot_read() {
        assert_eq!(parse_fzf_version(""), None);
        assert_eq!(parse_fzf_version("fzf"), None);
        assert_eq!(parse_fzf_version("1"), None);
    }

    #[test]
    fn fit_columns_caps_a_pathological_field() {
        let entries = vec![entry_with(&"n".repeat(200), "s", &"c".repeat(200), "sess")];

        let cols = fit_columns(&entries, 248);

        assert_eq!(cols.name, MAX_NAME);
        assert_eq!(cols.cwd, MAX_CWD);
    }

    fn make_entry(state: &'static str, timestamp: u64) -> ListEntry {
        ListEntry {
            target: String::new(),
            session_id: String::new(),
            state,
            claude_session: String::new(),
            summary: String::new(),
            cwd: String::new(),
            session_name: String::new(),
            timestamp,
        }
    }

    #[test]
    fn sort_order_parse_all_variants() {
        assert!(matches!(SortOrder::parse("recent"), SortOrder::Recent));
        assert!(matches!(
            SortOrder::parse("timestamp-desc"),
            SortOrder::TimestampDesc
        ));
        assert!(matches!(
            SortOrder::parse("timestamp-asc"),
            SortOrder::TimestampAsc
        ));
        assert!(matches!(SortOrder::parse("status"), SortOrder::Status));
        assert!(matches!(
            SortOrder::parse("status-rev"),
            SortOrder::StatusRev
        ));
    }

    #[test]
    fn sort_order_parse_unknown_defaults_to_recent() {
        assert!(matches!(SortOrder::parse("unknown"), SortOrder::Recent));
        assert!(matches!(SortOrder::parse(""), SortOrder::Recent));
    }

    #[test]
    fn sort_entries_timestamp_desc() {
        let mut entries = vec![
            make_entry("idle", 100),
            make_entry("active", 300),
            make_entry("idle", 200),
        ];
        sort_entries(&mut entries, SortOrder::TimestampDesc);
        assert_eq!(entries[0].timestamp, 300);
        assert_eq!(entries[1].timestamp, 200);
        assert_eq!(entries[2].timestamp, 100);
    }

    #[test]
    fn sort_entries_timestamp_asc() {
        let mut entries = vec![
            make_entry("idle", 300),
            make_entry("active", 100),
            make_entry("idle", 200),
        ];
        sort_entries(&mut entries, SortOrder::TimestampAsc);
        assert_eq!(entries[0].timestamp, 100);
        assert_eq!(entries[1].timestamp, 200);
        assert_eq!(entries[2].timestamp, 300);
    }

    #[test]
    fn sort_entries_status_idle_first() {
        let mut entries = vec![
            make_entry("active", 300),
            make_entry("idle", 100),
            make_entry("active", 200),
        ];
        sort_entries(&mut entries, SortOrder::Status);
        assert_eq!(entries[0].state, "idle");
        assert_eq!(entries[1].state, "active");
        assert_eq!(entries[2].state, "active");
    }

    #[test]
    fn sort_entries_status_tiebreaks_by_timestamp_desc() {
        let mut entries = vec![
            make_entry("active", 100),
            make_entry("active", 300),
            make_entry("active", 200),
        ];
        sort_entries(&mut entries, SortOrder::Status);
        assert_eq!(entries[0].timestamp, 300);
        assert_eq!(entries[1].timestamp, 200);
        assert_eq!(entries[2].timestamp, 100);
    }

    #[test]
    fn sort_entries_status_rev_active_first() {
        let mut entries = vec![
            make_entry("active", 200),
            make_entry("idle", 300),
            make_entry("idle", 100),
        ];
        sort_entries(&mut entries, SortOrder::StatusRev);
        assert_eq!(entries[0].state, "active");
        assert_eq!(entries[1].state, "idle");
        assert_eq!(entries[2].state, "idle");
    }

    #[test]
    fn sort_entries_status_rev_tiebreaks_by_timestamp_asc() {
        let mut entries = vec![
            make_entry("idle", 100),
            make_entry("idle", 300),
            make_entry("idle", 200),
        ];
        sort_entries(&mut entries, SortOrder::StatusRev);
        assert_eq!(entries[0].timestamp, 100);
        assert_eq!(entries[1].timestamp, 200);
        assert_eq!(entries[2].timestamp, 300);
    }

    #[test]
    fn sort_entries_timestamp_asc_tiebreaks_by_timestamp_desc() {
        let mut entries = vec![
            make_entry("active", 300),
            make_entry("idle", 100),
            make_entry("active", 200),
        ];
        sort_entries(&mut entries, SortOrder::TimestampAsc);
        assert_eq!(entries[0].timestamp, 100);
        assert_eq!(entries[1].timestamp, 200);
        assert_eq!(entries[2].timestamp, 300);
    }

    #[test]
    fn sort_entries_empty() {
        let mut entries: Vec<ListEntry> = vec![];
        sort_entries(&mut entries, SortOrder::Status);
        assert!(entries.is_empty());
    }

    #[test]
    fn sort_entries_single() {
        let mut entries = vec![make_entry("active", 100)];
        sort_entries(&mut entries, SortOrder::Status);
        assert_eq!(entries[0].timestamp, 100);
    }

    fn make_recent_entry(target: &str, session_id: &str, timestamp: u64) -> ListEntry {
        ListEntry {
            target: target.to_owned(),
            session_id: session_id.to_owned(),
            state: "active",
            claude_session: String::new(),
            summary: String::new(),
            cwd: String::new(),
            session_name: String::new(),
            timestamp,
        }
    }

    #[test]
    fn sort_recent_current_session_last() {
        let mut entries = vec![
            make_recent_entry("0:1.1", "sess-a", 100),
            make_recent_entry("0:1.2", "sess-b", 200),
            make_recent_entry("0:1.3", "sess-c", 300),
        ];
        let recent_data = vec![
            recent::RecentEntry {
                session_id: "sess-c".to_owned(),
                switched_at: 1000,
            },
            recent::RecentEntry {
                session_id: "sess-a".to_owned(),
                switched_at: 900,
            },
            recent::RecentEntry {
                session_id: "sess-b".to_owned(),
                switched_at: 800,
            },
        ];
        sort_entries_recent(&mut entries, &recent_data, Some("0:1.3"));
        assert_eq!(entries.first().expect("entry").session_id, "sess-a");
        assert_eq!(entries.get(1).expect("entry").session_id, "sess-b");
        assert_eq!(entries.get(2).expect("entry").session_id, "sess-c");
    }

    #[test]
    fn sort_recent_tracked_by_switched_at_desc() {
        let mut entries = vec![
            make_recent_entry("0:1.1", "sess-a", 100),
            make_recent_entry("0:1.2", "sess-b", 200),
            make_recent_entry("0:1.3", "sess-c", 300),
        ];
        let recent_data = vec![
            recent::RecentEntry {
                session_id: "sess-b".to_owned(),
                switched_at: 1000,
            },
            recent::RecentEntry {
                session_id: "sess-c".to_owned(),
                switched_at: 900,
            },
            recent::RecentEntry {
                session_id: "sess-a".to_owned(),
                switched_at: 800,
            },
        ];
        sort_entries_recent(&mut entries, &recent_data, None);
        assert_eq!(entries.first().expect("entry").session_id, "sess-b");
        assert_eq!(entries.get(1).expect("entry").session_id, "sess-c");
        assert_eq!(entries.get(2).expect("entry").session_id, "sess-a");
    }

    #[test]
    fn sort_recent_untracked_after_tracked() {
        let mut entries = vec![
            make_recent_entry("0:1.1", "sess-a", 300),
            make_recent_entry("0:1.2", "sess-b", 100),
            make_recent_entry("0:1.3", "sess-c", 200),
        ];
        let recent_data = vec![recent::RecentEntry {
            session_id: "sess-b".to_owned(),
            switched_at: 1000,
        }];
        sort_entries_recent(&mut entries, &recent_data, None);
        assert_eq!(entries.first().expect("entry").session_id, "sess-b");
        assert_eq!(entries.get(1).expect("entry").session_id, "sess-a");
        assert_eq!(entries.get(2).expect("entry").session_id, "sess-c");
    }

    #[test]
    fn sort_recent_untracked_sorted_by_timestamp_desc() {
        let mut entries = vec![
            make_recent_entry("0:1.1", "sess-a", 100),
            make_recent_entry("0:1.2", "sess-b", 300),
            make_recent_entry("0:1.3", "sess-c", 200),
        ];
        let recent_data: Vec<recent::RecentEntry> = vec![];
        sort_entries_recent(&mut entries, &recent_data, None);
        assert_eq!(entries.first().expect("entry").session_id, "sess-b");
        assert_eq!(entries.get(1).expect("entry").session_id, "sess-c");
        assert_eq!(entries.get(2).expect("entry").session_id, "sess-a");
    }

    #[test]
    fn sort_recent_full_harpoon_order() {
        let mut entries = vec![
            make_recent_entry("0:1.1", "sess-current", 100),
            make_recent_entry("0:1.2", "sess-prev", 200),
            make_recent_entry("0:1.3", "sess-old", 300),
            make_recent_entry("0:1.4", "sess-untracked", 400),
        ];
        let recent_data = vec![
            recent::RecentEntry {
                session_id: "sess-current".to_owned(),
                switched_at: 1000,
            },
            recent::RecentEntry {
                session_id: "sess-prev".to_owned(),
                switched_at: 900,
            },
            recent::RecentEntry {
                session_id: "sess-old".to_owned(),
                switched_at: 800,
            },
        ];
        sort_entries_recent(&mut entries, &recent_data, Some("0:1.1"));
        assert_eq!(entries.first().expect("entry").session_id, "sess-prev");
        assert_eq!(entries.get(1).expect("entry").session_id, "sess-old");
        assert_eq!(entries.get(2).expect("entry").session_id, "sess-untracked");
        assert_eq!(entries.get(3).expect("entry").session_id, "sess-current");
    }

    #[test]
    fn sort_recent_empty() {
        let mut entries: Vec<ListEntry> = vec![];
        let recent_data: Vec<recent::RecentEntry> = vec![];
        sort_entries_recent(&mut entries, &recent_data, None);
        assert!(entries.is_empty());
    }
}
