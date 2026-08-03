//! The trusted execution kernel (M5 K1) - the single sanctioned unsafe
//! module in the runtime.
//!
//! # Policy
//!
//! The workspace forbids `unsafe_code`; `nexa-runtime` alone lowers that
//! to `deny` so this one module can opt in explicitly, and `cargo xtask
//! repo-audit` fails if `unsafe` appears in any other `crates/` source.
//! Every `unsafe` block carries a `SAFETY:` justification grounded in an
//! invariant the verifier or the arena constructor has already proven -
//! the kernel removes *redundant re-checks* of proven facts, never a
//! semantic check. Fuel settlement, traps, GC reference validation, user
//! index bounds, write barriers, and reload epochs all stay exactly where
//! they were.
//!
//! # Trust boundary
//!
//! [`CheckedInterpreter`](crate::CheckedInterpreter) only executes
//! [`VerifiedModule`](nexa_verifier::VerifiedModule)s, and the verifier
//! proves for every instruction that its register operands are smaller
//! than the declared register count of the function it belongs to. Frames
//! are only pushed through [`FrameArena::push_call_at`], always with the
//! callee's declared register count, and that constructor enforces the
//! arena layout invariant this module relies on:
//!
//! > The top frame's register window is exactly the tail of the register
//! > store: `register_start + register_count == registers.len()`.
//!
//! (`push_call_at` resizes the store to precisely the new frame's end and
//! `pop` truncates back to `register_start`, so the invariant holds at
//! every instruction boundary.)
//!
//! Given a verified operand `register < register_count`, the access
//! `registers[register_start + register]` is therefore always in bounds.
//! Debug builds re-assert both facts on every access, and the WP36
//! differential gates, executable parity gate, and fuzz smoke corpus run
//! against debug assertions in CI.
#![allow(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

use crate::RuntimeValue;
use crate::frame::{FrameArena, FrameError};
use crate::interpreter::InterpreterError;

/// Reads one register of the current frame with the bounds re-checks the
/// verifier already discharged removed. Error mapping matches the checked
/// path exactly: any failure is `RegisterOutOfRange(register)`.
#[inline]
pub(crate) fn read_register(
    arena: &FrameArena,
    register: u16,
) -> Result<RuntimeValue, InterpreterError> {
    let (frames, registers) = arena.trusted_parts();
    let Some(frame) = frames.last() else {
        return Err(InterpreterError::RegisterOutOfRange(register));
    };
    debug_assert!(
        usize::from(register) < usize::from(frame.register_count),
        "verifier bounds every register operand below the function's register count"
    );
    debug_assert!(
        frame.register_start as usize + usize::from(frame.register_count) == registers.len(),
        "FrameArena keeps the top frame's window as the register-store tail"
    );
    // SAFETY: `register < frame.register_count` is proven by the verifier
    // for every operand of the executing function, and `push_call_at`
    // guarantees `register_start + register_count == registers.len()` for
    // the top frame, so the index is strictly below `registers.len()`.
    Ok(unsafe { *registers.get_unchecked(frame.register_start as usize + usize::from(register)) })
}

/// Writes one register of the current frame; the mirror of
/// [`read_register`] with the same invariants and error mapping.
#[inline]
pub(crate) fn write_register(
    arena: &mut FrameArena,
    register: u16,
    value: RuntimeValue,
) -> Result<(), InterpreterError> {
    let (frames, registers) = arena.trusted_parts_mut();
    let Some(frame) = frames.last() else {
        return Err(InterpreterError::RegisterOutOfRange(register));
    };
    debug_assert!(
        usize::from(register) < usize::from(frame.register_count),
        "verifier bounds every register operand below the function's register count"
    );
    debug_assert!(
        frame.register_start as usize + usize::from(frame.register_count) == registers.len(),
        "FrameArena keeps the top frame's window as the register-store tail"
    );
    let index = frame.register_start as usize + usize::from(register);
    // SAFETY: identical to `read_register` - verifier operand bound plus
    // the top-frame tail invariant put `index` strictly below
    // `registers.len()`.
    unsafe {
        *registers.get_unchecked_mut(index) = value;
    }
    Ok(())
}

/// Advances the current frame's program counter. The overflow check of
/// the safe path is dropped: the verifier bounds function code length far
/// below `u32::MAX` (the whole module must decode inside the wire byte
/// budget), and a hypothetical wrap would still be caught by the next
/// instruction fetch, which bounds `pc` against the function's code
/// before executing anything.
#[inline]
pub(crate) fn advance_pc(arena: &mut FrameArena) -> Result<(), InterpreterError> {
    let (frames, _) = arena.trusted_parts_mut();
    let Some(frame) = frames.last_mut() else {
        return Err(InterpreterError::ContinuationLimit(FrameError::NoFrame));
    };
    frame.pc = frame.pc.wrapping_add(1);
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::frame::{FrameArena, FrameLimits};
    use crate::{InterpreterError, RuntimeValue};

    #[test]
    fn kernel_register_access_matches_the_checked_arena() {
        let mut arena = FrameArena::new(FrameLimits::default());
        arena.push(3, 4).unwrap();
        super::write_register(&mut arena, 2, RuntimeValue::I32(41)).unwrap();
        assert_eq!(
            super::read_register(&arena, 2).unwrap(),
            RuntimeValue::I32(41)
        );
        assert_eq!(arena.register(2).unwrap(), RuntimeValue::I32(41));

        // A nested frame re-targets the window to the new tail.
        arena.push(4, 2).unwrap();
        super::write_register(&mut arena, 0, RuntimeValue::Bool(true)).unwrap();
        assert_eq!(
            super::read_register(&arena, 0).unwrap(),
            RuntimeValue::Bool(true)
        );
        assert_eq!(arena.register(0).unwrap(), RuntimeValue::Bool(true));
        // The parent frame's registers are untouched.
        arena.pop().unwrap();
        assert_eq!(
            super::read_register(&arena, 2).unwrap(),
            RuntimeValue::I32(41)
        );
    }

    #[test]
    fn kernel_access_without_a_frame_fails_like_the_checked_path() {
        let mut arena = FrameArena::new(FrameLimits::default());
        assert!(matches!(
            super::read_register(&arena, 0),
            Err(InterpreterError::RegisterOutOfRange(0))
        ));
        assert!(matches!(
            super::write_register(&mut arena, 1, RuntimeValue::Unit),
            Err(InterpreterError::RegisterOutOfRange(1))
        ));
        assert!(super::advance_pc(&mut arena).is_err());
    }

    #[test]
    fn kernel_pc_advance_matches_the_checked_path() {
        let mut arena = FrameArena::new(FrameLimits::default());
        arena.push(0, 1).unwrap();
        super::advance_pc(&mut arena).unwrap();
        super::advance_pc(&mut arena).unwrap();
        assert_eq!(arena.current().unwrap().pc, 2);
    }
}
