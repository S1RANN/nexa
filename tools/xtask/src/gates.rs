use std::path::Path;

use crate::DynError;
use crate::evidence::validate_workspace_test_receipt;

pub(crate) fn legacy_after_workspace(
    root: &Path,
    implementation_commit: &str,
) -> Result<(), DynError> {
    validate_workspace_test_receipt(root, implementation_commit)?;

    crate::test_binding_after_workspace()?;
    crate::test_task_after_workspace()?;
    crate::fuzz_build()?;
    crate::allocation_observer_smoke()?;
    crate::snake_headless("smoke")?;
    crate::snake_headless("bench")?;
    crate::m2_audit()?;
    crate::test_diagnostics_after_workspace()?;
    crate::test_cli_commands()?;
    crate::editor_check()?;
    crate::test_generation_accounting()?;
    crate::test_candidate_freshness()?;
    crate::m3_audit()?;
    crate::m3r1_audit()?;
    crate::m3r2_audit()?;
    crate::m3r3_product_audit()
}

pub(crate) fn language_scale_after_workspace(
    root: &Path,
    implementation_commit: &str,
) -> Result<(), DynError> {
    validate_workspace_test_receipt(root, implementation_commit)?;
    crate::m4::finalize_after_workspace()?;
    crate::m4r1::record_regression_pass()?;
    crate::m4r1::finalize_after_workspace()
}

pub(crate) fn m5_after_workspace(
    root: &Path,
    implementation_commit: &str,
    force_bench: bool,
) -> Result<(), DynError> {
    validate_workspace_test_receipt(root, implementation_commit)?;
    crate::check_m5_gates_after_workspace(force_bench)
}
