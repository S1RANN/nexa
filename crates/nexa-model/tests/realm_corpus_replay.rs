use nexa_runtime::model_adapter::{RealmRuntimeModelAdapter, RuntimeRealmEvent};

const CORPORA: [(&str, &[u8]); 6] = [
    (
        "waiting-reload",
        include_bytes!("../../../fuzz/realm-events/corpus/realm_event_sequence/waiting-reload"),
    ),
    (
        "rollback-then-reload",
        include_bytes!(
            "../../../fuzz/realm-events/corpus/realm_event_sequence/rollback-then-reload"
        ),
    ),
    (
        "activation-fault-recovery",
        include_bytes!(
            "../../../fuzz/realm-events/corpus/realm_event_sequence/activation-fault-recovery"
        ),
    ),
    (
        "repeated-completion",
        include_bytes!(
            "../../../fuzz/realm-events/corpus/realm_event_sequence/repeated-completion"
        ),
    ),
    (
        "repeated-spawn",
        include_bytes!("../../../fuzz/realm-events/corpus/realm_event_sequence/repeated-spawn"),
    ),
    (
        "drop-then-call",
        include_bytes!("../../../fuzz/realm-events/corpus/realm_event_sequence/drop-then-call"),
    ),
];

#[test]
fn committed_realm_event_corpora_replay_against_the_real_runtime() {
    for (name, bytes) in CORPORA {
        let mut runtime = RealmRuntimeModelAdapter::default();
        let mut last_attempts = 0;
        for byte in bytes
            .iter()
            .copied()
            .filter(|byte| !byte.is_ascii_whitespace())
        {
            let _ = runtime.apply(event(byte));
            let snapshot = runtime.snapshot();
            let attempts = runtime.invocation_counters().total();
            assert!(
                attempts >= last_attempts,
                "{name}: Runtime attempt counters regressed"
            );
            last_attempts = attempts;
            let state = runtime.state_fingerprint();
            assert!(state.ledger.tasks <= 1, "{name}: Task ledger overflow");
            assert!(
                state.ledger.scheduler_tokens <= 1,
                "{name}: scheduler ledger overflow"
            );
            assert!(
                state.ledger.requests <= 1,
                "{name}: request ledger overflow"
            );
            assert!(
                state.ledger.completion_reservations <= 1,
                "{name}: completion ledger overflow"
            );
            assert!(
                runtime.invariants_hold(),
                "{name}: real Runtime invariant failed at {snapshot:?}"
            );
        }
    }
}

const fn event(byte: u8) -> RuntimeRealmEvent {
    match byte % 9 {
        0 => RuntimeRealmEvent::Spawn,
        1 => RuntimeRealmEvent::Poll,
        2 => RuntimeRealmEvent::CompleteRequest,
        3 => RuntimeRealmEvent::Cancel,
        4 => RuntimeRealmEvent::RestartReload,
        5 => RuntimeRealmEvent::MigrationFailure,
        6 => RuntimeRealmEvent::ActivationFailure,
        7 => RuntimeRealmEvent::LateCompletion,
        8 => RuntimeRealmEvent::RealmDrop,
        _ => unreachable!(),
    }
}
