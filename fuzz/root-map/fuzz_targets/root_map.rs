#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    if let Ok(mut module) = nexa_bytecode::Module::decode(bytes) {
        if let Some(root) = module
            .functions
            .first_mut()
            .and_then(|function| function.root_maps.first_mut())
            .and_then(|root_map| root_map.bitmap.first_mut())
        {
            *root = !*root;
        }
        let _ = nexa_verifier::verify(module, nexa_verifier::VerifierLimits::default());
    }
});
