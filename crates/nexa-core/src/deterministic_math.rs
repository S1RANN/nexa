//! Platform-independent scalar floating-point operations used by the VM and reference models.

use crate::{CANONICAL_NAN_F32_BITS, CANONICAL_NAN_F64_BITS};

/// Replaces every `f32` NaN payload and sign with Nexa's canonical quiet NaN.
#[must_use]
pub fn canonicalize_nan_f32(value: f32) -> f32 {
    if value.is_nan() {
        f32::from_bits(CANONICAL_NAN_F32_BITS)
    } else {
        value
    }
}

/// Replaces every `f64` NaN payload and sign with Nexa's canonical quiet NaN.
#[must_use]
pub fn canonicalize_nan_f64(value: f64) -> f64 {
    if value.is_nan() {
        f64::from_bits(CANONICAL_NAN_F64_BITS)
    } else {
        value
    }
}

/// Deterministic `f32` floor using Nexa's pinned math backend.
#[must_use]
pub fn floor_f32(value: f32) -> f32 {
    canonicalize_nan_f32(libm::floorf(value))
}

/// Deterministic `f64` floor using Nexa's pinned math backend.
#[must_use]
pub fn floor_f64(value: f64) -> f64 {
    canonicalize_nan_f64(libm::floor(value))
}

/// Deterministic `f32` ceiling using Nexa's pinned math backend.
#[must_use]
pub fn ceil_f32(value: f32) -> f32 {
    canonicalize_nan_f32(libm::ceilf(value))
}

/// Deterministic `f64` ceiling using Nexa's pinned math backend.
#[must_use]
pub fn ceil_f64(value: f64) -> f64 {
    canonicalize_nan_f64(libm::ceil(value))
}

/// Deterministic `f32` rounding using Nexa's pinned math backend.
#[must_use]
pub fn round_f32(value: f32) -> f32 {
    canonicalize_nan_f32(libm::roundf(value))
}

/// Deterministic `f64` rounding using Nexa's pinned math backend.
#[must_use]
pub fn round_f64(value: f64) -> f64 {
    canonicalize_nan_f64(libm::round(value))
}

/// Deterministic `f32` square root using Nexa's pinned math backend.
#[must_use]
pub fn sqrt_f32(value: f32) -> f32 {
    if value.is_nan() || value < 0.0 {
        return f32::from_bits(CANONICAL_NAN_F32_BITS);
    }
    canonicalize_nan_f32(libm::sqrtf(value))
}

/// Deterministic `f64` square root using Nexa's pinned math backend.
#[must_use]
pub fn sqrt_f64(value: f64) -> f64 {
    if value.is_nan() || value < 0.0 {
        return f64::from_bits(CANONICAL_NAN_F64_BITS);
    }
    canonicalize_nan_f64(libm::sqrt(value))
}

/// Deterministic `f32` sine using Nexa's pinned math backend.
#[must_use]
pub fn sin_f32(value: f32) -> f32 {
    if !value.is_finite() {
        return f32::from_bits(CANONICAL_NAN_F32_BITS);
    }
    canonicalize_nan_f32(libm::sinf(value))
}

/// Deterministic `f64` sine using Nexa's pinned math backend.
#[must_use]
pub fn sin_f64(value: f64) -> f64 {
    if !value.is_finite() {
        return f64::from_bits(CANONICAL_NAN_F64_BITS);
    }
    canonicalize_nan_f64(libm::sin(value))
}

/// Deterministic `f32` cosine using Nexa's pinned math backend.
#[must_use]
pub fn cos_f32(value: f32) -> f32 {
    if !value.is_finite() {
        return f32::from_bits(CANONICAL_NAN_F32_BITS);
    }
    canonicalize_nan_f32(libm::cosf(value))
}

/// Deterministic `f64` cosine using Nexa's pinned math backend.
#[must_use]
pub fn cos_f64(value: f64) -> f64 {
    if !value.is_finite() {
        return f64::from_bits(CANONICAL_NAN_F64_BITS);
    }
    canonicalize_nan_f64(libm::cos(value))
}

#[cfg(test)]
mod tests {
    use super::{
        ceil_f32, ceil_f64, cos_f32, cos_f64, floor_f32, floor_f64, round_f32, round_f64, sin_f32,
        sin_f64, sqrt_f32, sqrt_f64,
    };
    use crate::{CANONICAL_NAN_F32_BITS, CANONICAL_NAN_F64_BITS};

    #[test]
    fn f32_special_values_have_canonical_bits() {
        let negative_zero = f32::from_bits(0x8000_0000);
        let subnormal = f32::from_bits(1);
        let signaling_nan = f32::from_bits(0x7f80_0001);
        let negative_nan = f32::from_bits(0xffc0_1234);

        assert_eq!(floor_f32(negative_zero).to_bits(), negative_zero.to_bits());
        assert_eq!(ceil_f32(negative_zero).to_bits(), negative_zero.to_bits());
        assert_eq!(round_f32(-0.25).to_bits(), negative_zero.to_bits());
        assert_eq!(sqrt_f32(negative_zero).to_bits(), negative_zero.to_bits());
        assert_eq!(sin_f32(negative_zero).to_bits(), negative_zero.to_bits());
        assert_eq!(cos_f32(negative_zero).to_bits(), 1.0_f32.to_bits());
        assert_eq!(
            sqrt_f32(subnormal).to_bits(),
            libm::sqrtf(subnormal).to_bits()
        );

        for value in [signaling_nan, negative_nan] {
            assert_eq!(floor_f32(value).to_bits(), CANONICAL_NAN_F32_BITS);
            assert_eq!(ceil_f32(value).to_bits(), CANONICAL_NAN_F32_BITS);
            assert_eq!(round_f32(value).to_bits(), CANONICAL_NAN_F32_BITS);
            assert_eq!(sqrt_f32(value).to_bits(), CANONICAL_NAN_F32_BITS);
            assert_eq!(sin_f32(value).to_bits(), CANONICAL_NAN_F32_BITS);
            assert_eq!(cos_f32(value).to_bits(), CANONICAL_NAN_F32_BITS);
        }
        assert_eq!(sqrt_f32(-1.0).to_bits(), CANONICAL_NAN_F32_BITS);
        for value in [f32::NEG_INFINITY, f32::INFINITY] {
            assert_eq!(sin_f32(value).to_bits(), CANONICAL_NAN_F32_BITS);
            assert_eq!(cos_f32(value).to_bits(), CANONICAL_NAN_F32_BITS);
        }
        assert_eq!(sqrt_f32(f32::INFINITY).to_bits(), f32::INFINITY.to_bits());
    }

    #[test]
    fn f64_special_values_have_canonical_bits() {
        let negative_zero = f64::from_bits(0x8000_0000_0000_0000);
        let subnormal = f64::from_bits(1);
        let signaling_nan = f64::from_bits(0x7ff0_0000_0000_0001);
        let negative_nan = f64::from_bits(0xfff8_0000_0000_1234);

        assert_eq!(floor_f64(negative_zero).to_bits(), negative_zero.to_bits());
        assert_eq!(ceil_f64(negative_zero).to_bits(), negative_zero.to_bits());
        assert_eq!(round_f64(-0.25).to_bits(), negative_zero.to_bits());
        assert_eq!(sqrt_f64(negative_zero).to_bits(), negative_zero.to_bits());
        assert_eq!(sin_f64(negative_zero).to_bits(), negative_zero.to_bits());
        assert_eq!(cos_f64(negative_zero).to_bits(), 1.0_f64.to_bits());
        assert_eq!(
            sqrt_f64(subnormal).to_bits(),
            libm::sqrt(subnormal).to_bits()
        );

        for value in [signaling_nan, negative_nan] {
            assert_eq!(floor_f64(value).to_bits(), CANONICAL_NAN_F64_BITS);
            assert_eq!(ceil_f64(value).to_bits(), CANONICAL_NAN_F64_BITS);
            assert_eq!(round_f64(value).to_bits(), CANONICAL_NAN_F64_BITS);
            assert_eq!(sqrt_f64(value).to_bits(), CANONICAL_NAN_F64_BITS);
            assert_eq!(sin_f64(value).to_bits(), CANONICAL_NAN_F64_BITS);
            assert_eq!(cos_f64(value).to_bits(), CANONICAL_NAN_F64_BITS);
        }
        assert_eq!(sqrt_f64(-1.0).to_bits(), CANONICAL_NAN_F64_BITS);
        for value in [f64::NEG_INFINITY, f64::INFINITY] {
            assert_eq!(sin_f64(value).to_bits(), CANONICAL_NAN_F64_BITS);
            assert_eq!(cos_f64(value).to_bits(), CANONICAL_NAN_F64_BITS);
        }
        assert_eq!(sqrt_f64(f64::INFINITY).to_bits(), f64::INFINITY.to_bits());
    }
}
