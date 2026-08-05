use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::finalize::rustc_version;
use crate::{DynError, captured_stdout, git_output};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct BaselineCacheKey {
    baseline_commit: String,
    benchmark_source_hash: String,
    rustc_version: String,
    hostname: String,
}

impl BaselineCacheKey {
    pub(crate) fn load() -> Result<Self, DynError> {
        Ok(Self {
            baseline_commit: git_output(&["rev-parse", "performance-m5-baseline^{}"])?,
            benchmark_source_hash: git_output(&[
                "rev-parse",
                "performance-m5-baseline:tools/benchmark-v7",
            ])?,
            rustc_version: rustc_version()?,
            hostname: captured_stdout(&mut Command::new("hostname"), "hostname")?
                .trim()
                .to_owned(),
        })
    }

    pub(crate) fn matches_report(&self, report: &serde_json::Value) -> bool {
        serde_json::from_value::<Self>(report["cache_key"].clone())
            .is_ok_and(|observed| observed == *self)
    }
}

#[cfg(test)]
mod tests {
    use super::BaselineCacheKey;

    #[test]
    fn baseline_cache_key_is_exact_and_fail_closed() {
        let key = BaselineCacheKey {
            baseline_commit: "baseline".to_owned(),
            benchmark_source_hash: "source".to_owned(),
            rustc_version: "rustc".to_owned(),
            hostname: "host".to_owned(),
        };
        let mut report = serde_json::json!({"cache_key": key});
        assert!(key.matches_report(&report));
        report["cache_key"]["hostname"] = "other".into();
        assert!(!key.matches_report(&report));
        report["cache_key"]["extra"] = true.into();
        assert!(!key.matches_report(&report));
    }
}
