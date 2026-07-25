#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    if let Ok(module) = nexa_bytecode::Module::decode(bytes) {
        let _ = nexa_verifier::verify(
            module,
            nexa_verifier::VerifierLimits {
                max_wcet_states: 4_096,
                ..nexa_verifier::VerifierLimits::default()
            },
        );
    }
});
