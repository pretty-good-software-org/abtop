//! Optional client for the agent-orchestrator status API.
//!
//! The HTTP request runs on a worker thread. `MultiCollector::collect` only
//! polls a channel, so an unavailable orchestrator cannot delay a TUI tick.

use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const ORCHESTRATOR_URL_ENV: &str = "ABTOP_ORCHESTRATOR_URL";
pub const ORCHESTRATOR_TOKEN_ENV: &str = "ABTOP_ORCHESTRATOR_TOKEN";
const STATUS_PATH: &str = "/v1/agents/status";
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(1);
const API_VERSION: &str = "v1";
const STALE_AFTER_SECONDS: u64 = 30;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct OrchestratorStatus {
    pub api_version: String,
    pub generated_at: String,
    pub freshness: OrchestratorFreshness,
    pub agents: Vec<OrchestratorAgent>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct OrchestratorFreshness {
    pub observed_at: String,
    pub stale_after_seconds: u64,
    pub stale: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct OrchestratorAgent {
    pub session: String,
    pub pid: Option<u32>,
    pub agent: Option<String>,
    pub tmux_state: OrchestratorTmuxState,
    pub liveness: OrchestratorLiveness,
    pub window_id: Option<String>,
    pub correlation: Option<OrchestratorCorrelation>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum OrchestratorTmuxState {
    Attached,
    Detached,
    Dead,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum OrchestratorLiveness {
    Live,
    Stalled,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct OrchestratorCorrelation {
    pub task_id: Option<String>,
    pub group_id: Option<String>,
    pub node_id: Option<String>,
}

#[derive(Clone, Debug)]
struct OrchestratorConfig {
    endpoint: String,
    token: Option<String>,
    timeout: Duration,
}

impl OrchestratorConfig {
    fn from_env() -> Option<Self> {
        let url = std::env::var(ORCHESTRATOR_URL_ENV).ok()?;
        let url = url.trim();
        if url.is_empty() {
            return None;
        }

        Some(Self {
            endpoint: status_endpoint(url),
            token: std::env::var(ORCHESTRATOR_TOKEN_ENV)
                .ok()
                .filter(|token| !token.is_empty()),
            timeout: REQUEST_TIMEOUT,
        })
    }
}

fn status_endpoint(base: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.ends_with(STATUS_PATH) {
        base.to_string()
    } else {
        format!("{base}{STATUS_PATH}")
    }
}

/// Parse and validate one v1 response from the orchestrator.
pub fn parse_status(body: &str) -> Result<OrchestratorStatus, String> {
    let mut status: OrchestratorStatus =
        serde_json::from_str(body).map_err(|e| format!("parse orchestrator status: {e}"))?;

    if status.api_version != API_VERSION {
        return Err(format!(
            "parse orchestrator status: unsupported api_version {:?}",
            status.api_version
        ));
    }
    validate_timestamp("generated_at", &status.generated_at)?;
    validate_timestamp("freshness.observed_at", &status.freshness.observed_at)?;
    if status.freshness.stale_after_seconds != STALE_AFTER_SECONDS {
        return Err(format!(
            "parse orchestrator status: freshness.stale_after_seconds must be {STALE_AFTER_SECONDS}"
        ));
    }

    for agent in &status.agents {
        if agent.session.trim().is_empty() {
            return Err("parse orchestrator status: agent session is empty".to_string());
        }
        if agent
            .agent
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err("parse orchestrator status: agent label is empty".to_string());
        }
        if agent
            .window_id
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err("parse orchestrator status: window_id is empty".to_string());
        }
        if let Some(correlation) = &agent.correlation {
            for value in [
                correlation.task_id.as_ref(),
                correlation.group_id.as_ref(),
                correlation.node_id.as_ref(),
            ] {
                if value.is_some_and(|value| value.trim().is_empty()) {
                    return Err("parse orchestrator status: correlation value is empty".to_string());
                }
            }
        }
    }

    // The contract says the list is sorted and keyed by session. Sorting and
    // deduplicating defensively keeps a malformed producer from duplicating
    // one remote session in downstream snapshots.
    status.agents.sort_by(|a, b| a.session.cmp(&b.session));
    status.agents.dedup_by(|a, b| a.session == b.session);
    Ok(status)
}

fn validate_timestamp(field: &str, value: &str) -> Result<(), String> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|_| ())
        .map_err(|e| format!("parse orchestrator status: invalid {field}: {e}"))
}

fn observed_at_epoch_seconds(status: &OrchestratorStatus) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(&status.freshness.observed_at)
        .ok()?
        .timestamp()
        .try_into()
        .ok()
}

impl OrchestratorStatus {
    /// Find a local session's remote record, preferring PID over the stable
    /// session name because names can be reused across host processes.
    pub fn find_agent(&self, session_id: &str, pid: u32) -> Option<&OrchestratorAgent> {
        self.agents
            .iter()
            .find(|agent| agent.pid == Some(pid))
            .or_else(|| self.agents.iter().find(|agent| agent.session == session_id))
    }

    pub fn is_stale_at(&self, now: SystemTime) -> bool {
        if self.freshness.stale {
            return true;
        }
        let observed = match observed_at_epoch_seconds(self) {
            Some(observed) => observed,
            None => return true,
        };
        let current = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        current.saturating_sub(observed) > self.freshness.stale_after_seconds
    }

    /// Return the status with the client-side freshness bit recalculated.
    pub fn with_current_staleness(&self, now: SystemTime) -> Self {
        let mut status = self.clone();
        status.freshness.stale = self.is_stale_at(now);
        status
    }
}

fn fetch_status(config: &OrchestratorConfig) -> Result<OrchestratorStatus, String> {
    let client = Client::builder()
        .timeout(config.timeout)
        .build()
        .map_err(|e| format!("build orchestrator HTTP client: {e}"))?;
    let mut request = client.get(&config.endpoint);
    if let Some(token) = &config.token {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .map_err(|e| format!("fetch orchestrator status: {e}"))?;
    let status_code = response.status();
    if !status_code.is_success() {
        return Err(format!(
            "fetch orchestrator status: HTTP {}",
            status_code.as_u16()
        ));
    }
    let body = response
        .text()
        .map_err(|e| format!("read orchestrator status response: {e}"))?;
    parse_status(&body)
}

type FetchResult = Result<OrchestratorStatus, String>;

/// Polls the optional source without doing network I/O on the caller thread.
pub struct OrchestratorSource {
    config: OrchestratorConfig,
    status: Option<OrchestratorStatus>,
    in_flight: bool,
    last_started: Option<Instant>,
    tx: mpsc::Sender<FetchResult>,
    rx: Receiver<FetchResult>,
}

impl OrchestratorSource {
    pub fn from_env() -> Option<Self> {
        Self::from_config(OrchestratorConfig::from_env()?)
    }

    fn from_config(config: OrchestratorConfig) -> Option<Self> {
        let (tx, rx) = mpsc::channel();
        Some(Self {
            config,
            status: None,
            in_flight: false,
            last_started: None,
            tx,
            rx,
        })
    }

    #[cfg(test)]
    fn for_test(endpoint: String, token: Option<String>, timeout: Duration) -> Self {
        Self::from_config(OrchestratorConfig {
            endpoint,
            token,
            timeout,
        })
        .expect("test source config")
    }

    /// Start a refresh when needed and return the last completed response.
    pub fn poll(&mut self) -> Option<OrchestratorStatus> {
        while let Ok(result) = self.rx.try_recv() {
            self.in_flight = false;
            if let Ok(status) = result {
                self.status = Some(status);
            }
        }

        let should_start = !self.in_flight
            && self
                .last_started
                .is_none_or(|started| started.elapsed() >= POLL_INTERVAL);
        if should_start {
            self.in_flight = true;
            self.last_started = Some(Instant::now());
            let config = self.config.clone();
            let tx = self.tx.clone();
            thread::spawn(move || {
                let _ = tx.send(fetch_status(&config));
            });
        }

        self.status.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    fn response_body(observed_at: &str, stale: bool) -> String {
        format!(
            r#"{{
                "api_version":"v1",
                "generated_at":"2026-08-24T12:00:00Z",
                "freshness":{{"observed_at":"{observed_at}","stale_after_seconds":30,"stale":{stale}}},
                "agents":[
                    {{"session":"z-session","agent":"codex","tmux_state":"dead","liveness":"unknown"}},
                    {{"session":"a-session","pid":4242,"agent":"claude","tmux_state":"detached","liveness":"live","window_id":"window-1","correlation":{{"task_id":"42"}}}}
                ]
            }}"#
        )
    }

    fn start_server(body: String, delay: Duration) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = format!("http://{}", listener.local_addr().expect("server address"));
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept test request");
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            loop {
                let count = stream.read(&mut buffer).expect("read test request");
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            thread::sleep(delay);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(), body
            );
            stream
                .write_all(response.as_bytes())
                .expect("write test response");
            String::from_utf8_lossy(&request).into_owned()
        });
        (address, handle)
    }

    #[test]
    fn parses_sorts_and_deduplicates_agents() {
        let status = parse_status(&response_body("2026-08-24T12:00:00Z", false)).unwrap();
        assert_eq!(status.agents[0].session, "a-session");
        assert_eq!(status.agents[1].session, "z-session");
        assert_eq!(
            status.agents[0].correlation.as_ref().unwrap().task_id,
            Some("42".into())
        );
        assert_eq!(status.agents[0].pid, Some(4242));
    }

    #[test]
    fn correlation_prefers_pid_then_session_name() {
        let status = parse_status(&response_body("2026-08-24T12:00:00Z", false)).unwrap();
        assert_eq!(
            status.find_agent("z-session", 4242).unwrap().session,
            "a-session"
        );
        assert_eq!(
            status.find_agent("z-session", 9999).unwrap().session,
            "z-session"
        );
        assert!(status.find_agent("missing", 9999).is_none());
    }

    #[test]
    fn rejects_invalid_contract_version() {
        let body = response_body("2026-08-24T12:00:00Z", false).replace("\"v1\"", "\"v2\"");
        let error = parse_status(&body).unwrap_err();
        assert!(error.contains("unsupported api_version"));
    }

    #[test]
    fn sends_bearer_auth_and_gets_status() {
        let (url, server) =
            start_server(response_body("2026-08-24T12:00:00Z", false), Duration::ZERO);
        let mut source = OrchestratorSource::for_test(
            status_endpoint(&url),
            Some("secret-token".to_string()),
            Duration::from_secs(1),
        );
        assert!(source.poll().is_none());
        let request = server.join().expect("server result");
        assert!(request.starts_with("GET /v1/agents/status HTTP/1.1"));
        assert!(request
            .to_ascii_lowercase()
            .contains("authorization: bearer secret-token"));
    }

    #[test]
    fn timeout_and_http_errors_do_not_replace_last_good_status() {
        let (url, server) = start_server(
            response_body("2026-08-24T12:00:00Z", false),
            Duration::from_millis(200),
        );
        let mut source =
            OrchestratorSource::for_test(status_endpoint(&url), None, Duration::from_millis(20));
        assert!(source.poll().is_none());
        let _ = server.join();
        thread::sleep(Duration::from_millis(25));
        assert!(source.poll().is_none(), "a timeout must not fabricate data");
    }

    #[test]
    fn stale_responses_are_identified_without_removing_their_agents() {
        let status = parse_status(&response_body("2026-08-24T11:58:00Z", false)).unwrap();
        assert!(status.is_stale_at(
            chrono::DateTime::parse_from_rfc3339("2026-08-24T12:00:00Z")
                .unwrap()
                .into()
        ));
        let explicitly_stale = parse_status(&response_body("2026-08-24T12:00:00Z", true)).unwrap();
        assert!(explicitly_stale.is_stale_at(SystemTime::now()));
        assert_eq!(explicitly_stale.agents.len(), 2);
    }
}
