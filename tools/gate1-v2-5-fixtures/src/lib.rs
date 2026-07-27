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
    BTreeMap::from([
        ("history", apparatus("history")),
        ("governance", apparatus("governance")),
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
        ("h2_configuration", outcome("h2_configuration", h2)),
        ("h2_cleanup", outcome("h2_cleanup", h2)),
        ("h2_invariants", outcome("h2_invariants", h2)),
        ("h2_allocations", outcome("h2_allocations", h2)),
        ("h2_performance", outcome("h2_performance", h2)),
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
        ("workspace", apparatus("workspace")),
        ("artifact_hygiene", apparatus("artifact_hygiene")),
    ])
}
