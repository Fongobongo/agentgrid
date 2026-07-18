//! Approval state machine lives in `agentgrid_common` so the control plane and
//! the ACP client share one definition. Re-exported here for backwards compat.
pub use agentgrid_common::approval::*;
