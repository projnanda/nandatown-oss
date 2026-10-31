use chrono::{DateTime, Utc};
use nasiko_hitl::{HitlObserver, HitlRequest, ResumeStatus};
use opentelemetry::{
    KeyValue, global,
    metrics::{Counter, Histogram},
};
use std::time::Instant;

use super::agent_replicas::ReadyReplicas;
use super::agent_request::{AgentRequestGuard, AgentRequestMetrics};

/// OTel GenAI semantic convention attribute names.
/// Based on: https://opentelemetry.io/docs/specs/semconv/gen-ai/
pub mod attr {
    pub const GEN_AI_SYSTEM: &str = "gen_ai.system";
    pub const GEN_AI_OPERATION_NAME: &str = "gen_ai.operation.name";
    pub const GEN_AI_REQUEST_MODEL: &str = "gen_ai.request.model";
    pub const GEN_AI_RESPONSE_MODEL: &str = "gen_ai.response.model";
    pub const GEN_AI_RESPONSE_FINISH_REASONS: &str = "gen_ai.response.finish_reasons";
    pub const GEN_AI_USAGE_INPUT_TOKENS: &str = "gen_ai.usage.input_tokens";
    pub const GEN_AI_USAGE_OUTPUT_TOKENS: &str = "gen_ai.usage.output_tokens";
    pub const GEN_AI_TOKEN_TYPE: &str = "gen_ai.token.type";
    pub const GEN_AI_TOOL_NAME: &str = "gen_ai.tool.name";
    pub const GEN_AI_TOOL_CALL_ID: &str = "gen_ai.tool.call.id";

    // Nasiko-specific extensions for multi-agent flows
    pub const NASIKO_AGENT_ID: &str = "nasiko.agent.id";
    pub const NASIKO_AGENT_NAME: &str = "nasiko.agent.name";
    pub const NASIKO_FLOW_DEPTH: &str = "nasiko.flow.depth";
    pub const NASIKO_FLOW_ROOT_AGENT: &str = "nasiko.flow.root_agent";
    pub const NASIKO_FLOW_TRACE_ID: &str = "nasiko.flow.trace_id";
    pub const NASIKO_TEAM_ID: &str = "nasiko.team.id";

    // Human-in-the-loop pauses and resumes. Bare names, scoped by the instrument they ride on,
    // like the `reason` on `nasiko.flow.cascade_rejections`.
    pub const HITL_KIND: &str = "kind";
    pub const HITL_ORIGIN: &str = "origin";
    pub const HITL_OUTCOME: &str = "outcome";
}

/// Histogram buckets (seconds) for how long a human takes to answer a pause: a few seconds up to
/// the request's TTL (a week by default). The SDK's defaults are tuned for milliseconds and end at
/// 10,000, which would put every answer slower than about three hours in the overflow bucket.
const HUMAN_RESPONSE_BUCKETS_SECS: [f64; 15] = [
    5.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1_800.0, 3_600.0, 7_200.0, 21_600.0, 43_200.0,
    86_400.0, 259_200.0, 604_800.0,
];

/// Histogram buckets (seconds) for answer to terminal resume outcome: a hand-off to the agent is
/// sub-second when it goes well and runs up to the dispatcher's request timeout (five minutes) per
/// attempt when it does not. The SDK's defaults would put everything under five seconds in one
/// bucket.
const RESUME_BUCKETS_SECS: [f64; 11] = [
    0.1, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0,
];

/// Metrics following OTel GenAI semantic conventions.
#[derive(Clone)]
pub struct GenAiMetrics {
    pub token_usage: Counter<u64>,
    pub operation_duration: Histogram<f64>,
    pub agent_invocations: Counter<u64>,
    pub flow_depth: Histogram<u64>,
    pub cascade_rejections: Counter<u64>,
    pub hitl_pauses: Counter<u64>,
    pub hitl_resume_outcomes: Counter<u64>,
    pub hitl_human_response_duration: Histogram<f64>,
    pub hitl_resume_duration: Histogram<f64>,
    agent_requests: AgentRequestMetrics,
}

impl Default for GenAiMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl GenAiMetrics {
    pub fn new() -> Self {
        let meter = global::meter("nasiko.cp");

        let token_usage = meter
            .u64_counter("gen_ai.client.token.usage")
            .with_description("Token usage per LLM call")
            .build();

        let operation_duration = meter
            .f64_histogram("gen_ai.client.operation.duration")
            .with_description("Duration of GenAI operations in seconds")
            .with_unit("s")
            .build();

        let agent_invocations = meter
            .u64_counter("nasiko.agent.invocations")
            .with_description("Total agent invocations proxied through CP")
            .build();

        let flow_depth = meter
            .u64_histogram("nasiko.flow.depth")
            .with_description("Depth of multi-agent invocation chains")
            .build();

        let cascade_rejections = meter
            .u64_counter("nasiko.flow.cascade_rejections")
            .with_description("Requests rejected due to cascade limits")
            .build();

        let hitl_pauses = meter
            .u64_counter("nasiko.hitl.pauses")
            .with_description("HITL pauses detected, each one a hitl_requests row created")
            .build();

        let hitl_resume_outcomes = meter
            .u64_counter("nasiko.hitl.resume_outcomes")
            .with_description("Terminal outcome of resuming a paused task with the human's answer")
            .build();

        let hitl_human_response_duration = meter
            .f64_histogram("nasiko.hitl.human_response_duration")
            .with_description("Seconds from a HITL pause being recorded to the human answering it")
            .with_unit("s")
            .with_boundaries(HUMAN_RESPONSE_BUCKETS_SECS.to_vec())
            .build();

        let hitl_resume_duration = meter
            .f64_histogram("nasiko.hitl.resume_duration")
            .with_description("Seconds from the human's answer to the resume's terminal outcome")
            .with_unit("s")
            .with_boundaries(RESUME_BUCKETS_SECS.to_vec())
            .build();

        Self {
            token_usage,
            operation_duration,
            agent_invocations,
            flow_depth,
            cascade_rejections,
            hitl_pauses,
            hitl_resume_outcomes,
            hitl_human_response_duration,
            hitl_resume_duration,
            agent_requests: AgentRequestMetrics::new(&meter),
        }
    }

    /// Count a request to `agent_id` as in flight until the guard is dropped; see
    /// [`AgentRequestGuard`]. Recorded in addition to `record_invocation`, which stays
    /// name-keyed and orchestrator-only.
    pub fn begin_agent_request(&self, agent_id: &str) -> AgentRequestGuard {
        self.agent_requests.begin(agent_id)
    }

    /// The same per-agent load metrics, behind the seam that the orchestrator crates use for
    /// the agent calls they make themselves.
    pub fn agent_call_meter(&self) -> std::sync::Arc<dyn nasiko_types::agent_call::AgentCallMeter> {
        std::sync::Arc::new(self.agent_requests.clone())
    }

    /// Requests to `agent_id` (a UUID) in flight from this process right now.
    pub fn agent_inflight_now(&self, agent_id: &str) -> u64 {
        self.agent_requests.inflight_now(agent_id)
    }

    /// The per-agent ready-replica cache the in-flight band divides by; the server's background
    /// refresher (`agent_replicas::run`) keeps it current.
    pub fn agent_ready_replicas(&self) -> ReadyReplicas {
        self.agent_requests.ready_replicas()
    }

    pub fn record_tokens(
        &self,
        input_tokens: u64,
        output_tokens: u64,
        model: &str,
        agent_name: &str,
        team_id: &str,
    ) {
        let common = &[
            KeyValue::new(attr::GEN_AI_REQUEST_MODEL, model.to_string()),
            KeyValue::new(attr::NASIKO_AGENT_NAME, agent_name.to_string()),
            KeyValue::new(attr::NASIKO_TEAM_ID, team_id.to_string()),
        ];

        self.token_usage.add(
            input_tokens,
            &[
                common[0].clone(),
                common[1].clone(),
                common[2].clone(),
                KeyValue::new(attr::GEN_AI_TOKEN_TYPE, "input"),
            ],
        );

        self.token_usage.add(
            output_tokens,
            &[
                common[0].clone(),
                common[1].clone(),
                common[2].clone(),
                KeyValue::new(attr::GEN_AI_TOKEN_TYPE, "output"),
            ],
        );
    }

    pub fn record_operation(
        &self,
        duration_secs: f64,
        operation: &str,
        model: &str,
        agent_name: &str,
        team_id: &str,
    ) {
        self.operation_duration.record(
            duration_secs,
            &[
                KeyValue::new(attr::GEN_AI_OPERATION_NAME, operation.to_string()),
                KeyValue::new(attr::GEN_AI_REQUEST_MODEL, model.to_string()),
                KeyValue::new(attr::NASIKO_AGENT_NAME, agent_name.to_string()),
                KeyValue::new(attr::NASIKO_TEAM_ID, team_id.to_string()),
            ],
        );
    }

    pub fn record_invocation(&self, agent_name: &str, team_id: &str) {
        self.agent_invocations.add(
            1,
            &[
                KeyValue::new(attr::NASIKO_AGENT_NAME, agent_name.to_string()),
                KeyValue::new(attr::NASIKO_TEAM_ID, team_id.to_string()),
            ],
        );
    }

    pub fn record_flow_depth(&self, depth: u64, root_agent: &str) {
        self.flow_depth.record(
            depth,
            &[KeyValue::new(
                attr::NASIKO_FLOW_ROOT_AGENT,
                root_agent.to_string(),
            )],
        );
    }

    /// Record an MCP tool call's latency, tagged as a `gen_ai.operation.name =
    /// "execute_tool"` span per the GenAI semconv (reuses the shared
    /// `operation_duration` histogram rather than adding a dedicated one).
    pub fn record_tool_call(&self, duration_secs: f64, tool_name: &str, agent_id: &str) {
        self.operation_duration.record(
            duration_secs,
            &[
                KeyValue::new(attr::GEN_AI_OPERATION_NAME, "execute_tool"),
                KeyValue::new(attr::GEN_AI_TOOL_NAME, tool_name.to_string()),
                KeyValue::new(attr::NASIKO_AGENT_ID, agent_id.to_string()),
            ],
        );
    }

    pub fn record_cascade_rejection(&self, reason: &str, agent_name: &str) {
        self.cascade_rejections.add(
            1,
            &[
                KeyValue::new("reason", reason.to_string()),
                KeyValue::new(attr::NASIKO_AGENT_NAME, agent_name.to_string()),
            ],
        );
    }

    /// A task paused for a human and its `hitl_requests` row was just created. Keyed by agent id,
    /// not name: names are reused and renamed, ids are not.
    pub fn record_hitl_pause(&self, row: &HitlRequest) {
        self.hitl_pauses.add(
            1,
            &[
                hitl_agent(row),
                KeyValue::new(attr::HITL_KIND, row.kind.as_str()),
                hitl_origin(row),
            ],
        );
    }

    /// The resume of an answered request reached the terminal `outcome` at `finished_at`, which
    /// the dispatcher reports the moment it writes it. One call records the outcome and both
    /// durations, so each answered request is counted exactly once, at the end. `row` is the
    /// request as the dispatcher held it, which carries `created_at` and `resolved_at`.
    pub fn record_hitl_resume_finished(
        &self,
        row: &HitlRequest,
        outcome: ResumeStatus,
        finished_at: DateTime<Utc>,
    ) {
        let outcome = KeyValue::new(attr::HITL_OUTCOME, outcome.as_str());
        self.hitl_resume_outcomes
            .add(1, &[hitl_agent(row), hitl_origin(row), outcome.clone()]);

        // Every path that makes a row resumable stamps `resolved_at`, so it is always set here;
        // without it neither duration has a starting point.
        let Some(answered_at) = row.resolved_at else {
            return;
        };
        self.hitl_human_response_duration.record(
            seconds_between(row.created_at, answered_at),
            &[
                hitl_agent(row),
                KeyValue::new(attr::HITL_KIND, row.kind.as_str()),
                hitl_origin(row),
            ],
        );
        self.hitl_resume_duration.record(
            seconds_between(answered_at, finished_at),
            &[hitl_agent(row), hitl_origin(row), outcome],
        );
    }
}

/// The HITL crate reports pauses and outcomes through this seam (`nasiko_hitl::observer`), so every
/// writer of either is metered, including the MCP gateway's rows and the `mcp_tool` dispatcher.
impl HitlObserver for GenAiMetrics {
    fn pause_recorded(&self, row: &HitlRequest) {
        self.record_hitl_pause(row);
    }

    fn resume_finished(
        &self,
        row: &HitlRequest,
        outcome: ResumeStatus,
        finished_at: DateTime<Utc>,
    ) {
        self.record_hitl_resume_finished(row, outcome, finished_at);
    }
}

fn hitl_agent(row: &HitlRequest) -> KeyValue {
    KeyValue::new(attr::NASIKO_AGENT_ID, row.agent_id.to_string())
}

fn hitl_origin(row: &HitlRequest) -> KeyValue {
    KeyValue::new(attr::HITL_ORIGIN, row.origin.as_str())
}

/// Whole milliseconds between two timestamps as seconds, never negative: a clock step must not
/// put a negative sample into a duration histogram.
fn seconds_between(from: DateTime<Utc>, to: DateTime<Utc>) -> f64 {
    (to - from).num_milliseconds().max(0) as f64 / 1_000.0
}

/// Builder for creating a GenAI span on a proxied LLM request.
/// Produces a tracing span with OTel GenAI semantic convention attributes.
pub struct GenAiSpan {
    pub operation: String,
    pub system: String,
    pub model: String,
    pub agent_id: String,
    pub agent_name: String,
    pub flow_depth: u32,
    pub flow_trace_id: Option<String>,
    pub start: Instant,
}

impl GenAiSpan {
    pub fn new(operation: &str, system: &str, model: &str) -> Self {
        Self {
            operation: operation.to_string(),
            system: system.to_string(),
            model: model.to_string(),
            agent_id: String::new(),
            agent_name: String::new(),
            flow_depth: 0,
            flow_trace_id: None,
            start: Instant::now(),
        }
    }

    pub fn with_agent(mut self, id: &str, name: &str) -> Self {
        self.agent_id = id.to_string();
        self.agent_name = name.to_string();
        self
    }

    pub fn with_flow(mut self, depth: u32, trace_id: Option<String>) -> Self {
        self.flow_depth = depth;
        self.flow_trace_id = trace_id;
        self
    }

    /// Returns OTel-compatible attribute key-value pairs for this span.
    pub fn attributes(&self) -> Vec<KeyValue> {
        let mut attrs = vec![
            KeyValue::new(attr::GEN_AI_OPERATION_NAME, self.operation.clone()),
            KeyValue::new(attr::GEN_AI_SYSTEM, self.system.clone()),
            KeyValue::new(attr::GEN_AI_REQUEST_MODEL, self.model.clone()),
            KeyValue::new(attr::NASIKO_AGENT_ID, self.agent_id.clone()),
            KeyValue::new(attr::NASIKO_AGENT_NAME, self.agent_name.clone()),
            KeyValue::new(attr::NASIKO_FLOW_DEPTH, self.flow_depth as i64),
        ];

        if let Some(ref trace_id) = self.flow_trace_id {
            attrs.push(KeyValue::new(attr::NASIKO_FLOW_TRACE_ID, trace_id.clone()));
        }

        attrs
    }

    pub fn elapsed_secs(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: i64, millis: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(seconds, (millis * 1_000_000) as u32).expect("valid timestamp")
    }

    #[test]
    fn seconds_between_is_the_interval_in_seconds() {
        assert_eq!(seconds_between(at(1_000, 0), at(1_090, 500)), 90.5);
        assert_eq!(seconds_between(at(1_000, 250), at(1_000, 250)), 0.0);
    }

    #[test]
    fn seconds_between_is_never_negative() {
        assert_eq!(seconds_between(at(1_090, 0), at(1_000, 0)), 0.0);
    }
}
