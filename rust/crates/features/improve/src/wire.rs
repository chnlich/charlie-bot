//! The internal improve API's wire names: the two route paths, the request fields and the response fields.
//!
//! The route module (`api`) and the CLI (`cli`) each read one side of this contract. Both sides name
//! these fields from here, so a renamed field cannot leave the client posting a key the route rejects.
//! The session field is the same key in the drafts payload, so it comes from the shared field owner. The
//! repository and base-branch fields are the same keys in the delegate payload and the run record, so the
//! store's task-record owner defines them.

pub use charliebot_cli_common::fields::SESSION_ID;
pub use charliebot_store::metadata::{BASE_BRANCH_FIELD, REPO_PATH_FIELD};

/// `improve start`'s route.
pub const START_PATH: &str = "/api/internal/improve";

/// `improve stop`'s route.
pub const STOP_PATH: &str = "/api/internal/improve/stop";

pub const BACKEND: &str = "backend";
pub const ITERATIONS: &str = "iterations";
pub const GOAL: &str = "goal";
pub const PLAN: &str = "plan";
pub const WORK_BRANCH: &str = "work_branch";
pub const MERGE_BACK: &str = "merge_back";
pub const STATUS: &str = "status";
pub const LOOP_ID: &str = "loop_id";
pub const GOAL_PATH: &str = "goal_path";
pub const PLAN_PATH: &str = "plan_path";
pub const CHILD_SESSION_ID: &str = "child_session_id";
