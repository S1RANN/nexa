#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    nexa_runtime::fuzz_migration_arena(bytes);
});
