use crate::manifest::{PackageId, SourceId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticStage {
    Source,
    Manifest,
    Policy,
    Entitlement,
    Compile,
    Verify,
    Load,
    Export,
    HandlerTrap,
    HandlerYield,
    HandlerWait,
    Reload,
    Release,
    Persistence,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageDiagnostic {
    pub package_id: Option<PackageId>,
    pub source_id: SourceId,
    pub stage: DiagnosticStage,
    pub message: String,
    pub source_start: Option<usize>,
    pub source_end: Option<usize>,
}
