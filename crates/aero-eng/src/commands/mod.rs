//! Built-in engineering commands — single source of truth.
//!
//! Every command is defined in this module via [`register_command!`] and
//! returned by [`all()`] in registration order (which is also the help /
//! completion order). [`CommandRegistry::collect`] builds the registry from
//! [`all()`]; binaries can also compose their own registry from
//! [`CommandRegistry::from_commands`] (Path A seam).

mod audit;
mod bench;
mod check;
mod completion;
mod dashboard;
mod doctor;
pub(crate) mod gate;
mod network;
mod skill;

use crate::command::Command;

/// All built-in commands, in registration order (help output order).
#[must_use]
pub(crate) fn all() -> Vec<Box<dyn Command>> {
    vec![
        Box::new(check::Check_),
        Box::new(gate::Gate_),
        Box::new(check::Test_),
        Box::new(check::Integration_),
        Box::new(skill::Skill_),
        Box::new(doctor::Doctor_),
        Box::new(completion::Completion_),
        Box::new(network::Network_),
        Box::new(audit::AuditProvisionCheck_),
        Box::new(bench::Bench_),
        Box::new(dashboard::Dashboard_),
    ]
}
