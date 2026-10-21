//! `FlowRegistry`: Quản lý các instance `Orchestrator` theo `flow_id` và trích xuất
//! danh mục `TargetAgent` động kèm `description` cho Voice Agent & API.

use std::collections::BTreeMap;
use std::sync::Arc;

use orchestrator_agent_rs::{AgentMode, Orchestrator};

use crate::config::TargetAgent;
use crate::error::{TaskError, TaskResult};
use crate::types::AgentCatalogEntry;

#[derive(Clone)]
pub(crate) struct FlowRegistry {
    flows: BTreeMap<String, Arc<Orchestrator>>,
    catalog: Vec<AgentCatalogEntry>,
    default_target: TargetAgent,
}

impl FlowRegistry {
    pub(crate) fn build(
        flows: BTreeMap<String, Arc<Orchestrator>>,
        configured_default: Option<TargetAgent>,
    ) -> TaskResult<Self> {
        if flows.is_empty() {
            return Err(TaskError::config(
                "at least one Orchestrator flow must be registered before calling start()",
            ));
        }

        let mut catalog = Vec::new();
        for (flow_id, orch) in &flows {
            if flow_id.trim().is_empty() {
                return Err(TaskError::config("flow_id must not be empty"));
            }
            for agent_id in orch.agent_ids() {
                if let Some(def) = orch.agent(agent_id) {
                    if def.mode != AgentMode::Subagent {
                        let desc = if def.description.trim().is_empty() {
                            format!("Primary agent '{}' on flow '{}'", def.id, flow_id)
                        } else {
                            def.description.trim().to_string()
                        };
                        catalog.push(AgentCatalogEntry {
                            target: TargetAgent::new(flow_id.clone(), def.id.clone()),
                            description: desc,
                        });
                    }
                }
            }
        }

        if catalog.is_empty() {
            return Err(TaskError::config(
                "registered Orchestrator flows contain no root-capable (Primary/All) agents",
            ));
        }

        let default_target = match configured_default {
            Some(target) => {
                let exists = catalog.iter().any(|c| c.target == target);
                if !exists {
                    return Err(TaskError::config(format!(
                        "configured default_target '{}/{}' is not a root-capable agent in registered flows",
                        target.flow_id, target.agent_id
                    )));
                }
                target
            }
            None => catalog[0].target.clone(),
        };

        Ok(Self {
            flows,
            catalog,
            default_target,
        })
    }

    pub(crate) fn get_flow(&self, flow_id: &str) -> TaskResult<&Arc<Orchestrator>> {
        self.flows
            .get(flow_id)
            .ok_or_else(|| TaskError::not_found(format!("flow '{flow_id}' not found")))
    }

    pub(crate) fn catalog(&self) -> &[AgentCatalogEntry] {
        &self.catalog
    }

    /// Phân giải chuỗi `target_agent` do Voice Agent hoặc API truyền vào:
    /// - `None` hoặc rỗng -> dùng `default_target`
    /// - `"flow_id/agent_id"` -> khớp chính xác cả `flow_id` và `agent_id`
    /// - `"agent_id"` -> tìm trong `catalog` theo `agent_id`
    pub(crate) fn resolve_target(
        &self,
        raw: Option<&str>,
    ) -> TaskResult<(TargetAgent, Arc<Orchestrator>)> {
        let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
            let orch = self.get_flow(&self.default_target.flow_id)?.clone();
            return Ok((self.default_target.clone(), orch));
        };

        if let Some((flow_part, agent_part)) = s.split_once('/') {
            let candidate = TargetAgent::new(flow_part.trim(), agent_part.trim());
            if self.catalog.iter().any(|c| c.target == candidate) {
                let orch = self.get_flow(&candidate.flow_id)?.clone();
                return Ok((candidate, orch));
            }
        }

        if let Some(entry) = self.catalog.iter().find(|c| c.target.agent_id == s) {
            let orch = self.get_flow(&entry.target.flow_id)?.clone();
            return Ok((entry.target.clone(), orch));
        }

        let valid_list: Vec<String> = self
            .catalog
            .iter()
            .map(|c| {
                format!(
                    "'{}' (or '{}')",
                    c.target.agent_id,
                    c.target.canonical_key()
                )
            })
            .collect();
        Err(TaskError::invalid_input(format!(
            "unknown target_agent '{s}'. Available root agents: {}",
            valid_list.join(", ")
        )))
    }
}
