#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    if let Ok(source) = std::str::from_utf8(bytes) {
        let _ = nexa_contract::parse_contract(source);
    }
});
