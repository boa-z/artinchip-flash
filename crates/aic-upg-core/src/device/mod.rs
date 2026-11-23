//! Transport-independent UPG commands and upgrade execution.

mod commands;
mod engine;
mod events;
mod options;
mod plan;

pub use commands::UpgDevice;
pub use events::{BurnCallback, BurnEvent, BurnReport, ResetOutcome};
pub use options::{is_cancelled, upg_mode, upg_mode_name, BurnOptions};
pub use plan::{
    default_burn_parts, default_burn_target, format_plan_lines, plan_burn, validate_erase_plan,
    BurnPlan, BurnPlanComponent,
};

#[cfg(test)]
mod tests;
