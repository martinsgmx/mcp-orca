//! Local policy model.

use serde::{Deserialize, Serialize};

use crate::config::Config;

/// Capability class for a tool/action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Read,
    WorkspaceWrite,
    Process,
    Export,
    Destructive,
}

/// Risk level for a tool/action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    Read,
    Low,
    Medium,
    High,
}

/// Policy decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    NeedsApproval,
    Deny,
}

/// Policy engine.
#[derive(Debug, Clone)]
pub struct Policy {
    config: Config,
}

impl Policy {
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    #[must_use]
    pub fn evaluate(&self, capability: Capability, risk: Risk) -> Decision {
        if self.config.policy.read_only && !matches!(capability, Capability::Read) {
            return Decision::Deny;
        }
        match capability {
            Capability::Read => Decision::Allow,
            Capability::WorkspaceWrite => {
                if self.config.policy.allow_workspace_writes {
                    Decision::Allow
                } else {
                    Decision::Deny
                }
            }
            Capability::Process => {
                if self.config.policy.allow_slicing {
                    Decision::Allow
                } else {
                    Decision::Deny
                }
            }
            Capability::Export => {
                if !self.config.policy.allow_export {
                    return Decision::Deny;
                }
                if matches!(risk, Risk::High) {
                    Decision::NeedsApproval
                } else {
                    Decision::Allow
                }
            }
            Capability::Destructive => {
                if !self.config.policy.allow_delete {
                    return Decision::Deny;
                }
                Decision::NeedsApproval
            }
        }
    }
}
