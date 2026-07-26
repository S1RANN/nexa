#![no_main]

use libfuzzer_sys::fuzz_target;

const MAX_INPUT: usize = 1 << 16;

fuzz_target!(|bytes: &[u8]| {
    if bytes.len() > MAX_INPUT {
        return;
    }
    let _ = nexa_migrate::parse_state_fixture(
        bytes,
        nexa_migrate::StateFixtureLimits {
            max_bytes: MAX_INPUT,
            max_objects: 1_024,
            max_fields: 4_096,
            max_string_bytes: MAX_INPUT,
        },
    );
});
