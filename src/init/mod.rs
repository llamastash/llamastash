//! v2 init wizard, doctor diagnostic, and shared HF/GH fetch substrate.
//!
//! Submodules land across phases 0–5 of the v2 plan
//! (`docs/plans/2026-05-18-001-feat-init-wizard-doctor-pull-plan.md`).
//! Unit 2 ships `snapshot`; Unit 3 ships `detection` plus the wizard /
//! doctor / download module shells (their public surface is wired so
//! the CLI dispatcher compiles; bodies fill in across Units 9, 10, 13).

pub mod benchmark;
pub mod config_writer;
pub mod detection;
pub mod doctor;
pub mod download;
pub mod fetch;
pub mod fetch_policy;
pub mod hf_api;
pub mod install;
pub mod prompts;
pub mod recommender;
pub mod smoke;
pub mod snapshot;
pub mod wizard;
