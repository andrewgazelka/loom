//! Fast math for cells: float types whose arithmetic the compiler may reorder, fuse and vectorize.
//!
//! Plain `f32` arithmetic keeps IEEE order, so `acc += a[i] * b[i]` stays one scalar chain and a multiply followed by
//! an add is never fused. `F32` and `F64` do the same operators with Rust's algebraic operations
//! (`f32::algebraic_add` and friends, stable): the compiler may treat the operation as associative, distributive and
//! contractible, so a sum over a slice vectorizes four lanes wide and `a * b + c` becomes one `relaxed_madd` (the
//! guest is built with `+simd128,+relaxed-simd`). The cost is the usual one: results may differ in the last bits from
//! strict order, and a NaN or infinity is not guaranteed to propagate. Use them where that does not matter (geometry,
//! shading, simulation) and plain floats where it does (exact comparisons, hashing, anything cached by value that a
//! caller compares across machines).
//!
//! ```ignore
//! use loom::fast::{F32, dot};
//! let d: f32 = dot(&a, &b); // same as the loop below, several times faster
//! let mut acc = F32(0.0);
//! for i in 0..a.len() { acc += F32(a[i]) * F32(b[i]); }
//! ```

use core::iter::Sum;
use core::ops::{Add, AddAssign, Div, DivAssign, Mul, MulAssign, Neg, Sub, SubAssign};

macro_rules! fast_float {
    ($name:ident, $float:ty, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Copy, Debug, Default, PartialEq, PartialOrd)]
        #[repr(transparent)]
        pub struct $name(pub $float);

        impl Add for $name {
            type Output = Self;
            #[inline(always)]
            fn add(self, rhs: Self) -> Self {
                Self(self.0.algebraic_add(rhs.0))
            }
        }
        impl Sub for $name {
            type Output = Self;
            #[inline(always)]
            fn sub(self, rhs: Self) -> Self {
                Self(self.0.algebraic_sub(rhs.0))
            }
        }
        impl Mul for $name {
            type Output = Self;
            #[inline(always)]
            fn mul(self, rhs: Self) -> Self {
                Self(self.0.algebraic_mul(rhs.0))
            }
        }
        impl Div for $name {
            type Output = Self;
            #[inline(always)]
            fn div(self, rhs: Self) -> Self {
                Self(self.0.algebraic_div(rhs.0))
            }
        }
        impl Neg for $name {
            type Output = Self;
            #[inline(always)]
            fn neg(self) -> Self {
                Self(-self.0)
            }
        }
        impl AddAssign for $name {
            #[inline(always)]
            fn add_assign(&mut self, rhs: Self) {
                *self = *self + rhs;
            }
        }
        impl SubAssign for $name {
            #[inline(always)]
            fn sub_assign(&mut self, rhs: Self) {
                *self = *self - rhs;
            }
        }
        impl MulAssign for $name {
            #[inline(always)]
            fn mul_assign(&mut self, rhs: Self) {
                *self = *self * rhs;
            }
        }
        impl DivAssign for $name {
            #[inline(always)]
            fn div_assign(&mut self, rhs: Self) {
                *self = *self / rhs;
            }
        }
        impl Sum for $name {
            #[inline(always)]
            fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
                iter.fold(Self(0.0), |a, b| a + b)
            }
        }
        impl From<$float> for $name {
            #[inline(always)]
            fn from(value: $float) -> Self {
                Self(value)
            }
        }
        impl From<$name> for $float {
            #[inline(always)]
            fn from(value: $name) -> Self {
                value.0
            }
        }
        impl $name {
            /// `self * a + b`, fused where the target has a fused multiply-add.
            #[inline(always)]
            pub fn mul_add(self, a: Self, b: Self) -> Self {
                self * a + b
            }
            #[inline(always)]
            pub fn sqrt(self) -> Self {
                Self(self.0.sqrt())
            }
            #[inline(always)]
            pub fn abs(self) -> Self {
                Self(self.0.abs())
            }
        }
    };
}

fast_float!(F32, f32, "An `f32` whose arithmetic may be reordered, fused and vectorized (see the module).");
fast_float!(F64, f64, "An `f64` whose arithmetic may be reordered, fused and vectorized (see the module).");

/// The sum of `values`, in whatever order vectorizes.
#[inline(never)]
pub fn sum(values: &[f32]) -> f32 {
    let mut acc = 0.0f32;
    for &v in values {
        acc = acc.algebraic_add(v);
    }
    acc
}

/// The dot product of `a` and `b` over their common length, in whatever order vectorizes and fuses.
#[inline(never)]
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = 0.0f32;
    for (&x, &y) in a.iter().zip(b) {
        acc = acc.algebraic_add(x.algebraic_mul(y));
    }
    acc
}

/// `y[i] = a * x[i] + y[i]` over the common length, fused.
#[inline(never)]
pub fn axpy(a: f32, x: &[f32], y: &mut [f32]) {
    for (yi, &xi) in y.iter_mut().zip(x) {
        *yi = a.algebraic_mul(xi).algebraic_add(*yi);
    }
}
