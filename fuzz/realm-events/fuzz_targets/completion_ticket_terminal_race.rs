#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    nexa_runtime::fuzz_completion_ticket_terminal_race(bytes);
});
