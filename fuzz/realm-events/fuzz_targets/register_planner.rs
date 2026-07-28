#![no_main]

use std::fmt::Write as _;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    if bytes.len() > 64 {
        return;
    }
    let arity = usize::from(bytes.first().copied().unwrap_or_default() % 8) + 1;
    let mut parameters = String::new();
    let mut arguments = String::new();
    let mut sum = String::new();
    for index in 0..arity {
        if index != 0 {
            parameters.push_str(", ");
            arguments.push_str(", ");
            sum.push_str(" + ");
        }
        let _ = write!(parameters, "p{index}: i32");
        let value = bytes.get(index + 1).copied().unwrap_or_default();
        let _ = write!(arguments, "{value}");
        let _ = write!(sum, "p{index}");
    }
    let source = format!(
        "fn sum({parameters}) -> i32 {{ return {sum}; }}
         fn call() -> i32 {{ return sum({arguments}); }}"
    );
    let _ = nexa_compiler::compile(&source);
});
