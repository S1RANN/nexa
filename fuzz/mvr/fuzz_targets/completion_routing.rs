#![no_main]

use libfuzzer_sys::fuzz_target;
use nexa_runtime::model_adapter::{RealmV4RoutingRuntimeAdapter, RealmV4RoutingRuntimeEvent};

fuzz_target!(|bytes: &[u8]| {
    if bytes.len() > 64 {
        return;
    }
    let mut runtime = RealmV4RoutingRuntimeAdapter::new();
    for byte in bytes.iter().copied().take(32) {
        let event = match byte % 5 {
            0 => RealmV4RoutingRuntimeEvent::CompleteA,
            1 => RealmV4RoutingRuntimeEvent::CompleteB,
            2 => RealmV4RoutingRuntimeEvent::RollbackA,
            3 => RealmV4RoutingRuntimeEvent::CommitA,
            _ => RealmV4RoutingRuntimeEvent::ActivationFaultA,
        };
        let _ = runtime.apply(event);
        let _ = runtime.snapshot();
    }
});
