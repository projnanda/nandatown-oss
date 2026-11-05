//! How many replicas an agent deploys with, and who may change that.
//!
//! OSS runs every agent at one replica: [`SingleReplica`] is the whole implementation here.
//! An edition that scales agents implements [`AgentScaling`] and is handed to
//! [`AppState::from_config_with_scaling`](crate::state::AppState::from_config_with_scaling) at
//! the composition root. It is a constructor argument rather than a late swap because the
//! build worker and the HITL dispatcher copy `AppState` while it is built, so a replacement
//! installed afterwards would never reach their deploys.

use std::sync::Arc;

use async_trait::async_trait;
use nasiko_hitl::HitlStore;
use nasiko_runtime::{ContainerRuntime, DeploymentSpec, DeploymentStatus};
use serde::Serialize;
use uuid::Uuid;

/// The replica range an agent deploys with. `min == max` is a fixed count.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct ReplicaBounds {
    pub min: u32,
    pub max: u32,
    /// Per-replica capacity hint for the autoscaler; `None` when not autoscaled or unset.
    pub target: Option<u32>,
}

impl ReplicaBounds {
    /// One replica, no autoscaling.
    pub const SINGLE: Self = Self {
        min: 1,
        max: 1,
        target: None,
    };

    pub fn is_autoscaled(&self) -> bool {
        self.max > self.min
    }
}

#[async_trait]
pub trait AgentScaling: Send + Sync {
    /// Starts one deploy of a registered agent. Call before its deploy env is built, which
    /// may reset the agent's `state_mode`.
    async fn begin_deploy(&self, agent_id: Uuid) -> Box<dyn AgentDeploy>;

    /// Whether an autoscaler owns the replica count, so a manual count above 0 would be undone.
    async fn is_autoscaled(&self, agent_id: Uuid) -> sqlx::Result<bool>;

    /// The agent just went from in-memory to durable state. Must return at once.
    fn agent_became_durable(&self, agent_id: Uuid);

    /// Called once at startup with the store every HITL pause goes through. May wrap it.
    fn observe_hitl_store(&self, store: Arc<dyn HitlStore>) -> Arc<dyn HitlStore>;
}

/// One deploy of one agent, from [`AgentScaling::begin_deploy`].
#[async_trait]
pub trait AgentDeploy: Send + Sync {
    /// Read right before the deploy env is built. An error fails the deploy.
    async fn bounds(&self) -> sqlx::Result<ReplicaBounds>;

    /// Used in place of [`ContainerRuntime::deploy`].
    async fn deploy(&self, spec: &DeploymentSpec) -> nasiko_runtime::Result<DeploymentStatus>;
}

/// The OSS behaviour: one replica per agent, no autoscaler.
pub struct SingleReplica {
    runtime: Arc<dyn ContainerRuntime>,
}

impl SingleReplica {
    pub fn new(runtime: Arc<dyn ContainerRuntime>) -> Self {
        Self { runtime }
    }
}

#[async_trait]
impl AgentScaling for SingleReplica {
    async fn begin_deploy(&self, _agent_id: Uuid) -> Box<dyn AgentDeploy> {
        Box::new(SingleReplicaDeploy {
            runtime: self.runtime.clone(),
        })
    }

    async fn is_autoscaled(&self, _agent_id: Uuid) -> sqlx::Result<bool> {
        Ok(false)
    }

    fn agent_became_durable(&self, _agent_id: Uuid) {}

    fn observe_hitl_store(&self, store: Arc<dyn HitlStore>) -> Arc<dyn HitlStore> {
        store
    }
}

struct SingleReplicaDeploy {
    runtime: Arc<dyn ContainerRuntime>,
}

#[async_trait]
impl AgentDeploy for SingleReplicaDeploy {
    async fn bounds(&self) -> sqlx::Result<ReplicaBounds> {
        Ok(ReplicaBounds::SINGLE)
    }

    async fn deploy(&self, spec: &DeploymentSpec) -> nasiko_runtime::Result<DeploymentStatus> {
        self.runtime.deploy(spec).await
    }
}
