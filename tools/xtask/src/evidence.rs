use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::finalize::rustc_version;
use crate::{DynError, git_output};

const WORKSPACE_TEST_COMMAND: &[&str] = &["cargo", "test", "--workspace", "--all-targets"];

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Receipt {
    schema: u32,
    kind: String,
    implementation_commit: String,
    source_tree: String,
    toolchain: String,
    command: Vec<String>,
    status: String,
}

impl Receipt {
    fn workspace_test(
        implementation_commit: String,
        source_tree: String,
        toolchain: String,
    ) -> Self {
        Self {
            schema: 1,
            kind: "workspace-test".to_owned(),
            implementation_commit,
            source_tree,
            toolchain,
            command: WORKSPACE_TEST_COMMAND
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect(),
            status: "PASS".to_owned(),
        }
    }

    fn is_workspace_test_for(
        &self,
        implementation_commit: &str,
        source_tree: &str,
        toolchain: &str,
        worktree_clean: bool,
    ) -> bool {
        self.schema == 1
            && self.kind == "workspace-test"
            && self.implementation_commit == implementation_commit
            && self.source_tree == source_tree
            && self.toolchain == toolchain
            && self.command
                == WORKSPACE_TEST_COMMAND
                    .iter()
                    .map(|argument| (*argument).to_owned())
                    .collect::<Vec<_>>()
            && self.status == "PASS"
            && worktree_clean
    }
}

pub(crate) fn workspace_test_receipt_path(root: &Path) -> PathBuf {
    root.join("target/nexa-artifacts/m5-finalize/workspace-test-receipt.json")
}

pub(crate) fn write_workspace_test_receipt(root: &Path) -> Result<Receipt, DynError> {
    let receipt = Receipt::workspace_test(
        git_output(&["rev-parse", "HEAD"])?,
        git_output(&["rev-parse", "HEAD^{tree}"])?,
        rustc_version()?,
    );
    write_receipt(&workspace_test_receipt_path(root), &receipt)?;
    Ok(receipt)
}

pub(crate) fn validate_workspace_test_receipt(
    root: &Path,
    implementation_commit: &str,
) -> Result<Receipt, DynError> {
    let path = workspace_test_receipt_path(root);
    let receipt: Receipt = serde_json::from_slice(&fs::read(&path).map_err(|error| {
        format!(
            "workspace test receipt {} is missing: {error}",
            path.display()
        )
    })?)?;
    let source_tree = git_output(&["rev-parse", "HEAD^{tree}"])?;
    let toolchain = rustc_version()?;
    let worktree_clean = git_output(&["status", "--porcelain"])?.is_empty();
    if !receipt.is_workspace_test_for(
        implementation_commit,
        &source_tree,
        &toolchain,
        worktree_clean,
    ) {
        return Err(
            "workspace test receipt is stale, malformed, from another toolchain, or the checkout became dirty"
                .into(),
        );
    }
    Ok(receipt)
}

fn write_receipt(path: &Path, receipt: &Receipt) -> Result<(), DynError> {
    fs::create_dir_all(path.parent().ok_or("receipt path has no parent")?)?;
    let temporary = path.with_extension("json.tmp");
    fs::write(
        &temporary,
        format!("{}\n", serde_json::to_string_pretty(receipt)?),
    )?;
    fs::rename(temporary, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Receipt;

    #[test]
    fn workspace_receipt_is_fail_closed_for_dirty_or_retargeted_sources() {
        let receipt =
            Receipt::workspace_test("head".to_owned(), "tree".to_owned(), "rustc".to_owned());
        assert!(receipt.is_workspace_test_for("head", "tree", "rustc", true));
        assert!(!receipt.is_workspace_test_for("other", "tree", "rustc", true));
        assert!(!receipt.is_workspace_test_for("head", "other", "rustc", true));
        assert!(!receipt.is_workspace_test_for("head", "tree", "other", true));
        assert!(!receipt.is_workspace_test_for("head", "tree", "rustc", false));
    }
}
