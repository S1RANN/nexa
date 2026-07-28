use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub type AnyError = Box<dyn std::error::Error>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FixtureCase {
    SyntheticPass,
    SyntheticH1Fail,
    SyntheticH2Fail,
    SyntheticH3Fail,
    SyntheticInvalid,
    SyntheticInconclusive,
    SyntheticNoPilot,
    SyntheticPilotNoBudget,
    SyntheticPilotBudget,
    SyntheticTerminalShortCircuit,
}

impl FixtureCase {
    pub const ALL: [Self; 10] = [
        Self::SyntheticPass,
        Self::SyntheticH1Fail,
        Self::SyntheticH2Fail,
        Self::SyntheticH3Fail,
        Self::SyntheticInvalid,
        Self::SyntheticInconclusive,
        Self::SyntheticNoPilot,
        Self::SyntheticPilotNoBudget,
        Self::SyntheticPilotBudget,
        Self::SyntheticTerminalShortCircuit,
    ];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::SyntheticPass => "synthetic-pass",
            Self::SyntheticH1Fail => "synthetic-h1-fail",
            Self::SyntheticH2Fail => "synthetic-h2-fail",
            Self::SyntheticH3Fail => "synthetic-h3-fail",
            Self::SyntheticInvalid => "synthetic-invalid",
            Self::SyntheticInconclusive => "synthetic-inconclusive",
            Self::SyntheticNoPilot => "synthetic-no-pilot",
            Self::SyntheticPilotNoBudget => "synthetic-pilot-no-budget",
            Self::SyntheticPilotBudget => "synthetic-pilot-budget",
            Self::SyntheticTerminalShortCircuit => "synthetic-terminal-short-circuit",
        }
    }

    pub fn parse(value: &str) -> Result<Self, AnyError> {
        Self::ALL
            .into_iter()
            .find(|case| case.name() == value)
            .ok_or_else(|| format!("unknown synthetic fixture `{value}`").into())
    }
}

#[must_use]
pub fn expected_decision(case: FixtureCase) -> &'static str {
    match case {
        FixtureCase::SyntheticH1Fail
        | FixtureCase::SyntheticH2Fail
        | FixtureCase::SyntheticH3Fail => "STOP",
        FixtureCase::SyntheticInvalid | FixtureCase::SyntheticTerminalShortCircuit => "INVALID",
        FixtureCase::SyntheticInconclusive => "UNVERIFIABLE_WITHIN_MVR",
        FixtureCase::SyntheticPilotNoBudget => "PROCEED_TO_PILOT",
        FixtureCase::SyntheticPilotBudget => "PROCEED_TO_GATE2_RFC",
        FixtureCase::SyntheticPass | FixtureCase::SyntheticNoPilot => "HOLD",
    }
}

#[must_use]
pub fn artifact_bundle(case: FixtureCase) -> Value {
    let invalid = matches!(
        case,
        FixtureCase::SyntheticInvalid | FixtureCase::SyntheticTerminalShortCircuit
    );
    let inconclusive = case == FixtureCase::SyntheticInconclusive;
    let h1 = if case == FixtureCase::SyntheticH1Fail {
        "FAIL"
    } else if invalid {
        "NOT_RUN_DUE_TO_TERMINAL_DECISION"
    } else if inconclusive {
        "INCONCLUSIVE"
    } else {
        "PASS"
    };
    let h2 = if case == FixtureCase::SyntheticH2Fail {
        "FAIL"
    } else if invalid {
        "NOT_RUN_DUE_TO_TERMINAL_DECISION"
    } else if inconclusive {
        "INCONCLUSIVE"
    } else {
        "PASS"
    };
    let h3 = if case == FixtureCase::SyntheticH3Fail {
        "FAIL"
    } else if invalid {
        "NOT_RUN_DUE_TO_TERMINAL_DECISION"
    } else if inconclusive {
        "INCONCLUSIVE"
    } else {
        "PASS"
    };
    let pilot = matches!(
        case,
        FixtureCase::SyntheticPilotNoBudget | FixtureCase::SyntheticPilotBudget
    );
    let budget = case == FixtureCase::SyntheticPilotBudget;
    let second_run = if invalid {
        "NOT_RUN_DUE_TO_TERMINAL_DECISION"
    } else {
        "PASS"
    };
    let replay = second_run;
    json!({
        "schema_version": 1,
        "synthetic": true,
        "formal_evidence_usable": false,
        "fixture": case.name(),
        "runs": {
            "formal-run-1": if invalid {"INVALID"} else if inconclusive {"INCONCLUSIVE"} else {"PASS"},
            "formal-run-2": second_run,
            "replay": replay
        },
        "gates": gate_artifacts(h1, h2, h3, invalid, inconclusive, pilot, budget),
        "decision": expected_decision(case),
        "receipt_recomputable": true,
        "structural_gaps": []
    })
}

#[allow(clippy::too_many_lines, clippy::fn_params_excessive_bools)]
fn gate_artifacts(
    h1: &str,
    h2: &str,
    h3: &str,
    invalid: bool,
    inconclusive: bool,
    pilot: bool,
    budget: bool,
) -> BTreeMap<&'static str, Value> {
    let apparatus =
        |name: &str| json!({"gate":name,"contract_status":"PASS","outcome":"PASS","failures":[]});
    let outcome = |name: &str, status: &str| json!({"gate":name,"contract_status":"PASS","outcome":status,"failures":[]});
    let h2_outcome = |name: &str, measurements: Value| {
        json!({
            "gate": name,
            "contract_status": "PASS",
            "outcome": h2,
            "failures": [],
            "metrics": {
                "derived_outcomes": [h2, h2, h2],
                "recorded_outcomes": [h2, h2, h2],
                "direct_contract_assertions": [true, true, true],
                "measurements": measurements
            }
        })
    };
    BTreeMap::from([
        (
            "history",
            json!({
                "gate": "history",
                "contract_status": "PASS",
                "outcome": "PASS",
                "failures": [],
                "metrics": {
                    "v2_6_recorded_decision_authorized": false,
                    "v2_6_archive_artifact_count": 337,
                    "v2_7_formal_run_1_status": "INVALID",
                    "v2_7_decision_usable": false,
                    "v2_7_bound_invalid_artifact_count": 5,
                    "v2_8_status": "SEMANTICALLY_INSUFFICIENT",
                    "v2_8_recorded_decision_authorized": false,
                    "v2_8_archive_artifact_count": 343,
                    "v2_8_correct_stable_core_failures": ["H1", "H2", "H3"],
                    "v2_9_status_publication_pending_f": true,
                    "published_status_experiment": "gate1-v2.8"
                }
            }),
        ),
        (
            "governance",
            json!({
                "gate": "governance",
                "contract_status": "PASS",
                "outcome": "PASS",
                "failures": [],
                "metrics": {
                    "formal_decision_evidence": {
                        "source": "FORMAL_RAW_COMPONENT_OUTCOMES_AND_COMPARISONS",
                        "decision_recomputed_from_raw": true,
                        "apparatus_statuses": ["PASS", "PASS", "PASS"],
                        "per_run_product_outcomes": {
                            "H1": ["FAIL", "FAIL", "FAIL"],
                            "H2": ["FAIL", "FAIL", "FAIL"],
                            "H3": ["FAIL", "FAIL", "FAIL"]
                        },
                        "aggregate_product_outcomes": {
                            "H1": "FAIL",
                            "H2": "FAIL",
                            "H3": "FAIL"
                        },
                        "signature_stable": {"H1": true, "H2": true, "H3": true},
                        "formal_semantic_comparisons": {"H1": "PASS", "H2": "PASS", "H3": "PASS"},
                        "replay_semantic_comparisons": {"H1": "PASS", "H2": "PASS", "H3": "PASS"},
                        "h2_allocation_comparisons": ["PASS", "PASS"],
                        "h2_performance_comparisons": ["INCONCLUSIVE", "INCONCLUSIVE"],
                        "stable_core_failures": ["H1", "H2", "H3"],
                        "decision_priority_branch": "stable_core_failure",
                        "stable_failure_dominates_inconclusive": true,
                        "stable_failure_priority_applied": true,
                        "structured_pivot_approved": false,
                        "derived_product_decision": "STOP"
                    }
                }
            }),
        ),
        ("environment", apparatus("environment")),
        ("process", apparatus("process")),
        (
            "validity",
            outcome(
                "validity",
                if invalid {
                    "INVALID"
                } else if inconclusive {
                    "INCONCLUSIVE"
                } else {
                    "PASS"
                },
            ),
        ),
        ("h1_equivalence", outcome("h1_equivalence", h1)),
        ("h1_metrics", outcome("h1_metrics", h1)),
        (
            "h2_configuration",
            h2_outcome(
                "h2_configuration",
                json!({
                    "configuration_counts": [32, 32, 32],
                    "execution_fingerprint_counts": [32, 32, 32]
                }),
            ),
        ),
        (
            "h2_cleanup",
            h2_outcome(
                "h2_cleanup",
                json!({
                    "cleanup_counts": [12, 12, 12],
                    "trigger_fingerprint_counts": [12, 12, 12]
                }),
            ),
        ),
        (
            "h2_invariants",
            h2_outcome("h2_invariants", json!({"violation_counts": [0, 0, 0]})),
        ),
        (
            "h2_allocations",
            h2_outcome(
                "h2_allocations",
                json!({
                    "observer_run_counts": [3, 3, 3],
                    "absolute_zero_verified": [true, true, true],
                    "baseline_subtracted": [false, false, false],
                    "count_semantics": "ABSOLUTE_GLOBAL_ALLOCATOR_DELTAS"
                }),
            ),
        ),
        (
            "h2_performance",
            h2_outcome(
                "h2_performance",
                json!({
                    "benchmark_process_counts": [3, 3, 3],
                    "warmup_samples": [100, 100, 100],
                    "timed_samples": [1000, 1000, 1000],
                    "process_sample_counts": [[1000, 1000, 1000], [1000, 1000, 1000], [1000, 1000, 1000]]
                }),
            ),
        ),
        ("h3_migration", outcome("h3_migration", h3)),
        ("h3_completion", outcome("h3_completion", h3)),
        ("h3_transaction", outcome("h3_transaction", h3)),
        (
            "comparison",
            outcome(
                "comparison",
                if invalid { "NOT_APPLICABLE" } else { "PASS" },
            ),
        ),
        (
            "replay",
            outcome(
                "replay",
                if invalid {
                    "NOT_RUN_DUE_TO_TERMINAL_DECISION"
                } else {
                    "PASS"
                },
            ),
        ),
        (
            "pilot",
            json!({"gate":"pilot","contract_status":"PASS","outcome":"PASS","metrics":{"committed":pilot},"failures":[]}),
        ),
        (
            "budget",
            json!({"gate":"budget","contract_status":"PASS","outcome":"PASS","metrics":{"approved":budget},"failures":[]}),
        ),
        (
            "workspace",
            json!({
                "gate": "workspace",
                "contract_status": "PASS",
                "outcome": "PASS",
                "failures": [],
                "metrics": {
                    "stable_failure_extraction_regression": {
                        "exercises_stable_failure_extraction": true,
                        "representative_subgate_outcomes": {
                            "H1": ["PASS", "PASS", "PASS"],
                            "H2": ["PASS", "PASS", "PASS"],
                            "H3": ["PASS", "PASS", "PASS"]
                        },
                        "per_run_product_outcomes": {
                            "H1": ["FAIL", "FAIL", "FAIL"],
                            "H2": ["FAIL", "FAIL", "FAIL"],
                            "H3": ["FAIL", "FAIL", "FAIL"]
                        },
                        "stable_core_failures": ["H1", "H2", "H3"],
                        "actual": "STOP",
                        "matched": true
                    }
                }
            }),
        ),
        ("artifact_hygiene", apparatus("artifact_hygiene")),
    ])
}
