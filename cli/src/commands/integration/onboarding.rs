//! The progress this computer reports to the server while it discovers, connects and imports coding
//! agents and delivers their past sessions, so the web app can show it; and the server's request to
//! retry turns that were set aside.
//!
//! The routes are `/api/me/coding-agents/sync/{device_id}` (report) and `/api/me/coding-agents/sync`
//! (read back, with `retry_requested_at`). A report that fails is logged and never stops the work it
//! describes.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::api::Client;

use super::agents::Agent;
use super::backlog::AgentBacklog;
use super::import::{ImportProgress, ImportRun, WindowStatus as ReadStatus};

/// Which local client this copy is, as the sync report names it.
const CLIENT: &str = "cli";
/// Reports go out at most this often, except on a phase change and at the end.
const REPORT_INTERVAL: Duration = Duration::from_secs(1);
const SYNC_PATH: &str = "/me/coding-agents/sync";
const DEVICE_ID_FILE: &str = "device_id";
const DEVICE_ID_MAX: usize = 64;
const DEVICE_NAME_MAX: usize = 128;
const ERROR_MAX: usize = 500;
const WINDOW_LABEL_MAX: usize = 64;
const FALLBACK_DEVICE_NAME: &str = "This computer";
/// The last `retry_requested_at` this computer acted on.
const RETRY_HANDLED_FILE: &str = "retry_handled_at";

// ─── Progress ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Discovering,
    Connecting,
    Importing,
    /// Every window read; queued turns are still being delivered.
    Sending,
    /// Every window done and nothing left to deliver.
    Done,
    /// The run couldn't go on (signed out, say). Undelivered turns alone never fail a run.
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowStatus {
    Waiting,
    Running,
    /// Read, and every turn it queued accepted by the server (or set aside).
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WindowReport {
    pub label: String,
    pub status: WindowStatus,
    pub sessions: u32,
    pub turns: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReasonCount {
    pub reason: String,
    pub count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportStatus {
    Waiting,
    Running,
    Done,
    Failed,
    /// Not on this computer, or not connected.
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportReport {
    pub status: ImportStatus,
    pub done: u32,
    pub total: u32,
    pub sessions: u32,
    pub turns: u32,
    pub failed: u32,
    pub error: Option<String>,
    /// Turns queued here, not yet accepted.
    pub pending: u32,
    /// Turns set aside.
    pub rejected: u32,
    pub rejected_reasons: Vec<ReasonCount>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentReport {
    pub id: &'static str,
    pub name: &'static str,
    pub detected: bool,
    pub connected: bool,
    pub import: ImportReport,
}

/// One run on this computer, as `PUT /api/me/coding-agents/sync/{device_id}` takes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SyncReport {
    pub device_name: String,
    pub client: &'static str,
    pub phase: Phase,
    /// The import's windows, newest first.
    pub windows: Vec<WindowReport>,
    pub agents: Vec<AgentReport>,
    pub error: Option<String>,
}

/// One agent's row before its import starts.
pub fn agent_row(agent: Agent, detected: bool, connected: bool) -> AgentReport {
    let spec = agent.spec();
    AgentReport {
        id: spec.id,
        name: spec.display_name,
        detected,
        connected,
        import: ImportReport {
            status: if connected {
                ImportStatus::Waiting
            } else {
                ImportStatus::Skipped
            },
            done: 0,
            total: 0,
            sessions: 0,
            turns: 0,
            failed: 0,
            error: None,
            pending: 0,
            rejected: 0,
            rejected_reasons: Vec::new(),
        },
    }
}

/// Sends reports, throttled. Without a client (a server that predates the routes) it sends nothing.
pub struct Reporter {
    client: Option<Client>,
    device_id: String,
    last: Option<(Instant, Phase)>,
    failing: bool,
}

impl Reporter {
    pub fn new(client: Option<Client>, device_id: String) -> Self {
        Self {
            client,
            device_id,
            last: None,
            failing: false,
        }
    }

    /// Sends `report` unless one went out less than [`REPORT_INTERVAL`] ago in the same phase;
    /// `force` sends it regardless (the final report).
    pub fn send(&mut self, report: &SyncReport, force: bool) {
        let now = Instant::now();
        if !force && !due(self.last, report.phase, now) {
            return;
        }
        self.last = Some((now, report.phase));
        let Some(client) = &self.client else {
            return;
        };
        let path = format!("{SYNC_PATH}/{}", self.device_id);
        match client.put_json_quiet::<serde_json::Value, _>(&path, report) {
            Ok(_) => self.failing = false,
            // Once per run of failures, not once per second.
            Err(error) if !self.failing => {
                self.failing = true;
                eprintln!("warning: couldn't report coding-agent setup progress: {error:#}");
            }
            Err(_) => {}
        }
    }
}

fn due(last: Option<(Instant, Phase)>, phase: Phase, now: Instant) -> bool {
    last.is_none_or(|(at, last_phase)| {
        last_phase != phase || now.duration_since(at) >= REPORT_INTERVAL
    })
}

/// One run's report, kept current and sent as it changes.
pub struct Tracker {
    report: SyncReport,
    reporter: Reporter,
}

impl Tracker {
    pub fn new(reporter: Reporter) -> Self {
        Self {
            report: SyncReport {
                device_name: device_name(),
                client: CLIENT,
                phase: Phase::Discovering,
                windows: Vec::new(),
                agents: Vec::new(),
                error: None,
            },
            reporter,
        }
    }

    /// The desktop app's copy shows it on its own page; the CLI prints as it goes instead.
    #[allow(dead_code)]
    pub fn report(&self) -> &SyncReport {
        &self.report
    }

    /// Moves to `phase` with `agents` as the rows (detection and connection as they stand now).
    pub fn phase(&mut self, phase: Phase, agents: Vec<AgentReport>) {
        self.report.phase = phase;
        self.report.agents = agents;
        self.reporter.send(&self.report, false);
    }

    /// Follows an import: its windows, each agent's progress and backlog, and `sending` once every
    /// window is read while turns are still waiting.
    pub fn import(&mut self, run: &ImportRun) {
        self.report.phase = if run.sending.is_some() {
            Phase::Sending
        } else {
            Phase::Importing
        };
        self.report.windows = run.windows.iter().map(window_report).collect();
        for (agent, progress) in &run.agents {
            let id = agent.spec().id;
            if let Some(row) = self.report.agents.iter_mut().find(|row| row.id == id) {
                row.import = import_report(progress, &run.backlog.agent(id));
            }
        }
        self.reporter.send(&self.report, false);
    }

    /// The last report: failed with `error`; otherwise done when every window is done and nothing is
    /// left to deliver, else still sending. Rows whose import never started are skipped.
    pub fn finish(&mut self, error: Option<String>) {
        for row in &mut self.report.agents {
            if row.import.status == ImportStatus::Waiting {
                row.import.status = ImportStatus::Skipped;
            }
        }
        self.report.phase = final_phase(&self.report, error.is_some());
        self.report.error = error.map(|e| bounded(&e, ERROR_MAX));
        self.reporter.send(&self.report, true);
    }
}

fn final_phase(report: &SyncReport, failed: bool) -> Phase {
    let delivered = report
        .windows
        .iter()
        .all(|window| window.status == WindowStatus::Done)
        && report.agents.iter().all(|agent| agent.import.pending == 0);
    match (failed, delivered) {
        (true, _) => Phase::Failed,
        (false, true) => Phase::Done,
        (false, false) => Phase::Sending,
    }
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn window_report(window: &super::import::WindowProgress) -> WindowReport {
    WindowReport {
        label: bounded(&window.label, WINDOW_LABEL_MAX),
        status: match window.status {
            ReadStatus::Waiting => WindowStatus::Waiting,
            ReadStatus::Reading | ReadStatus::Sending => WindowStatus::Running,
            ReadStatus::Done => WindowStatus::Done,
        },
        sessions: count(window.sessions),
        turns: count(window.turns),
    }
}

fn import_report(progress: &ImportProgress, backlog: &AgentBacklog) -> ImportReport {
    ImportReport {
        status: match (progress.finished, &progress.error) {
            (_, Some(_)) => ImportStatus::Failed,
            (true, None) => ImportStatus::Done,
            (false, None) => ImportStatus::Running,
        },
        done: count(progress.done),
        total: count(progress.total),
        sessions: count(progress.sessions),
        turns: count(progress.turns),
        failed: count(progress.failed),
        error: progress.error.as_deref().map(|e| bounded(e, ERROR_MAX)),
        pending: count(backlog.pending),
        rejected: count(backlog.rejected),
        rejected_reasons: backlog
            .reasons
            .iter()
            .map(|(reason, n)| ReasonCount {
                reason: reason.clone(),
                count: count(*n),
            })
            .collect(),
    }
}

// ─── Retry requests ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct Runs {
    runs: Vec<RunRetry>,
}

#[derive(Deserialize)]
struct RunRetry {
    device_id: String,
    #[serde(default)]
    retry_requested_at: Option<DateTime<Utc>>,
}

/// Whether the user asked (in the web app) to retry this computer's set-aside turns since it last did.
/// The request is marked handled in `dir` as it is returned, so it is acted on once. A server without
/// the route, or without a run for this computer, asks for nothing.
pub fn take_retry_request(client: &Client, dir: &Path, device_id: &str) -> Result<bool> {
    let Some(runs) = client.get_json_optional::<Runs>(SYNC_PATH)? else {
        return Ok(false);
    };
    let requested = runs
        .runs
        .into_iter()
        .find(|run| run.device_id == device_id)
        .and_then(|run| run.retry_requested_at);
    let path = dir.join(RETRY_HANDLED_FILE);
    let handled = std::fs::read_to_string(&path)
        .ok()
        .and_then(|at| at.trim().parse::<DateTime<Utc>>().ok());
    let Some(requested) = requested.filter(|at| handled.is_none_or(|handled| *at > handled)) else {
        return Ok(false);
    };
    std::fs::create_dir_all(dir)?;
    std::fs::write(&path, requested.to_rfc3339())?;
    Ok(true)
}

// ─── This computer ───────────────────────────────────────────────────────────

/// This computer's id for sync reports, made once and kept in `dir`.
pub fn device_id(dir: &Path) -> String {
    let path = dir.join(DEVICE_ID_FILE);
    if let Some(id) = std::fs::read_to_string(&path)
        .ok()
        .map(|id| id.trim().to_string())
        .filter(|id| valid_device_id(id))
    {
        return id;
    }
    let id = uuid::Uuid::new_v4().simple().to_string();
    let saved = std::fs::create_dir_all(dir).and_then(|()| std::fs::write(&path, &id));
    if let Err(error) = saved {
        eprintln!(
            "warning: couldn't save this computer's id in {}: {error}",
            path.display()
        );
    }
    id
}

fn valid_device_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= DEVICE_ID_MAX
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The computer's host name, for the web app to tell devices apart.
fn device_name() -> String {
    let from_command = std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok());
    let name = from_command
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| FALLBACK_DEVICE_NAME.to_string());
    bounded(&name, DEVICE_NAME_MAX)
}

fn bounded(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(phase: Phase) -> SyncReport {
        SyncReport {
            device_name: "laptop".into(),
            client: CLIENT,
            phase,
            windows: vec![],
            agents: vec![agent_row(Agent::Codex, true, true)],
            error: None,
        }
    }

    /// An import of Claude Code's sessions: one window done, one still sending with `pending` turns.
    fn import_run(pending: usize) -> ImportRun {
        let mut backlog = super::super::backlog::Backlog::default();
        backlog.agents.insert(
            "claude".into(),
            AgentBacklog {
                pending,
                rejected: 3,
                reasons: vec![("permanently rejected: too big".into(), 3)],
            },
        );
        ImportRun {
            agents: vec![(
                Agent::Claude,
                ImportProgress {
                    done: 2,
                    total: 5,
                    sessions: 1,
                    turns: 7,
                    ..ImportProgress::default()
                },
            )],
            windows: vec![
                super::super::import::WindowProgress::for_test(
                    "Last 8 hours",
                    ReadStatus::Done,
                    2,
                    7,
                ),
                super::super::import::WindowProgress::for_test("Oct 19", ReadStatus::Sending, 3, 0),
            ],
            backlog,
            sending: None,
            delivered: 0,
        }
    }

    #[test]
    fn reports_are_due_on_a_phase_change_or_after_the_interval() {
        let start = Instant::now();
        let last = Some((start, Phase::Importing));

        assert!(due(None, Phase::Discovering, start));
        assert!(!due(
            last,
            Phase::Importing,
            start + Duration::from_millis(500)
        ));
        assert!(due(last, Phase::Done, start + Duration::from_millis(1)));
        assert!(due(last, Phase::Importing, start + REPORT_INTERVAL));
    }

    #[test]
    fn the_reporter_throttles_within_a_phase_and_always_sends_the_last_report() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("PUT", "/api/me/coding-agents/sync/dev-1")
            .match_header("authorization", "Bearer t")
            .with_status(200)
            .with_body("{}")
            .expect(3)
            .create();
        let mut reporter = Reporter::new(
            Some(Client::for_test(&server.url(), Some("t"))),
            "dev-1".into(),
        );

        reporter.send(&report(Phase::Importing), false);
        reporter.send(&report(Phase::Importing), false);
        reporter.send(&report(Phase::Importing), false);
        reporter.send(&report(Phase::Done), false);
        reporter.send(&report(Phase::Done), true);

        mock.assert();
    }

    #[test]
    fn a_failing_server_never_stops_the_reporter() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("PUT", "/api/me/coding-agents/sync/dev-1")
            .with_status(500)
            .with_body("boom")
            .expect(2)
            .create();
        let mut reporter =
            Reporter::new(Some(Client::for_test(&server.url(), None)), "dev-1".into());

        reporter.send(&report(Phase::Discovering), false);
        reporter.send(&report(Phase::Failed), true);

        mock.assert();
        // Nowhere to send to is not an error either.
        Reporter::new(None, "dev-1".into()).send(&report(Phase::Done), true);
    }

    #[test]
    fn the_report_body_matches_the_wire_contract() {
        let mut tracker = Tracker::new(Reporter::new(None, "dev".into()));
        tracker.phase(
            Phase::Importing,
            vec![
                agent_row(Agent::Claude, true, true),
                agent_row(Agent::Cursor, false, false),
            ],
        );
        tracker.import(&import_run(4));
        let body = serde_json::to_value(tracker.report()).unwrap();

        assert_eq!(body["client"], "cli");
        assert_eq!(body["phase"], "importing");
        assert_eq!(body["windows"][0]["label"], "Last 8 hours");
        assert_eq!(body["windows"][0]["status"], "done");
        assert_eq!(body["windows"][1]["status"], "running");
        assert_eq!(body["windows"][1]["sessions"], 3);
        let claude = &body["agents"][0];
        assert_eq!(claude["id"], "claude");
        assert_eq!(claude["import"]["status"], "running");
        assert_eq!(claude["import"]["done"], 2);
        assert_eq!(claude["import"]["pending"], 4);
        assert_eq!(claude["import"]["rejected"], 3);
        assert_eq!(
            claude["import"]["rejected_reasons"][0],
            serde_json::json!({"reason": "permanently rejected: too big", "count": 3})
        );
        assert_eq!(body["agents"][1]["import"]["status"], "skipped");
        assert_eq!(body["agents"][1]["import"]["pending"], 0);

        tracker.finish(Some("x".repeat(ERROR_MAX * 2)));
        let body = serde_json::to_value(tracker.report()).unwrap();
        assert_eq!(body["phase"], "failed");
        assert_eq!(body["error"].as_str().unwrap().len(), ERROR_MAX);
    }

    #[test]
    fn a_run_is_never_done_while_turns_are_pending_or_a_window_is_open() {
        let finished = |run: ImportRun| {
            let mut tracker = Tracker::new(Reporter::new(None, "dev".into()));
            tracker.phase(Phase::Importing, vec![agent_row(Agent::Claude, true, true)]);
            tracker.import(&run);
            tracker.finish(None);
            tracker.report().phase
        };
        let mut sending = import_run(4);
        sending.sending = Some(super::super::import::Sending {
            next_attempt_in: None,
            blocked: None,
        });
        let mut open_window = import_run(0);
        open_window.windows[1].status = ReadStatus::Sending;
        let mut delivered = import_run(0);
        delivered.windows[1].status = ReadStatus::Done;

        assert_eq!(finished(sending), Phase::Sending);
        assert_eq!(finished(open_window), Phase::Sending);
        assert_eq!(finished(delivered), Phase::Done);
    }

    #[test]
    fn a_retry_request_is_taken_once() {
        let mut server = mockito::Server::new();
        let dir = tempfile::tempdir().unwrap();
        let runs = |at: &str| {
            format!(
                r#"{{"runs":[{{"device_id":"other","retry_requested_at":"2030-01-01T00:00:00Z"}},
                             {{"device_id":"dev","retry_requested_at":{at}}}]}}"#
            )
        };
        let client = Client::for_test(&server.url(), Some("t"));
        let none = server
            .mock("GET", "/api/me/coding-agents/sync")
            .with_body(runs("null"))
            .create();
        assert!(!take_retry_request(&client, dir.path(), "dev").unwrap());
        none.remove();

        let first = server
            .mock("GET", "/api/me/coding-agents/sync")
            .with_body(runs(r#""2026-10-21T10:00:00Z""#))
            .create();
        assert!(take_retry_request(&client, dir.path(), "dev").unwrap());
        assert!(!take_retry_request(&client, dir.path(), "dev").unwrap());
        first.remove();

        server
            .mock("GET", "/api/me/coding-agents/sync")
            .with_body(runs(r#""2026-10-21T11:00:00Z""#))
            .create();
        assert!(take_retry_request(&client, dir.path(), "dev").unwrap());
    }

    #[test]
    fn a_server_without_the_runs_route_asks_for_no_retry() {
        let mut server = mockito::Server::new();
        server
            .mock("GET", "/api/me/coding-agents/sync")
            .with_status(404)
            .create();
        let dir = tempfile::tempdir().unwrap();

        assert!(
            !take_retry_request(&Client::for_test(&server.url(), None), dir.path(), "dev").unwrap()
        );
    }

    #[test]
    fn a_device_id_is_made_once_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let first = device_id(dir.path());
        assert!(valid_device_id(&first));
        assert_eq!(device_id(dir.path()), first);

        std::fs::write(dir.path().join(DEVICE_ID_FILE), "../not an id").unwrap();
        assert_ne!(device_id(dir.path()), "../not an id");
    }
}
