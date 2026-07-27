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
    let apparatus = json!({"status":"PASS","failures":[]});
    let outcome = |status: &str| json!({"status":status,"failures":[]});
    BTreeMap::from([
        (
            "governance",
            json!({
                "status":"PASS",
                "failures":[],
                "metrics":{
                    "invalid_evidence_absent_from_ancestry":{"value":true},
                    "negative_matrix_passed":{"value":true},
                    "unique_authorization":{"value":true},
                    "thresholds_equivalent":{"value":true},
                    "frozen_inputs_valid":{"value":true}
                }
            }),
        ),
        (
            "history",
            json!({
                "status":"PASS",
                "failures":[],
                "metrics":{
                    "v2_2_sealed":{"value":true},
                    "version_count":{"value":5},
                    "all_historical_nodes_reach_current":{"value":true}
                }
            }),
        ),
        (
            "environment",
            json!({
                "status":"PASS",
                "failures":[],
                "metrics":{"qualification_status":{"value":"QUALIFIED"}}
            }),
        ),
        ("process_provenance", apparatus.clone()),
        (
            "validity",
            json!({
                "status": if invalid {
                    "INVALID"
                } else if inconclusive {
                    "INCONCLUSIVE"
                } else {
                    "PASS"
                },
                "failures":[],
                "metrics":{"strict_clean_checks":{"value":true}}
            }),
        ),
        (
            "h1",
            json!({
                "status":h1,
                "failures":[],
                "metrics":{"real_measurements":{"value":true}}
            }),
        ),
        (
            "h2_semantic",
            json!({
                "status":h2,
                "failures":[],
                "metrics":{
                    "scenario_count":{"value":96},
                    "per_run_scenario_count":{"value":32}
                }
            }),
        ),
        ("h2_allocations", outcome(h2)),
        ("h2_performance", outcome(h2)),
        ("h3_migration", outcome(h3)),
        ("h3_completion", outcome(h3)),
        ("h3_transaction", outcome(h3)),
        (
            "comparison",
            outcome(if invalid { "NOT_APPLICABLE" } else { "PASS" }),
        ),
        (
            "replay",
            outcome(if invalid {
                "NOT_RUN_DUE_TO_TERMINAL_DECISION"
            } else {
                "PASS"
            }),
        ),
        (
            "pilot",
            json!({"status":"PASS","commitment":if pilot {"COMMITTED"} else {"ABSENT"},"failures":[]}),
        ),
        (
            "budget",
            json!({"status":"PASS","approved":budget,"failures":[]}),
        ),
        (
            "workspace",
            json!({
                "status":"PASS",
                "failures":[],
                "metrics":{
                    "dual_contract_layers":{"value":true},
                    "milestone_semantics_valid":{"value":true},
                    "contract_satisfiability_passed":{"value":true},
                    "synthetic_git_chain_passed":{"value":true},
                    "prefreeze_closure_passed":{"value":true},
                    "output_isolation":{"value":true},
                    "contract_count":{"value":36},
                    "implementation_commit_valid":{"value":true}
                }
            }),
        ),
    ])
}
