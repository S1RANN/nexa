//! Parser, validator, and Rust generator for Nexa's intentionally small state-machine format.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use nexa_core::StableId;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MachineSpec {
    pub name: String,
    pub states: Vec<StateSpec>,
    pub events: Vec<String>,
    pub guards: Vec<String>,
    pub resources: Vec<String>,
    pub invariants: Vec<InvariantSpec>,
    pub transitions: Vec<TransitionSpec>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateSpec {
    pub name: String,
    pub initial: bool,
    pub terminal: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransitionSpec {
    pub name: String,
    pub from: String,
    pub event: String,
    pub to: String,
    pub guards: Vec<String>,
    pub deltas: Vec<ResourceChange>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceChange {
    pub resource: String,
    pub amount: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InvariantSpec {
    Nonnegative {
        resource: String,
    },
    TerminalZero {
        resource: String,
    },
    StateRequires {
        state: String,
        resource: String,
        minimum: i64,
    },
}

impl InvariantSpec {
    #[must_use]
    pub fn name(&self) -> String {
        match self {
            Self::Nonnegative { resource } => format!("nonnegative::{resource}"),
            Self::TerminalZero { resource } => format!("terminal_zero::{resource}"),
            Self::StateRequires {
                state,
                resource,
                minimum,
            } => format!("state_requires::{state}::{resource}::{minimum}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpecError {
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for SpecError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.line == 0 {
            write!(formatter, "{}", self.message)
        } else {
            write!(formatter, "line {}: {}", self.line, self.message)
        }
    }
}

impl std::error::Error for SpecError {}

impl MachineSpec {
    pub fn parse(source: &str) -> Result<Self, Vec<SpecError>> {
        let mut errors = Vec::new();
        let mut machine_name = None;
        let mut states = Vec::new();
        let mut events = Vec::new();
        let mut guards = Vec::new();
        let mut resources = Vec::new();
        let mut invariants = Vec::new();
        let mut transitions = Vec::new();
        let mut saw_end = false;

        for (zero_based_line, raw_line) in source.lines().enumerate() {
            let line_number = zero_based_line + 1;
            let line = raw_line.split('#').next().unwrap_or_default().trim();
            if line.is_empty() {
                continue;
            }
            if saw_end {
                errors.push(SpecError {
                    line: line_number,
                    message: "content after `end`".into(),
                });
                continue;
            }

            let words = line.split_whitespace().collect::<Vec<_>>();
            match words.first().copied() {
                Some("machine") => {
                    if words.len() != 2 {
                        errors.push(usage(line_number, "machine <Name>"));
                    } else if machine_name.replace(words[1].to_owned()).is_some() {
                        errors.push(SpecError {
                            line: line_number,
                            message: "machine name declared more than once".into(),
                        });
                    }
                }
                Some("state") => match parse_state(&words, line_number) {
                    Ok(state) => states.push(state),
                    Err(error) => errors.push(error),
                },
                Some("event") => parse_single_name(&words, line_number, "event")
                    .map_or_else(|error| errors.push(error), |name| events.push(name)),
                Some("guard") => parse_single_name(&words, line_number, "guard")
                    .map_or_else(|error| errors.push(error), |name| guards.push(name)),
                Some("resource") => parse_single_name(&words, line_number, "resource")
                    .map_or_else(|error| errors.push(error), |name| resources.push(name)),
                Some("invariant") => match parse_invariant(&words, line_number) {
                    Ok(invariant) => invariants.push(invariant),
                    Err(error) => errors.push(error),
                },
                Some("transition") => match parse_transition(&words, line_number) {
                    Ok(transition) => transitions.push(transition),
                    Err(error) => errors.push(error),
                },
                Some("end") => {
                    if words.len() != 1 {
                        errors.push(usage(line_number, "end"));
                    }
                    saw_end = true;
                }
                Some(other) => errors.push(SpecError {
                    line: line_number,
                    message: format!("unknown declaration `{other}`"),
                }),
                None => {}
            }
        }

        if !saw_end {
            errors.push(SpecError {
                line: 0,
                message: "missing final `end`".into(),
            });
        }

        let Some(name) = machine_name else {
            errors.push(SpecError {
                line: 0,
                message: "missing `machine <Name>` declaration".into(),
            });
            return Err(errors);
        };

        let spec = Self {
            name,
            states,
            events,
            guards,
            resources,
            invariants,
            transitions,
        };
        errors.extend(spec.validation_errors());
        if errors.is_empty() {
            Ok(spec)
        } else {
            Err(errors)
        }
    }

    pub fn from_path(path: &Path) -> Result<Self, Vec<SpecError>> {
        match std::fs::read_to_string(path) {
            Ok(source) => Self::parse(&source),
            Err(error) => Err(vec![SpecError {
                line: 0,
                message: format!("could not read {}: {error}", path.display()),
            }]),
        }
    }

    #[must_use]
    pub fn initial_state(&self) -> &StateSpec {
        self.states
            .iter()
            .find(|state| state.initial)
            .expect("validated machine has one initial state")
    }

    #[must_use]
    pub fn transition_id(&self, transition: &TransitionSpec) -> StableId {
        StableId::from_name(&format!("{}::Transition::{}", self.name, transition.name))
    }

    #[must_use]
    pub fn state_id(&self, state: &StateSpec) -> StableId {
        StableId::from_name(&format!("{}::State::{}", self.name, state.name))
    }

    #[must_use]
    pub fn event_id(&self, event: &str) -> StableId {
        StableId::from_name(&format!("{}::Event::{event}", self.name))
    }

    #[must_use]
    pub fn invariant_id(&self, invariant: &InvariantSpec) -> StableId {
        StableId::from_name(&format!("{}::Invariant::{}", self.name, invariant.name()))
    }

    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn generate_rust(&self) -> String {
        let module_name = to_snake_case(&self.name);
        let mut output = String::new();
        writeln!(output, "// @generated by nexa-machine; do not edit.").unwrap();
        writeln!(output, "pub mod {module_name} {{").unwrap();
        writeln!(
            output,
            "    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]"
        )
        .unwrap();
        writeln!(output, "    pub enum State {{").unwrap();
        for state in &self.states {
            writeln!(output, "        {},", state.name).unwrap();
        }
        writeln!(output, "    }}").unwrap();
        writeln!(
            output,
            "    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]"
        )
        .unwrap();
        writeln!(output, "    pub enum Event {{").unwrap();
        for event in &self.events {
            writeln!(output, "        {event},").unwrap();
        }
        writeln!(output, "    }}").unwrap();
        writeln!(
            output,
            "    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]"
        )
        .unwrap();
        writeln!(output, "    pub enum Guard {{").unwrap();
        for guard in &self.guards {
            writeln!(output, "        {},", to_pascal_case(guard)).unwrap();
        }
        writeln!(output, "    }}").unwrap();
        self.write_generated_metadata(&mut output);
        writeln!(output, "    #[derive(Clone, Copy, Debug, PartialEq, Eq)]").unwrap();
        writeln!(output, "    pub struct ResourceDelta {{").unwrap();
        writeln!(output, "        pub resource: &'static str,").unwrap();
        writeln!(output, "        pub amount: i64,").unwrap();
        writeln!(output, "    }}").unwrap();
        writeln!(output, "    pub struct Outcome {{").unwrap();
        writeln!(output, "        pub state: State,").unwrap();
        writeln!(output, "        pub transition_id: u64,").unwrap();
        writeln!(output, "        pub deltas: &'static [ResourceDelta],").unwrap();
        writeln!(output, "    }}").unwrap();
        writeln!(output, "    #[derive(Clone, Copy, Debug, PartialEq, Eq)]").unwrap();
        writeln!(output, "    pub enum TransitionError {{").unwrap();
        writeln!(
            output,
            "        GuardRejected {{ guard: Guard, transition_id: u64 }},"
        )
        .unwrap();
        writeln!(
            output,
            "        Undefined {{ state: State, event: Event }},"
        )
        .unwrap();
        writeln!(output, "    }}").unwrap();
        writeln!(output, "    impl std::fmt::Display for TransitionError {{").unwrap();
        writeln!(
            output,
            "        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {{"
        )
        .unwrap();
        writeln!(output, "            match self {{").unwrap();
        writeln!(
            output,
            "                Self::GuardRejected {{ guard, .. }} => write!(formatter, \"guard `{{guard:?}}` rejected transition\"),"
        )
        .unwrap();
        writeln!(
            output,
            "                Self::Undefined {{ state, event }} => write!(formatter, \"transition is not defined for {{state:?}} + {{event:?}}\"),"
        )
        .unwrap();
        writeln!(output, "            }}").unwrap();
        writeln!(output, "        }}").unwrap();
        writeln!(output, "    }}").unwrap();
        writeln!(
            output,
            "    impl std::error::Error for TransitionError {{}}"
        )
        .unwrap();
        writeln!(output, "    pub fn apply(").unwrap();
        writeln!(output, "        state: State,").unwrap();
        writeln!(output, "        event: Event,").unwrap();
        writeln!(output, "        guard: impl Fn(Guard) -> bool,").unwrap();
        writeln!(output, "    ) -> Result<Outcome, TransitionError> {{").unwrap();
        writeln!(output, "        let _ = &guard;").unwrap();
        writeln!(output, "        match (state, event) {{").unwrap();

        for (index, transition) in self.transitions.iter().enumerate() {
            writeln!(
                output,
                "            (State::{}, Event::{}) => {{",
                transition.from, transition.event
            )
            .unwrap();
            for guard in &transition.guards {
                let guard_variant = to_pascal_case(guard);
                writeln!(
                    output,
                    "                if !guard(Guard::{guard_variant}) {{ return Err(TransitionError::GuardRejected {{ guard: Guard::{guard_variant}, transition_id: 0x{:016x} }}); }}",
                    self.transition_id(transition).0
                )
                .unwrap();
            }
            if transition.deltas.is_empty() {
                writeln!(
                    output,
                    "                const DELTAS_{index}: &[ResourceDelta] = &[];"
                )
                .unwrap();
            } else {
                writeln!(
                    output,
                    "                const DELTAS_{index}: &[ResourceDelta] = &["
                )
                .unwrap();
                for delta in &transition.deltas {
                    writeln!(
                        output,
                        "                    ResourceDelta {{ resource: \"{}\", amount: {} }},",
                        delta.resource, delta.amount
                    )
                    .unwrap();
                }
                writeln!(output, "                ];").unwrap();
            }
            writeln!(output, "                Ok(Outcome {{").unwrap();
            writeln!(
                output,
                "                    state: State::{},",
                transition.to
            )
            .unwrap();
            writeln!(
                output,
                "                    transition_id: 0x{:016x},",
                self.transition_id(transition).0
            )
            .unwrap();
            writeln!(output, "                    deltas: DELTAS_{index},").unwrap();
            writeln!(output, "                }})").unwrap();
            writeln!(output, "            }}").unwrap();
        }

        writeln!(
            output,
            "            (state, event) => Err(TransitionError::Undefined {{ state, event }}),"
        )
        .unwrap();
        writeln!(output, "        }}").unwrap();
        writeln!(output, "    }}").unwrap();
        writeln!(output, "}}").unwrap();
        output
    }

    #[allow(clippy::too_many_lines)]
    fn write_generated_metadata(&self, output: &mut String) {
        writeln!(output, "    pub const fn state_id(state: State) -> u64 {{").unwrap();
        writeln!(output, "        match state {{").unwrap();
        for state in &self.states {
            writeln!(
                output,
                "            State::{} => 0x{:016x},",
                state.name,
                self.state_id(state).0
            )
            .unwrap();
        }
        writeln!(output, "        }}").unwrap();
        writeln!(output, "    }}").unwrap();
        writeln!(output, "    pub const fn event_id(event: Event) -> u64 {{").unwrap();
        writeln!(output, "        match event {{").unwrap();
        for event in &self.events {
            writeln!(
                output,
                "            Event::{event} => 0x{:016x},",
                self.event_id(event).0
            )
            .unwrap();
        }
        writeln!(output, "        }}").unwrap();
        writeln!(output, "    }}").unwrap();
        writeln!(
            output,
            "    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]"
        )
        .unwrap();
        writeln!(output, "    pub enum Resource {{").unwrap();
        for resource in &self.resources {
            writeln!(output, "        {},", to_pascal_case(resource)).unwrap();
        }
        writeln!(output, "    }}").unwrap();
        writeln!(
            output,
            "    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]"
        )
        .unwrap();
        writeln!(output, "    pub enum Invariant {{").unwrap();
        for index in 0..self.invariants.len() {
            writeln!(output, "        I{index},").unwrap();
        }
        writeln!(output, "    }}").unwrap();
        writeln!(
            output,
            "    pub const fn invariant_id(invariant: Invariant) -> u64 {{"
        )
        .unwrap();
        writeln!(output, "        match invariant {{").unwrap();
        for (index, invariant) in self.invariants.iter().enumerate() {
            writeln!(
                output,
                "            Invariant::I{index} => 0x{:016x},",
                self.invariant_id(invariant).0
            )
            .unwrap();
        }
        writeln!(output, "        }}").unwrap();
        writeln!(output, "    }}").unwrap();
        writeln!(output, "    #[derive(Clone, Copy, Debug, PartialEq, Eq)]").unwrap();
        writeln!(output, "    pub struct InvariantViolation {{").unwrap();
        writeln!(output, "        pub invariant: Invariant,").unwrap();
        writeln!(output, "    }}").unwrap();
        writeln!(output, "    pub fn check_invariants(").unwrap();
        writeln!(output, "        state: State,").unwrap();
        writeln!(output, "        resource: impl Fn(Resource) -> i64,").unwrap();
        writeln!(output, "    ) -> Result<(), InvariantViolation> {{").unwrap();
        writeln!(output, "        let _ = &resource;").unwrap();
        let terminal_states = self
            .states
            .iter()
            .filter(|state| state.terminal)
            .collect::<Vec<_>>();
        if terminal_states.is_empty() {
            writeln!(output, "        let terminal = false;").unwrap();
        } else {
            write!(output, "        let terminal = matches!(state, ").unwrap();
            for (index, state) in terminal_states.iter().enumerate() {
                if index != 0 {
                    write!(output, " | ").unwrap();
                }
                write!(output, "State::{}", state.name).unwrap();
            }
            writeln!(output, ");").unwrap();
        }
        writeln!(output, "        let _ = state;").unwrap();
        writeln!(output, "        let _ = terminal;").unwrap();
        for (index, invariant) in self.invariants.iter().enumerate() {
            let condition = match invariant {
                InvariantSpec::Nonnegative { resource } => {
                    format!("resource(Resource::{}) < 0", to_pascal_case(resource))
                }
                InvariantSpec::TerminalZero { resource } => format!(
                    "terminal && resource(Resource::{}) != 0",
                    to_pascal_case(resource)
                ),
                InvariantSpec::StateRequires {
                    state,
                    resource,
                    minimum,
                } => format!(
                    "state == State::{state} && resource(Resource::{}) < {minimum}",
                    to_pascal_case(resource)
                ),
            };
            writeln!(
                output,
                "        if {condition} {{ return Err(InvariantViolation {{ invariant: Invariant::I{index} }}); }}"
            )
            .unwrap();
        }
        writeln!(output, "        Ok(())").unwrap();
        writeln!(output, "    }}").unwrap();
    }

    #[allow(clippy::too_many_lines)]
    fn validation_errors(&self) -> Vec<SpecError> {
        let mut errors = Vec::new();
        validate_unique_names(
            self.states.iter().map(|state| state.name.as_str()),
            "state",
            &mut errors,
        );
        validate_unique_names(self.events.iter().map(String::as_str), "event", &mut errors);
        validate_unique_names(self.guards.iter().map(String::as_str), "guard", &mut errors);
        validate_unique_names(
            self.resources.iter().map(String::as_str),
            "resource",
            &mut errors,
        );
        let mut invariant_names = BTreeSet::new();
        for invariant in &self.invariants {
            let name = invariant.name();
            if !invariant_names.insert(name.clone()) {
                errors.push(SpecError {
                    line: 0,
                    message: format!("duplicate invariant `{name}`"),
                });
            }
        }
        validate_unique_names(
            self.transitions
                .iter()
                .map(|transition| transition.name.as_str()),
            "transition",
            &mut errors,
        );

        let initial_count = self.states.iter().filter(|state| state.initial).count();
        if initial_count != 1 {
            errors.push(SpecError {
                line: 0,
                message: format!("expected exactly one initial state, found {initial_count}"),
            });
        }

        let state_names = self
            .states
            .iter()
            .map(|state| state.name.as_str())
            .collect::<BTreeSet<_>>();
        let terminal_states = self
            .states
            .iter()
            .filter(|state| state.terminal)
            .map(|state| state.name.as_str())
            .collect::<BTreeSet<_>>();
        let event_names = self
            .events
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let guard_names = self
            .guards
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let resource_names = self
            .resources
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        for invariant in &self.invariants {
            match invariant {
                InvariantSpec::Nonnegative { resource }
                | InvariantSpec::TerminalZero { resource } => {
                    if !resource_names.contains(resource.as_str()) {
                        errors.push(unknown("resource", resource, &invariant.name()));
                    }
                }
                InvariantSpec::StateRequires {
                    state, resource, ..
                } => {
                    if !state_names.contains(state.as_str()) {
                        errors.push(unknown("state", state, &invariant.name()));
                    }
                    if !resource_names.contains(resource.as_str()) {
                        errors.push(unknown("resource", resource, &invariant.name()));
                    }
                }
            }
        }
        let mut transition_keys = BTreeSet::new();

        for transition in &self.transitions {
            if !state_names.contains(transition.from.as_str()) {
                errors.push(unknown("source state", &transition.from, &transition.name));
            }
            if !state_names.contains(transition.to.as_str()) {
                errors.push(unknown("target state", &transition.to, &transition.name));
            }
            if !event_names.contains(transition.event.as_str()) {
                errors.push(unknown("event", &transition.event, &transition.name));
            }
            if terminal_states.contains(transition.from.as_str()) {
                errors.push(SpecError {
                    line: 0,
                    message: format!(
                        "transition `{}` leaves terminal state `{}`",
                        transition.name, transition.from
                    ),
                });
            }
            for guard in &transition.guards {
                if !guard_names.contains(guard.as_str()) {
                    errors.push(unknown("guard", guard, &transition.name));
                }
            }
            for delta in &transition.deltas {
                if !resource_names.contains(delta.resource.as_str()) {
                    errors.push(unknown("resource", &delta.resource, &transition.name));
                }
            }
            let key = (&transition.from, &transition.event);
            if !transition_keys.insert(key) {
                errors.push(SpecError {
                    line: 0,
                    message: format!(
                        "more than one transition handles state `{}` and event `{}`",
                        transition.from, transition.event
                    ),
                });
            }
        }

        let reachable = reachable_states(self);
        for state in &self.states {
            if !reachable.contains(state.name.as_str()) {
                errors.push(SpecError {
                    line: 0,
                    message: format!("state `{}` is unreachable", state.name),
                });
            }
        }
        errors
    }
}

fn parse_state(words: &[&str], line: usize) -> Result<StateSpec, SpecError> {
    if !(2..=4).contains(&words.len()) {
        return Err(usage(line, "state <Name> [initial] [terminal]"));
    }
    let flags = words[2..].iter().copied().collect::<BTreeSet<_>>();
    for flag in &flags {
        if !matches!(*flag, "initial" | "terminal") {
            return Err(SpecError {
                line,
                message: format!("unknown state flag `{flag}`"),
            });
        }
    }
    Ok(StateSpec {
        name: words[1].to_owned(),
        initial: flags.contains("initial"),
        terminal: flags.contains("terminal"),
    })
}

fn parse_single_name(words: &[&str], line: usize, kind: &str) -> Result<String, SpecError> {
    if words.len() != 2 {
        return Err(usage(line, &format!("{kind} <Name>")));
    }
    Ok(words[1].to_owned())
}

fn parse_transition(words: &[&str], line: usize) -> Result<TransitionSpec, SpecError> {
    if words.len() < 5 {
        return Err(usage(
            line,
            "transition <Id> <From> <Event> <To> [when=<Guard>] [delta=<Resource>:<SignedAmount>]",
        ));
    }
    let mut guards = Vec::new();
    let mut deltas = Vec::new();
    for option in &words[5..] {
        if let Some(guard) = option.strip_prefix("when=") {
            guards.push(guard.to_owned());
        } else if let Some(delta) = option.strip_prefix("delta=") {
            let Some((resource, amount)) = delta.split_once(':') else {
                return Err(SpecError {
                    line,
                    message: format!("invalid resource delta `{delta}`"),
                });
            };
            let Ok(amount) = amount.parse::<i64>() else {
                return Err(SpecError {
                    line,
                    message: format!("invalid signed amount in `{delta}`"),
                });
            };
            deltas.push(ResourceChange {
                resource: resource.to_owned(),
                amount,
            });
        } else {
            return Err(SpecError {
                line,
                message: format!("unknown transition option `{option}`"),
            });
        }
    }
    Ok(TransitionSpec {
        name: words[1].to_owned(),
        from: words[2].to_owned(),
        event: words[3].to_owned(),
        to: words[4].to_owned(),
        guards,
        deltas,
    })
}

fn parse_invariant(words: &[&str], line: usize) -> Result<InvariantSpec, SpecError> {
    match words {
        ["invariant", "nonnegative", resource] => Ok(InvariantSpec::Nonnegative {
            resource: (*resource).to_owned(),
        }),
        ["invariant", "terminal_zero", resource] => Ok(InvariantSpec::TerminalZero {
            resource: (*resource).to_owned(),
        }),
        ["invariant", "state_requires", state, resource, minimum] => {
            let minimum = minimum.parse::<i64>().map_err(|_| SpecError {
                line,
                message: format!("invalid invariant minimum `{minimum}`"),
            })?;
            Ok(InvariantSpec::StateRequires {
                state: (*state).to_owned(),
                resource: (*resource).to_owned(),
                minimum,
            })
        }
        _ => Err(usage(
            line,
            "invariant nonnegative <Resource> | invariant terminal_zero <Resource> | invariant state_requires <State> <Resource> <Minimum>",
        )),
    }
}

fn validate_unique_names<'a>(
    names: impl Iterator<Item = &'a str>,
    kind: &str,
    errors: &mut Vec<SpecError>,
) {
    let mut seen = BTreeSet::new();
    for name in names {
        if !is_identifier(name) {
            errors.push(SpecError {
                line: 0,
                message: format!("{kind} name `{name}` is not a valid identifier"),
            });
        }
        if !seen.insert(name) {
            errors.push(SpecError {
                line: 0,
                message: format!("duplicate {kind} `{name}`"),
            });
        }
    }
}

fn is_identifier(name: &str) -> bool {
    let mut characters = name.chars();
    matches!(characters.next(), Some(first) if first == '_' || first.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn reachable_states(spec: &MachineSpec) -> BTreeSet<&str> {
    let mut reachable = BTreeSet::from([spec.initial_state().name.as_str()]);
    loop {
        let before = reachable.len();
        for transition in &spec.transitions {
            if reachable.contains(transition.from.as_str()) {
                reachable.insert(transition.to.as_str());
            }
        }
        if before == reachable.len() {
            return reachable;
        }
    }
}

fn unknown(kind: &str, name: &str, transition: &str) -> SpecError {
    SpecError {
        line: 0,
        message: format!("transition `{transition}` refers to unknown {kind} `{name}`"),
    }
}

fn usage(line: usize, expected: &str) -> SpecError {
    SpecError {
        line,
        message: format!("expected `{expected}`"),
    }
}

fn to_snake_case(name: &str) -> String {
    let mut result = String::new();
    for (index, character) in name.chars().enumerate() {
        if character.is_ascii_uppercase() {
            if index != 0 {
                result.push('_');
            }
            result.push(character.to_ascii_lowercase());
        } else {
            result.push(character);
        }
    }
    result
}

fn to_pascal_case(name: &str) -> String {
    let mut result = String::new();
    let mut uppercase = true;
    for character in name.chars() {
        if character == '_' {
            uppercase = true;
        } else if uppercase {
            result.push(character.to_ascii_uppercase());
            uppercase = false;
        } else {
            result.push(character);
        }
    }
    result
}

/// Returns transition IDs and detects the vanishingly unlikely but normative collision case.
pub fn transition_id_map(spec: &MachineSpec) -> Result<BTreeMap<StableId, &str>, SpecError> {
    let mut ids = BTreeMap::new();
    for transition in &spec.transitions {
        let id = spec.transition_id(transition);
        if let Some(existing) = ids.insert(id, transition.name.as_str()) {
            return Err(SpecError {
                line: 0,
                message: format!(
                    "stable transition ID collision between `{existing}` and `{}`",
                    transition.name
                ),
            });
        }
    }
    Ok(ids)
}

/// Returns every stable machine ID and rejects collisions across ID categories.
pub fn stable_id_map(spec: &MachineSpec) -> Result<BTreeMap<StableId, String>, SpecError> {
    let mut ids = BTreeMap::new();
    for (id, name) in spec
        .states
        .iter()
        .map(|state| {
            (
                spec.state_id(state),
                format!("{}::State::{}", spec.name, state.name),
            )
        })
        .chain(spec.events.iter().map(|event| {
            (
                spec.event_id(event),
                format!("{}::Event::{event}", spec.name),
            )
        }))
        .chain(spec.transitions.iter().map(|transition| {
            (
                spec.transition_id(transition),
                format!("{}::Transition::{}", spec.name, transition.name),
            )
        }))
        .chain(spec.invariants.iter().map(|invariant| {
            (
                spec.invariant_id(invariant),
                format!("{}::Invariant::{}", spec.name, invariant.name()),
            )
        }))
    {
        if let Some(existing) = ids.insert(id, name.clone()) {
            return Err(SpecError {
                line: 0,
                message: format!("stable ID collision between `{existing}` and `{name}`"),
            });
        }
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::{MachineSpec, transition_id_map};

    const SIMPLE: &str = r"
machine Task
state Created initial
state Ready
state Completed terminal
event Start
event Finish
resource active_tasks
invariant nonnegative active_tasks
invariant terminal_zero active_tasks
transition TASK_CREATED_START_READY Created Start Ready delta=active_tasks:+1
transition TASK_READY_FINISH_COMPLETED Ready Finish Completed delta=active_tasks:-1
end
";

    #[test]
    fn parses_and_validates_a_machine() {
        let spec = MachineSpec::parse(SIMPLE).expect("valid spec");
        assert_eq!(spec.name, "Task");
        assert_eq!(spec.states.len(), 3);
        assert_eq!(spec.transitions.len(), 2);
        assert_eq!(transition_id_map(&spec).unwrap().len(), 2);
    }

    #[test]
    fn rejects_unreachable_state() {
        let source = SIMPLE.replace(
            "state Completed terminal",
            "state Completed terminal\nstate Orphan",
        );
        let errors = MachineSpec::parse(&source).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|error| error.message.contains("unreachable"))
        );
    }

    #[test]
    fn generated_rust_contains_stable_transition_ids() {
        let spec = MachineSpec::parse(SIMPLE).expect("valid spec");
        let generated = spec.generate_rust();
        assert!(generated.contains("pub enum State"));
        assert!(!generated.contains("TASK_CREATED"));
        assert!(generated.contains("transition_id: 0x"));
    }

    #[test]
    fn transition_ids_are_namespaced_by_machine() {
        let task = MachineSpec::parse(SIMPLE).expect("valid task spec");
        let other_source = SIMPLE
            .replace("machine Task", "machine Other")
            .replace("Task::", "Other::");
        let other = MachineSpec::parse(&other_source).expect("valid other spec");
        assert_ne!(
            task.transition_id(&task.transitions[0]),
            other.transition_id(&other.transitions[0])
        );
    }
}
