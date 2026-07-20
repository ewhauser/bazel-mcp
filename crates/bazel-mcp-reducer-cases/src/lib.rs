//! Manifest-driven live and replay validation for reducer integration cases.

#![forbid(unsafe_code)]

mod manifest;
mod mcp;
mod replay;
mod sanitize;
mod semantics;

pub use manifest::{
    AbsentExpectation, ArtifactExpectation, CaseExpectation, CaseManifest, DiagnosticExpectation,
    EvidenceSpec, LoadedCase, ProvenanceSpec, ReplaySpec, discover_cases, find_repository_root,
    schema,
};
pub use mcp::{LiveOptions, LiveRun, run_live_case};
pub use replay::{
    ReplayOutput, replay_with_evidence, verify_case_contract, verify_case_evidence,
    verify_recorded_case,
};
pub use sanitize::{sanitize_binary, sanitize_text, verify_sanitized_evidence};
pub use semantics::{
    CaseObservation, VerificationFailure, observe_replay, verify_expectations,
    verify_live_replay_parity,
};

pub(crate) const CASE_SCHEMA_VERSION: u32 = 1;
