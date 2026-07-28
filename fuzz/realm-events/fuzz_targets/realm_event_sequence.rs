#![no_main]

use libfuzzer_sys::fuzz_target;
use nexa_runtime::model_adapter::{RealmRuntimeModelAdapter, RuntimeRealmEvent};

fuzz_target!(|bytes: &[u8]| {
    if bytes.len() > 128 {
        return;
    }
    let mut runtime = RealmRuntimeModelAdapter::default();
    for byte in bytes.iter().copied().take(64) {
        let event = match byte % 9 {
            0 => RuntimeRealmEvent::Spawn,
            1 => RuntimeRealmEvent::Poll,
            2 => RuntimeRealmEvent::CompleteRequest,
            3 => RuntimeRealmEvent::Cancel,
            4 => RuntimeRealmEvent::RestartReload,
            5 => RuntimeRealmEvent::MigrationFailure,
            6 => RuntimeRealmEvent::ActivationFailure,
            7 => RuntimeRealmEvent::LateCompletion,
            _ => RuntimeRealmEvent::RealmDrop,
        };
        let _ = runtime.apply(event);
        let _ = runtime.snapshot();
        assert!(runtime.invariants_hold());
    }
});
