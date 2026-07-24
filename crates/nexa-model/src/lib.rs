//! Bounded exploration of generated Nexa machine specifications.

pub mod system;

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use nexa_core::{RawHandle, StableId, machine_invariant_hash};
use nexa_machine::{InvariantSpec, MachineSpec, TransitionSpec};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Snapshot {
    state: String,
    resources: BTreeMap<String, i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExplorationReport {
    pub machine: String,
    pub reachable_states: BTreeSet<String>,
    pub taken_transitions: BTreeSet<StableId>,
    pub guard_true_branches: BTreeSet<(StableId, String)>,
    pub guard_false_branches: BTreeSet<(StableId, String)>,
    pub guard_rejections: Vec<GuardRejection>,
    pub visited_snapshots: usize,
    pub truncated: bool,
    pub failures: Vec<ModelFailure>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExploreConfig {
    pub max_depth: usize,
    pub max_snapshots: usize,
    pub max_resource_amount: i64,
}

impl Default for ExploreConfig {
    fn default() -> Self {
        Self {
            max_depth: 256,
            max_snapshots: 100_000,
            max_resource_amount: 4,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelFailure {
    pub message: String,
    pub path: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuardRejection {
    pub transition_id: StableId,
    pub guard: String,
    pub preceding_true_guards: Vec<String>,
    pub state: String,
    pub resources: BTreeMap<String, i64>,
}

impl ExplorationReport {
    #[must_use]
    pub fn is_success(&self, spec: &MachineSpec) -> bool {
        self.failures.is_empty()
            && self.reachable_states.len() == spec.states.len()
            && self.taken_transitions.len() == spec.transitions.len()
            && spec.transitions.iter().all(|transition| {
                transition.guards.iter().all(|guard| {
                    let key = (spec.transition_id(transition), guard.clone());
                    self.guard_true_branches.contains(&key)
                        && self.guard_false_branches.contains(&key)
                })
            })
    }
}

#[derive(Clone, Debug)]
struct QueueEntry {
    snapshot: Snapshot,
    path: Vec<String>,
}

/// Exhaustively explores the finite state/resource space expressed by one validated specification.
#[must_use]
pub fn explore(spec: &MachineSpec) -> ExplorationReport {
    explore_with_config(spec, ExploreConfig::default())
}

/// Explores a specification with explicit bounds so malformed resource cycles cannot run forever.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn explore_with_config(spec: &MachineSpec, config: ExploreConfig) -> ExplorationReport {
    let initial = Snapshot {
        state: spec.initial_state().name.clone(),
        resources: spec
            .resources
            .iter()
            .map(|resource| (resource.clone(), 0))
            .collect(),
    };
    let mut report = ExplorationReport {
        machine: spec.name.clone(),
        ..ExplorationReport::default()
    };
    let mut seen = BTreeSet::from([initial.clone()]);
    let mut queue = VecDeque::from([QueueEntry {
        snapshot: initial,
        path: Vec::new(),
    }]);

    while let Some(entry) = queue.pop_front() {
        report.reachable_states.insert(entry.snapshot.state.clone());
        check_invariants(spec, &entry.snapshot, &entry.path, &mut report);
        let terminal = spec
            .states
            .iter()
            .find(|state| state.name == entry.snapshot.state)
            .is_some_and(|state| state.terminal);
        if terminal {
            for (resource, amount) in &entry.snapshot.resources {
                if *amount != 0 {
                    report.failures.push(ModelFailure {
                        message: format!(
                            "terminal state `{}` retains resource `{resource}` with amount {amount}",
                            entry.snapshot.state
                        ),
                        path: entry.path.clone(),
                    });
                }
            }
            continue;
        }
        if entry.path.len() >= config.max_depth {
            report.truncated = true;
            continue;
        }

        for transition in spec
            .transitions
            .iter()
            .filter(|transition| transition.from == entry.snapshot.state)
        {
            if !record_guard_paths(spec, transition, &entry.snapshot, &mut report) {
                continue;
            }
            let mut next = entry.snapshot.clone();
            next.state.clone_from(&transition.to);
            let mut path = entry.path.clone();
            path.push(transition.name.clone());
            let mut valid_resources = true;
            for delta in &transition.deltas {
                let value = next
                    .resources
                    .get_mut(&delta.resource)
                    .expect("validated transition resource");
                *value += delta.amount;
                if *value < 0 {
                    report.failures.push(ModelFailure {
                        message: format!(
                            "transition `{}` makes resource `{}` negative",
                            transition.name, delta.resource
                        ),
                        path: path.clone(),
                    });
                    valid_resources = false;
                } else if *value > config.max_resource_amount {
                    report.truncated = true;
                    valid_resources = false;
                }
            }
            report
                .taken_transitions
                .insert(spec.transition_id(transition));
            if valid_resources && !seen.contains(&next) && seen.len() >= config.max_snapshots {
                report.truncated = true;
            } else if valid_resources && seen.insert(next.clone()) {
                queue.push_back(QueueEntry {
                    snapshot: next,
                    path,
                });
            }
        }
    }
    report.visited_snapshots = seen.len();

    for state in &spec.states {
        if !report.reachable_states.contains(&state.name) {
            report.failures.push(ModelFailure {
                message: format!("state `{}` was not reached by exploration", state.name),
                path: Vec::new(),
            });
        }
    }
    for transition in &spec.transitions {
        if !report
            .taken_transitions
            .contains(&spec.transition_id(transition))
        {
            report.failures.push(ModelFailure {
                message: format!("transition `{}` was not taken", transition.name),
                path: Vec::new(),
            });
        }
    }
    report
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReferenceStep {
    pub transition_id: StableId,
    pub old_state: String,
    pub event: String,
    pub new_state: String,
    pub resource_deltas: Vec<nexa_machine::ResourceChange>,
    pub resources: BTreeMap<String, i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplayError {
    UndefinedTransition { state: String, event: String },
    GuardRejected(String),
    ResourceUnderflow(String),
}

/// Independent, string-based reference machine used for runtime differential replay.
#[derive(Clone, Debug)]
pub struct ReferenceMachine<'a> {
    spec: &'a MachineSpec,
    state: String,
    resources: BTreeMap<String, i64>,
}

impl<'a> ReferenceMachine<'a> {
    #[must_use]
    pub fn new(spec: &'a MachineSpec) -> Self {
        Self {
            spec,
            state: spec.initial_state().name.clone(),
            resources: spec
                .resources
                .iter()
                .map(|resource| (resource.clone(), 0))
                .collect(),
        }
    }

    pub fn apply(
        &mut self,
        event: &str,
        guard: impl Fn(&str) -> bool,
    ) -> Result<ReferenceStep, ReplayError> {
        let transition = self
            .spec
            .transitions
            .iter()
            .find(|transition| transition.from == self.state && transition.event == event)
            .ok_or_else(|| ReplayError::UndefinedTransition {
                state: self.state.clone(),
                event: event.to_owned(),
            })?;
        if let Some(rejected) = transition.guards.iter().find(|required| !guard(required)) {
            return Err(ReplayError::GuardRejected(rejected.clone()));
        }

        let mut resources = self.resources.clone();
        for delta in &transition.deltas {
            let value = resources
                .get_mut(&delta.resource)
                .expect("validated transition resource");
            *value += delta.amount;
            if *value < 0 {
                return Err(ReplayError::ResourceUnderflow(delta.resource.clone()));
            }
        }
        let step = ReferenceStep {
            transition_id: self.spec.transition_id(transition),
            old_state: self.state.clone(),
            event: event.to_owned(),
            new_state: transition.to.clone(),
            resource_deltas: transition.deltas.clone(),
            resources: resources.clone(),
        };
        self.state.clone_from(&transition.to);
        self.resources = resources;
        Ok(step)
    }

    #[must_use]
    pub fn state(&self) -> &str {
        &self.state
    }

    #[must_use]
    pub fn resources(&self) -> &BTreeMap<String, i64> {
        &self.resources
    }

    #[must_use]
    pub fn invariant_hash(&self, owner_scope: Option<RawHandle>) -> u64 {
        let resources = self
            .resources
            .iter()
            .map(|(name, amount)| (name.as_str(), *amount))
            .collect::<Vec<_>>();
        machine_invariant_hash(&self.spec.name, &self.state, owner_scope, &resources)
    }
}

fn record_guard_paths(
    spec: &MachineSpec,
    transition: &TransitionSpec,
    snapshot: &Snapshot,
    report: &mut ExplorationReport,
) -> bool {
    let transition_id = spec.transition_id(transition);
    let mut preceding_true_guards = Vec::new();
    for guard in &transition.guards {
        let value = guard_value(snapshot, guard);
        if value != Some(true) {
            report.guard_rejections.push(GuardRejection {
                transition_id,
                guard: guard.clone(),
                preceding_true_guards: preceding_true_guards.clone(),
                state: snapshot.state.clone(),
                resources: snapshot.resources.clone(),
            });
            report
                .guard_false_branches
                .insert((transition_id, guard.clone()));
        }
        if value == Some(false) {
            return false;
        }
        report
            .guard_true_branches
            .insert((transition_id, guard.clone()));
        preceding_true_guards.push(guard.clone());
    }
    true
}

fn guard_value(snapshot: &Snapshot, guard: &str) -> Option<bool> {
    match guard {
        "children_zero" => Some(
            snapshot
                .resources
                .get("transient_child")
                .copied()
                .unwrap_or(0)
                == 0
                && snapshot
                    .resources
                    .get("persistent_child")
                    .copied()
                    .unwrap_or(0)
                    == 0,
        ),
        "has_transient" => Some(
            snapshot
                .resources
                .get("transient_child")
                .copied()
                .unwrap_or(0)
                > 0,
        ),
        "has_persistent" => Some(
            snapshot
                .resources
                .get("persistent_child")
                .copied()
                .unwrap_or(0)
                > 0,
        ),
        _ => None,
    }
}

fn check_invariants(
    spec: &MachineSpec,
    snapshot: &Snapshot,
    path: &[String],
    report: &mut ExplorationReport,
) {
    let terminal = spec
        .states
        .iter()
        .find(|state| state.name == snapshot.state)
        .is_some_and(|state| state.terminal);
    for invariant in &spec.invariants {
        let violation = match invariant {
            InvariantSpec::Nonnegative { resource } => {
                (snapshot.resources[resource] < 0).then(|| {
                    format!(
                        "invariant `{}` failed with {}",
                        invariant.name(),
                        snapshot.resources[resource]
                    )
                })
            }
            InvariantSpec::TerminalZero { resource } => {
                (terminal && snapshot.resources[resource] != 0).then(|| {
                    format!(
                        "invariant `{}` failed in terminal state `{}`",
                        invariant.name(),
                        snapshot.state
                    )
                })
            }
            InvariantSpec::StateRequires {
                state,
                resource,
                minimum,
            } => (snapshot.state == *state && snapshot.resources[resource] < *minimum).then(|| {
                format!(
                    "invariant `{}` failed with {}",
                    invariant.name(),
                    snapshot.resources[resource]
                )
            }),
        };
        if let Some(message) = violation {
            report.failures.push(ModelFailure {
                message,
                path: path.to_vec(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use nexa_machine::MachineSpec;

    use super::{ExploreConfig, ReferenceMachine, explore, explore_with_config};

    #[test]
    fn task_machine_has_complete_transition_and_guard_coverage() {
        let source = include_str!("../../../specs/machines/task.machine.spec");
        let spec = MachineSpec::parse(source).expect("task spec is valid");
        let report = explore(&spec);
        assert!(report.is_success(&spec), "{:#?}", report.failures);
    }

    #[test]
    fn explorer_reports_terminal_resource_leaks() {
        let source = r"
machine Leaky
state Start initial
state Done terminal
event Finish
resource token
transition LEAK Start Finish Done delta=token:+1
end
";
        let spec = MachineSpec::parse(source).expect("machine syntax is valid");
        let report = explore(&spec);
        assert_eq!(report.failures.len(), 1);
        assert!(report.failures[0].message.contains("retains resource"));
    }

    #[test]
    fn explorer_bounds_an_unbounded_resource_cycle() {
        let source = r"
machine Growing
state Start initial
event Grow
resource items
transition GROW Start Grow Start delta=items:+1
end
";
        let spec = MachineSpec::parse(source).expect("machine syntax is valid");
        let report = explore_with_config(
            &spec,
            ExploreConfig {
                max_resource_amount: 2,
                ..ExploreConfig::default()
            },
        );
        assert!(report.truncated);
        assert!(report.failures.is_empty());
    }

    #[test]
    fn reference_machine_rejects_illegal_events_without_mutation() {
        let spec = MachineSpec::parse(include_str!("../../../specs/machines/task.machine.spec"))
            .expect("task spec is valid");
        let mut model = ReferenceMachine::new(&spec);
        assert!(model.apply("Poll", |_| true).is_err());
        assert_eq!(model.state(), "Created");
        assert_eq!(model.resources()["task_slot"], 0);
    }

    #[test]
    fn guard_rejections_preserve_short_circuit_order_and_state() {
        let spec = MachineSpec::parse(include_str!("../../../specs/machines/task.machine.spec"))
            .expect("task spec is valid");
        let report = explore(&spec);
        let admission = spec
            .transitions
            .iter()
            .find(|transition| transition.event == "Admit")
            .unwrap();
        let rejections = report
            .guard_rejections
            .iter()
            .filter(|rejection| rejection.transition_id == spec.transition_id(admission))
            .collect::<Vec<_>>();
        assert_eq!(rejections.len(), 2);
        assert!(rejections[0].preceding_true_guards.is_empty());
        assert_eq!(rejections[1].preceding_true_guards, ["owner_scope_valid"]);
        assert!(
            rejections
                .iter()
                .all(|rejection| rejection.state == "Created"
                    && rejection.resources["task_slot"] == 0)
        );
    }
}
