use rand::Rng as _;

use rustc_apfloat::{ieee::Single, Float};
use rustc_middle::ty::layout::LayoutOf as _;
use rustc_middle::ty::Ty;
use rustc_middle::{mir, ty};
use rustc_span::Symbol;
use rustc_target::abi::Size;
use rustc_target::spec::abi::Abi;

use crate::*;
use helpers::bool_to_simd_element;
use shims::foreign_items::EmulateForeignItemResult;

mod aesni;
mod avx;
mod avx2;
mod sse;
mod sse2;
mod sse3;
mod sse41;
mod ssse3;

impl<'mir, 'tcx: 'mir> EvalContextExt<'mir, 'tcx> for crate::MiriInterpCx<'mir, 'tcx> {}
pub(super) trait EvalContextExt<'mir, 'tcx: 'mir>:
    crate::MiriInterpCxExt<'mir, 'tcx>
{
    fn emulate_x86_intrinsic(
        &mut self,
        link_name: Symbol,
        abi: Abi,
        args: &[OpTy<'tcx, Provenance>],
        dest: &MPlaceTy<'tcx, Provenance>,
    ) -> InterpResult<'tcx, EmulateForeignItemResult> {
        let this = self.eval_context_mut();
        // Prefix should have already been checked.
        let unprefixed_name = link_name.as_str().strip_prefix("llvm.x86.").unwrap();
        match unprefixed_name {
            // Used to implement the `_addcarry_u32` and `_addcarry_u64` functions.
            // Computes a + b with input and output carry. The input carry is an 8-bit
            // value, which is interpreted as 1 if it is non-zero. The output carry is
            // an 8-bit value that will be 0 or 1.
            // https://www.intel.com/content/www/us/en/docs/cpp-compiler/developer-guide-reference/2021-8/addcarry-u32-addcarry-u64.html
            "addcarry.32" | "addcarry.64" => {
                if unprefixed_name == "addcarry.64" && this.tcx.sess.target.arch != "x86_64" {
                    return Ok(EmulateForeignItemResult::NotSupported);
                }

                let [c_in, a, b] = this.check_shim(abi, Abi::Unadjusted, link_name, args)?;
                let c_in = this.read_scalar(c_in)?.to_u8()? != 0;
                let a = this.read_immediate(a)?;
                let b = this.read_immediate(b)?;

                let (sum, overflow1) = this.overflowing_binary_op(mir::BinOp::Add, &a, &b)?;
                let (sum, overflow2) = this.overflowing_binary_op(
                    mir::BinOp::Add,
                    &sum,
                    &ImmTy::from_uint(c_in, a.layout),
                )?;
                let c_out = overflow1 | overflow2;

                this.write_scalar(Scalar::from_u8(c_out.into()), &this.project_field(dest, 0)?)?;
                this.write_immediate(*sum, &this.project_field(dest, 1)?)?;
            }
            // Used to implement the `_subborrow_u32` and `_subborrow_u64` functions.
            // Computes a - b with input and output borrow. The input borrow is an 8-bit
            // value, which is interpreted as 1 if it is non-zero. The output borrow is
            // an 8-bit value that will be 0 or 1.
            // https://www.intel.com/content/www/us/en/docs/cpp-compiler/developer-guide-reference/2021-8/subborrow-u32-subborrow-u64.html
            "subborrow.32" | "subborrow.64" => {
                if unprefixed_name == "subborrow.64" && this.tcx.sess.target.arch != "x86_64" {
                    return Ok(EmulateForeignItemResult::NotSupported);
                }

                let [b_in, a, b] = this.check_shim(abi, Abi::Unadjusted, link_name, args)?;
                let b_in = this.read_scalar(b_in)?.to_u8()? != 0;
                let a = this.read_immediate(a)?;
                let b = this.read_immediate(b)?;

                let (sub, overflow1) = this.overflowing_binary_op(mir::BinOp::Sub, &a, &b)?;
                let (sub, overflow2) = this.overflowing_binary_op(
                    mir::BinOp::Sub,
                    &sub,
                    &ImmTy::from_uint(b_in, a.layout),
                )?;
                let b_out = overflow1 | overflow2;

                this.write_scalar(Scalar::from_u8(b_out.into()), &this.project_field(dest, 0)?)?;
                this.write_immediate(*sub, &this.project_field(dest, 1)?)?;
            }

            // Used to implement the `_mm_pause` function.
            // The intrinsic is used to hint the processor that the code is in a spin-loop.
            // It is compiled down to a `pause` instruction. When SSE2 is not available,
            // the instruction behaves like a no-op, so it is always safe to call the
            // intrinsic.
            "sse2.pause" => {
                let [] = this.check_shim(abi, Abi::C { unwind: false }, link_name, args)?;
                // Only exhibit the spin-loop hint behavior when SSE2 is enabled.
                if this.tcx.sess.unstable_target_features.contains(&Symbol::intern("sse2")) {
                    this.yield_active_thread();
                }
            }

            name if name.starts_with("sse.") => {
                return sse::EvalContextExt::emulate_x86_sse_intrinsic(
                    this, link_name, abi, args, dest,
                );
            }
            name if name.starts_with("sse2.") => {
                return sse2::EvalContextExt::emulate_x86_sse2_intrinsic(
                    this, link_name, abi, args, dest,
                );
            }
            name if name.starts_with("sse3.") => {
                return sse3::EvalContextExt::emulate_x86_sse3_intrinsic(
                    this, link_name, abi, args, dest,
                );
            }
            name if name.starts_with("ssse3.") => {
                return ssse3::EvalContextExt::emulate_x86_ssse3_intrinsic(
                    this, link_name, abi, args, dest,
                );
            }
            name if name.starts_with("sse41.") => {
                return sse41::EvalContextExt::emulate_x86_sse41_intrinsic(
                    this, link_name, abi, args, dest,
                );
            }
            name if name.starts_with("aesni.") => {
                return aesni::EvalContextExt::emulate_x86_aesni_intrinsic(
                    this, link_name, abi, args, dest,
                );
            }
            name if name.starts_with("avx.") => {
                return avx::EvalContextExt::emulate_x86_avx_intrinsic(
                    this, link_name, abi, args, dest,
                );
            }
            name if name.starts_with("avx2.") => {
                return avx2::EvalContextExt::emulate_x86_avx2_intrinsic(
                    this, link_name, abi, args, dest,
                );
            }

            _ => return Ok(EmulateForeignItemResult::NotSupported),
        }
        Ok(EmulateForeignItemResult::NeedsJumping)
    }
}

#[derive(Copy, Clone)]
enum FloatBinOp {
    /// Arithmetic operation
    Arith(mir::BinOp),
    /// Comparison
    ///
    /// The semantics of this operator is a case distinction: we compare the two operands,
    /// and then we return one of the four booleans `gt`, `lt`, `eq`, `unord` depending on
    /// which class they fall into.
    ///
    /// AVX supports all 16 combinations, SSE only a subset
    ///
    /// <https://www.felixcloutier.com/x86/cmpss>
    /// <https://www.felixcloutier.com/x86/cmpps>
    /// <https://www.felixcloutier.com/x86/cmpsd>
    /// <https://www.felixcloutier.com/x86/cmppd>
    Cmp {
        /// Result when lhs < rhs
        gt: bool,
        /// Result when lhs > rhs
        lt: bool,
        /// Result when lhs == rhs
        eq: bool,
        /// Result when lhs is NaN or rhs is NaN
        unord: bool,
    },
    /// Minimum value (with SSE semantics)
    ///
    /// <https://www.felixcloutier.com/x86/minss>
    /// <https://www.felixcloutier.com/x86/minps>
    /// <https://www.felixcloutier.com/x86/minsd>
    /// <https://www.felixcloutier.com/x86/minpd>
    Min,
    /// Maximum value (with SSE semantics)
    ///
    /// <https://www.felixcloutier.com/x86/maxss>
    /// <https://www.felixcloutier.com/x86/maxps>
    /// <https://www.felixcloutier.com/x86/maxsd>
    /// <https://www.felixcloutier.com/x86/maxpd>
    Max,
}

impl FloatBinOp {
    /// Convert from the `imm` argument used to specify the comparison
    /// operation in intrinsics such as `llvm.x86.sse.cmp.ss`.
    fn cmp_from_imm<'tcx>(
        this: &crate::MiriInterpCx<'_, 'tcx>,
        imm: i8,
        intrinsic: Symbol,
    ) -> InterpResult<'tcx, Self> {
        // Only bits 0..=4 are used, remaining should be zero.
        if imm & !0b1_1111 != 0 {
            throw_unsup_format!("invalid `imm` parameter of {intrinsic}: 0x{imm:x}");
        }
        // Bit 4 specifies whether the operation is quiet or signaling, which
        // we do not care in Miri.
        // Bits 0..=2 specifies the operation.
        // `gt` indicates the result to be returned when the LHS is strictly
        // greater than the RHS, and so on.
        let (gt, lt, eq, mut unord) = match imm & 0b111 {
            // Equal
            0x0 => (false, false, true, false),
            // Less-than
            0x1 => (false, true, false, false),
            // Less-or-equal
            0x2 => (false, true, true, false),
            // Unordered (either is NaN)
            0x3 => (false, false, false, true),
            // Not equal
            0x4 => (true, true, false, true),
            // Not less-than
            0x5 => (true, false, true, true),
            // Not less-or-equal
            0x6 => (true, false, false, true),
            // Ordered (neither is NaN)
            0x7 => (true, true, true, false),
            _ => unreachable!(),
        };
        // When bit 3 is 1 (only possible in AVX), unord is toggled.
        if imm & 0b1000 != 0 {
            this.expect_target_feature_for_intrinsic(intrinsic, "avx")?;
            unord = !unord;
        }
        Ok(Self::Cmp { gt, lt, eq, unord })
    }
}

/// Performs `which` scalar operation on `left` and `right` and returns
/// the result.
fn bin_op_float<'tcx, F: rustc_apfloat::Float>(
    this: &crate::MiriInterpCx<'_, 'tcx>,
    which: FloatBinOp,
    left: &ImmTy<'tcx, Provenance>,
    right: &ImmTy<'tcx, Provenance>,
) -> InterpResult<'tcx, Scalar<Provenance>> {
    match which {
        FloatBinOp::Arith(which) => {
            let res = this.wrapping_binary_op(which, left, right)?;
            Ok(res.to_scalar())
        }
        FloatBinOp::Cmp { gt, lt, eq, unord } => {
            let left = left.to_scalar().to_float::<F>()?;
            let right = right.to_scalar().to_float::<F>()?;

            let res = match left.partial_cmp(&right) {
                None => unord,
                Some(std::cmp::Ordering::Less) => lt,
                Some(std::cmp::Ordering::Equal) => eq,
                Some(std::cmp::Ordering::Greater) => gt,
            };
            Ok(bool_to_simd_element(res, Size::from_bits(F::BITS)))
        }
        FloatBinOp::Min => {
            let left_scalar = left.to_scalar();
            let left = left_scalar.to_float::<F>()?;
            let right_scalar = right.to_scalar();
            let right = right_scalar.to_float::<F>()?;
            // SSE semantics to handle zero and NaN. Note that `x == F::ZERO`
            // is true when `x` is either +0 or -0.
            if (left == F::ZERO && right == F::ZERO)
                || left.is_nan()
                || right.is_nan()
                || left >= right
            {
                Ok(right_scalar)
            } else {
                Ok(left_scalar)
            }
        }
        FloatBinOp::Max => {
            let left_scalar = left.to_scalar();
            let left = left_scalar.to_float::<F>()?;
            let right_scalar = right.to_scalar();
            let right = right_scalar.to_float::<F>()?;
            // SSE semantics to handle zero and NaN. Note that `x == F::ZERO`
            // is true when `x` is either +0 or -0.
            if (left == F::ZERO && right == F::ZERO)
                || left.is_nan()
                || right.is_nan()
                || left <= right
            {
                Ok(right_scalar)
            } else {
                Ok(left_scalar)
            }
        }
    }
}

/// Performs `which` operation on the first component of `left` and `right`
/// and copies the other components from `left`. The result is stored in `dest`.
fn bin_op_simd_float_first<'tcx, F: rustc_apfloat::Float>(
    this: &mut crate::MiriInterpCx<'_, 'tcx>,
    which: FloatBinOp,
    left: &OpTy<'tcx, Provenance>,
    right: &OpTy<'tcx, Provenance>,
    dest: &MPlaceTy<'tcx, Provenance>,
) -> InterpResult<'tcx, ()> {
    let (left, left_len) = this.operand_to_simd(left)?;
    let (right, right_len) = this.operand_to_simd(right)?;
    let (dest, dest_len) = this.mplace_to_simd(dest)?;

    assert_eq!(dest_len, left_len);
    assert_eq!(dest_len, right_len);

    let res0 = bin_op_float::<F>(
        this,
        which,
        &this.read_immediate(&this.project_index(&left, 0)?)?,
        &this.read_immediate(&this.project_index(&right, 0)?)?,
    )?;
    this.write_scalar(res0, &this.project_index(&dest, 0)?)?;

    for i in 1..dest_len {
        this.copy_op(&this.project_index(&left, i)?, &this.project_index(&dest, i)?)?;
    }

    Ok(())
}

/// Performs `which` operation on each component of `left` and
/// `right`, storing the result is stored in `dest`.
fn bin_op_simd_float_all<'tcx, F: rustc_apfloat::Float>(
    this: &mut crate::MiriInterpCx<'_, 'tcx>,
    which: FloatBinOp,
    left: &OpTy<'tcx, Provenance>,
    right: &OpTy<'tcx, Provenance>,
    dest: &MPlaceTy<'tcx, Provenance>,
) -> InterpResult<'tcx, ()> {
    let (left, left_len) = this.operand_to_simd(left)?;
    let (right, right_len) = this.operand_to_simd(right)?;
    let (dest, dest_len) = this.mplace_to_simd(dest)?;

    assert_eq!(dest_len, left_len);
    assert_eq!(dest_len, right_len);

    for i in 0..dest_len {
        let left = this.read_immediate(&this.project_index(&left, i)?)?;
        let right = this.read_immediate(&this.project_index(&right, i)?)?;
        let dest = this.project_index(&dest, i)?;

        let res = bin_op_float::<F>(this, which, &left, &right)?;
        this.write_scalar(res, &dest)?;
    }

    Ok(())
}

#[derive(Copy, Clone)]
enum FloatUnaryOp {
    /// sqrt(x)
    ///
    /// <https://www.felixcloutier.com/x86/sqrtss>
    /// <https://www.felixcloutier.com/x86/sqrtps>
    Sqrt,
    /// Approximation of 1/x
    ///
    /// <https://www.felixcloutier.com/x86/rcpss>
    /// <https://www.felixcloutier.com/x86/rcpps>
    Rcp,
    /// Approximation of 1/sqrt(x)
    ///
    /// <https://www.felixcloutier.com/x86/rsqrtss>
    /// <https://www.felixcloutier.com/x86/rsqrtps>
    Rsqrt,
}

/// Performs `which` scalar operation on `op` and returns the result.
#[allow(clippy::arithmetic_side_effects)] // floating point operations without side effects
fn unary_op_f32<'tcx>(
    this: &mut crate::MiriInterpCx<'_, 'tcx>,
    which: FloatUnaryOp,
    op: &ImmTy<'tcx, Provenance>,
) -> InterpResult<'tcx, Scalar<Provenance>> {
    match which {
        FloatUnaryOp::Sqrt => {
            let op = op.to_scalar();
            // FIXME using host floats
            Ok(Scalar::from_u32(f32::from_bits(op.to_u32()?).sqrt().to_bits()))
        }
        FloatUnaryOp::Rcp => {
            let op = op.to_scalar().to_f32()?;
            let div = (Single::from_u128(1).value / op).value;
            // Apply a relative error with a magnitude on the order of 2^-12 to simulate the
            // inaccuracy of RCP.
            let res = apply_random_float_error(this, div, -12);
            Ok(Scalar::from_f32(res))
        }
        FloatUnaryOp::Rsqrt => {
            let op = op.to_scalar().to_u32()?;
            // FIXME using host floats
            let sqrt = Single::from_bits(f32::from_bits(op).sqrt().to_bits().into());
            let rsqrt = (Single::from_u128(1).value / sqrt).value;
            // Apply a relative error with a magnitude on the order of 2^-12 to simulate the
            // inaccuracy of RSQRT.
            let res = apply_random_float_error(this, rsqrt, -12);
            Ok(Scalar::from_f32(res))
        }
    }
}

/// Disturbes a floating-point result by a relative error on the order of (-2^scale, 2^scale).
#[allow(clippy::arithmetic_side_effects)] // floating point arithmetic cannot panic
fn apply_random_float_error<F: rustc_apfloat::Float>(
    this: &mut crate::MiriInterpCx<'_, '_>,
    val: F,
    err_scale: i32,
) -> F {
    let rng = this.machine.rng.get_mut();
    // generates rand(0, 2^64) * 2^(scale - 64) = rand(0, 1) * 2^scale
    let err =
        F::from_u128(rng.gen::<u64>().into()).value.scalbn(err_scale.checked_sub(64).unwrap());
    // give it a random sign
    let err = if rng.gen::<bool>() { -err } else { err };
    // multiple the value with (1+err)
    (val * (F::from_u128(1).value + err).value).value
}

/// Performs `which` operation on the first component of `op` and copies
/// the other components. The result is stored in `dest`.
fn unary_op_ss<'tcx>(
    this: &mut crate::MiriInterpCx<'_, 'tcx>,
    which: FloatUnaryOp,
    op: &OpTy<'tcx, Provenance>,
    dest: &MPlaceTy<'tcx, Provenance>,
) -> InterpResult<'tcx, ()> {
    let (op, op_len) = this.operand_to_simd(op)?;
    let (dest, dest_len) = this.mplace_to_simd(dest)?;

    assert_eq!(dest_len, op_len);

    let res0 = unary_op_f32(this, which, &this.read_immediate(&this.project_index(&op, 0)?)?)?;
    this.write_scalar(res0, &this.project_index(&dest, 0)?)?;

    for i in 1..dest_len {
        this.copy_op(&this.project_index(&op, i)?, &this.project_index(&dest, i)?)?;
    }

    Ok(())
}

/// Performs `which` operation on each component of `op`, storing the
/// result is stored in `dest`.
fn unary_op_ps<'tcx>(
    this: &mut crate::MiriInterpCx<'_, 'tcx>,
    which: FloatUnaryOp,
    op: &OpTy<'tcx, Provenance>,
    dest: &MPlaceTy<'tcx, Provenance>,
) -> InterpResult<'tcx, ()> {
    let (op, op_len) = this.operand_to_simd(op)?;
    let (dest, dest_len) = this.mplace_to_simd(dest)?;

    assert_eq!(dest_len, op_len);

    for i in 0..dest_len {
        let op = this.read_immediate(&this.project_index(&op, i)?)?;
        let dest = this.project_index(&dest, i)?;

        let res = unary_op_f32(this, which, &op)?;
        this.write_scalar(res, &dest)?;
    }

    Ok(())
}

enum ShiftOp {
    /// Shift left, logically (shift in zeros) -- same as shift left, arithmetically
    Left,
    /// Shift right, logically (shift in zeros)
    RightLogic,
    /// Shift right, arithmetically (shift in sign)
    RightArith,
}

/// Shifts each element of `left` by a scalar amount. The shift amount
/// is determined by the lowest 64 bits of `right` (which is a 128-bit vector).
///
/// For logic shifts, when right is larger than BITS - 1, zero is produced.
/// For arithmetic right-shifts, when right is larger than BITS - 1, the sign
/// bit is copied to all bits.
fn shift_simd_by_scalar<'tcx>(
    this: &mut crate::MiriInterpCx<'_, 'tcx>,
    left: &OpTy<'tcx, Provenance>,
    right: &OpTy<'tcx, Provenance>,
    which: ShiftOp,
    dest: &MPlaceTy<'tcx, Provenance>,
) -> InterpResult<'tcx, ()> {
    let (left, left_len) = this.operand_to_simd(left)?;
    let (dest, dest_len) = this.mplace_to_simd(dest)?;

    assert_eq!(dest_len, left_len);
    // `right` may have a different length, and we only care about its
    // lowest 64bit anyway.

    // Get the 64-bit shift operand and convert it to the type expected
    // by checked_{shl,shr} (u32).
    // It is ok to saturate the value to u32::MAX because any value
    // above BITS - 1 will produce the same result.
    let shift = u32::try_from(extract_first_u64(this, right)?).unwrap_or(u32::MAX);

    for i in 0..dest_len {
        let left = this.read_scalar(&this.project_index(&left, i)?)?;
        let dest = this.project_index(&dest, i)?;

        let res = match which {
            ShiftOp::Left => {
                let left = left.to_uint(dest.layout.size)?;
                let res = left.checked_shl(shift).unwrap_or(0);
                // `truncate` is needed as left-shift can make the absolute value larger.
                Scalar::from_uint(dest.layout.size.truncate(res), dest.layout.size)
            }
            ShiftOp::RightLogic => {
                let left = left.to_uint(dest.layout.size)?;
                let res = left.checked_shr(shift).unwrap_or(0);
                // No `truncate` needed as right-shift can only make the absolute value smaller.
                Scalar::from_uint(res, dest.layout.size)
            }
            ShiftOp::RightArith => {
                let left = left.to_int(dest.layout.size)?;
                // On overflow, copy the sign bit to the remaining bits
                let res = left.checked_shr(shift).unwrap_or(left >> 127);
                // No `truncate` needed as right-shift can only make the absolute value smaller.
                Scalar::from_int(res, dest.layout.size)
            }
        };
        this.write_scalar(res, &dest)?;
    }

    Ok(())
}

/// Shifts each element of `left` by the corresponding element of `right`.
///
/// For logic shifts, when right is larger than BITS - 1, zero is produced.
/// For arithmetic right-shifts, when right is larger than BITS - 1, the sign
/// bit is copied to all bits.
fn shift_simd_by_simd<'tcx>(
    this: &mut crate::MiriInterpCx<'_, 'tcx>,
    left: &OpTy<'tcx, Provenance>,
    right: &OpTy<'tcx, Provenance>,
    which: ShiftOp,
    dest: &MPlaceTy<'tcx, Provenance>,
) -> InterpResult<'tcx, ()> {
    let (left, left_len) = this.operand_to_simd(left)?;
    let (right, right_len) = this.operand_to_simd(right)?;
    let (dest, dest_len) = this.mplace_to_simd(dest)?;

    assert_eq!(dest_len, left_len);
    assert_eq!(dest_len, right_len);

    for i in 0..dest_len {
        let left = this.read_scalar(&this.project_index(&left, i)?)?;
        let right = this.read_scalar(&this.project_index(&right, i)?)?;
        let dest = this.project_index(&dest, i)?;

        // It is ok to saturate the value to u32::MAX because any value
        // above BITS - 1 will produce the same result.
        let shift = u32::try_from(right.to_uint(dest.layout.size)?).unwrap_or(u32::MAX);

        let res = match which {
            ShiftOp::Left => {
                let left = left.to_uint(dest.layout.size)?;
                let res = left.checked_shl(shift).unwrap_or(0);
                // `truncate` is needed as left-shift can make the absolute value larger.
                Scalar::from_uint(dest.layout.size.truncate(res), dest.layout.size)
            }
            ShiftOp::RightLogic => {
                let left = left.to_uint(dest.layout.size)?;
                let res = left.checked_shr(shift).unwrap_or(0);
                // No `truncate` needed as right-shift can only make the absolute value smaller.
                Scalar::from_uint(res, dest.layout.size)
            }
            ShiftOp::RightArith => {
                let left = left.to_int(dest.layout.size)?;
                // On overflow, copy the sign bit to the remaining bits
                let res = left.checked_shr(shift).unwrap_or(left >> 127);
                // No `truncate` needed as right-shift can only make the absolute value smaller.
                Scalar::from_int(res, dest.layout.size)
            }
        };
        this.write_scalar(res, &dest)?;
    }

    Ok(())
}

/// Takes a 128-bit vector, transmutes it to `[u64; 2]` and extracts
/// the first value.
fn extract_first_u64<'tcx>(
    this: &crate::MiriInterpCx<'_, 'tcx>,
    op: &OpTy<'tcx, Provenance>,
) -> InterpResult<'tcx, u64> {
    // Transmute vector to `[u64; 2]`
    let array_layout = this.layout_of(Ty::new_array(this.tcx.tcx, this.tcx.types.u64, 2))?;
    let op = op.transmute(array_layout, this)?;

    // Get the first u64 from the array
    this.read_scalar(&this.project_index(&op, 0)?)?.to_u64()
}

// Rounds the first element of `right` according to `rounding`
// and copies the remaining elements from `left`.
fn round_first<'tcx, F: rustc_apfloat::Float>(
    this: &mut crate::MiriInterpCx<'_, 'tcx>,
    left: &OpTy<'tcx, Provenance>,
    right: &OpTy<'tcx, Provenance>,
    rounding: &OpTy<'tcx, Provenance>,
    dest: &MPlaceTy<'tcx, Provenance>,
) -> InterpResult<'tcx, ()> {
    let (left, left_len) = this.operand_to_simd(left)?;
    let (right, right_len) = this.operand_to_simd(right)?;
    let (dest, dest_len) = this.mplace_to_simd(dest)?;

    assert_eq!(dest_len, left_len);
    assert_eq!(dest_len, right_len);

    let rounding = rounding_from_imm(this.read_scalar(rounding)?.to_i32()?)?;

    let op0: F = this.read_scalar(&this.project_index(&right, 0)?)?.to_float()?;
    let res = op0.round_to_integral(rounding).value;
    this.write_scalar(
        Scalar::from_uint(res.to_bits(), Size::from_bits(F::BITS)),
        &this.project_index(&dest, 0)?,
    )?;

    for i in 1..dest_len {
        this.copy_op(&this.project_index(&left, i)?, &this.project_index(&dest, i)?)?;
    }

    Ok(())
}

// Rounds all elements of `op` according to `rounding`.
fn round_all<'tcx, F: rustc_apfloat::Float>(
    this: &mut crate::MiriInterpCx<'_, 'tcx>,
    op: &OpTy<'tcx, Provenance>,
    rounding: &OpTy<'tcx, Provenance>,
    dest: &MPlaceTy<'tcx, Provenance>,
) -> InterpResult<'tcx, ()> {
    let (op, op_len) = this.operand_to_simd(op)?;
    let (dest, dest_len) = this.mplace_to_simd(dest)?;

    assert_eq!(dest_len, op_len);

    let rounding = rounding_from_imm(this.read_scalar(rounding)?.to_i32()?)?;

    for i in 0..dest_len {
        let op: F = this.read_scalar(&this.project_index(&op, i)?)?.to_float()?;
        let res = op.round_to_integral(rounding).value;
        this.write_scalar(
            Scalar::from_uint(res.to_bits(), Size::from_bits(F::BITS)),
            &this.project_index(&dest, i)?,
        )?;
    }

    Ok(())
}

/// Gets equivalent `rustc_apfloat::Round` from rounding mode immediate of
/// `round.{ss,sd,ps,pd}` intrinsics.
fn rounding_from_imm<'tcx>(rounding: i32) -> InterpResult<'tcx, rustc_apfloat::Round> {
    // The fourth bit of `rounding` only affects the SSE status
    // register, which cannot be accessed from Miri (or from Rust,
    // for that matter), so we can ignore it.
    match rounding & !0b1000 {
        // When the third bit is 0, the rounding mode is determined by the
        // first two bits.
        0b000 => Ok(rustc_apfloat::Round::NearestTiesToEven),
        0b001 => Ok(rustc_apfloat::Round::TowardNegative),
        0b010 => Ok(rustc_apfloat::Round::TowardPositive),
        0b011 => Ok(rustc_apfloat::Round::TowardZero),
        // When the third bit is 1, the rounding mode is determined by the
        // SSE status register. Since we do not support modifying it from
        // Miri (or Rust), we assume it to be at its default mode (round-to-nearest).
        0b100..=0b111 => Ok(rustc_apfloat::Round::NearestTiesToEven),
        rounding => throw_unsup_format!("unsupported rounding mode 0x{rounding:02x}"),
    }
}

/// Converts each element of `op` from floating point to signed integer.
///
/// When the input value is NaN or out of range, fall back to minimum value.
///
/// If `op` has more elements than `dest`, extra elements are ignored. If `op`
/// has less elements than `dest`, the rest is filled with zeros.
fn convert_float_to_int<'tcx>(
    this: &mut crate::MiriInterpCx<'_, 'tcx>,
    op: &OpTy<'tcx, Provenance>,
    rnd: rustc_apfloat::Round,
    dest: &MPlaceTy<'tcx, Provenance>,
) -> InterpResult<'tcx, ()> {
    let (op, op_len) = this.operand_to_simd(op)?;
    let (dest, dest_len) = this.mplace_to_simd(dest)?;

    // Output must be *signed* integers.
    assert!(matches!(dest.layout.field(this, 0).ty.kind(), ty::Int(_)));

    for i in 0..op_len.min(dest_len) {
        let op = this.read_immediate(&this.project_index(&op, i)?)?;
        let dest = this.project_index(&dest, i)?;

        let res = this.float_to_int_checked(&op, dest.layout, rnd)?.unwrap_or_else(|| {
            // Fallback to minimum according to SSE/AVX semantics.
            ImmTy::from_int(dest.layout.size.signed_int_min(), dest.layout)
        });
        this.write_immediate(*res, &dest)?;
    }
    // Fill remainder with zeros
    for i in op_len..dest_len {
        let dest = this.project_index(&dest, i)?;
        this.write_scalar(Scalar::from_int(0, dest.layout.size), &dest)?;
    }

    Ok(())
}

/// Calculates absolute value of integers in `op` and stores the result in `dest`.
///
/// In case of overflow (when the operand is the minimum value), the operation
/// will wrap around.
fn int_abs<'tcx>(
    this: &mut crate::MiriInterpCx<'_, 'tcx>,
    op: &OpTy<'tcx, Provenance>,
    dest: &MPlaceTy<'tcx, Provenance>,
) -> InterpResult<'tcx, ()> {
    let (op, op_len) = this.operand_to_simd(op)?;
    let (dest, dest_len) = this.mplace_to_simd(dest)?;

    assert_eq!(op_len, dest_len);

    let zero = ImmTy::from_int(0, op.layout.field(this, 0));

    for i in 0..dest_len {
        let op = this.read_immediate(&this.project_index(&op, i)?)?;
        let dest = this.project_index(&dest, i)?;

        let lt_zero = this.wrapping_binary_op(mir::BinOp::Lt, &op, &zero)?;
        let res = if lt_zero.to_scalar().to_bool()? {
            this.wrapping_unary_op(mir::UnOp::Neg, &op)?
        } else {
            op
        };

        this.write_immediate(*res, &dest)?;
    }

    Ok(())
}

/// Splits `op` (which must be a SIMD vector) into 128-bit chuncks.
///
/// Returns a tuple where:
/// * The first element is the number of 128-bit chunks (let's call it `N`).
/// * The second element is the number of elements per chunk (let's call it `M`).
/// * The third element is the `op` vector split into chunks, i.e, it's
///   type is `[[T; M]; N]` where `T` is the element type of `op`.
fn split_simd_to_128bit_chunks<'tcx, P: Projectable<'tcx, Provenance>>(
    this: &mut crate::MiriInterpCx<'_, 'tcx>,
    op: &P,
) -> InterpResult<'tcx, (u64, u64, P)> {
    let simd_layout = op.layout();
    let (simd_len, element_ty) = simd_layout.ty.simd_size_and_type(this.tcx.tcx);

    assert_eq!(simd_layout.size.bits() % 128, 0);
    let num_chunks = simd_layout.size.bits() / 128;
    let items_per_chunk = simd_len.checked_div(num_chunks).unwrap();

    // Transmute to `[[T; items_per_chunk]; num_chunks]`
    let chunked_layout = this
        .layout_of(Ty::new_array(
            this.tcx.tcx,
            Ty::new_array(this.tcx.tcx, element_ty, items_per_chunk),
            num_chunks,
        ))
        .unwrap();
    let chunked_op = op.transmute(chunked_layout, this)?;

    Ok((num_chunks, items_per_chunk, chunked_op))
}

/// Horizontaly performs `which` operation on adjacent values of
/// `left` and `right` SIMD vectors and stores the result in `dest`.
/// "Horizontal" means that the i-th output element is calculated
/// from the elements 2*i and 2*i+1 of the concatenation of `left` and
/// `right`.
///
/// Each 128-bit chunk is treated independently (i.e., the value for
/// the is i-th 128-bit chunk of `dest` is calculated with the i-th
/// 128-bit chunks of `left` and `right`).
fn horizontal_bin_op<'tcx>(
    this: &mut crate::MiriInterpCx<'_, 'tcx>,
    which: mir::BinOp,
    saturating: bool,
    left: &OpTy<'tcx, Provenance>,
    right: &OpTy<'tcx, Provenance>,
    dest: &MPlaceTy<'tcx, Provenance>,
) -> InterpResult<'tcx, ()> {
    assert_eq!(left.layout, dest.layout);
    assert_eq!(right.layout, dest.layout);

    let (num_chunks, items_per_chunk, left) = split_simd_to_128bit_chunks(this, left)?;
    let (_, _, right) = split_simd_to_128bit_chunks(this, right)?;
    let (_, _, dest) = split_simd_to_128bit_chunks(this, dest)?;

    let middle = items_per_chunk / 2;
    for i in 0..num_chunks {
        let left = this.project_index(&left, i)?;
        let right = this.project_index(&right, i)?;
        let dest = this.project_index(&dest, i)?;

        for j in 0..items_per_chunk {
            // `j` is the index in `dest`
            // `k` is the index of the 2-item chunk in `src`
            let (k, src) =
                if j < middle { (j, &left) } else { (j.checked_sub(middle).unwrap(), &right) };
            // `base_i` is the index of the first item of the 2-item chunk in `src`
            let base_i = k.checked_mul(2).unwrap();
            let lhs = this.read_immediate(&this.project_index(src, base_i)?)?;
            let rhs =
                this.read_immediate(&this.project_index(src, base_i.checked_add(1).unwrap())?)?;

            let res = if saturating {
                Immediate::from(this.saturating_arith(which, &lhs, &rhs)?)
            } else {
                *this.wrapping_binary_op(which, &lhs, &rhs)?
            };

            this.write_immediate(res, &this.project_index(&dest, j)?)?;
        }
    }

    Ok(())
}

/// Conditionally multiplies the packed floating-point elements in
/// `left` and `right` using the high 4 bits in `imm`, sums the calculated
/// products (up to 4), and conditionally stores the sum in `dest` using
/// the low 4 bits of `imm`.
///
/// Each 128-bit chunk is treated independently (i.e., the value for
/// the is i-th 128-bit chunk of `dest` is calculated with the i-th
/// 128-bit blocks of `left` and `right`).
fn conditional_dot_product<'tcx>(
    this: &mut crate::MiriInterpCx<'_, 'tcx>,
    left: &OpTy<'tcx, Provenance>,
    right: &OpTy<'tcx, Provenance>,
    imm: &OpTy<'tcx, Provenance>,
    dest: &MPlaceTy<'tcx, Provenance>,
) -> InterpResult<'tcx, ()> {
    assert_eq!(left.layout, dest.layout);
    assert_eq!(right.layout, dest.layout);

    let (num_chunks, items_per_chunk, left) = split_simd_to_128bit_chunks(this, left)?;
    let (_, _, right) = split_simd_to_128bit_chunks(this, right)?;
    let (_, _, dest) = split_simd_to_128bit_chunks(this, dest)?;

    let element_layout = left.layout.field(this, 0).field(this, 0);
    assert!(items_per_chunk <= 4);

    // `imm` is a `u8` for SSE4.1 or an `i32` for AVX :/
    let imm = this.read_scalar(imm)?.to_uint(imm.layout.size)?;

    for i in 0..num_chunks {
        let left = this.project_index(&left, i)?;
        let right = this.project_index(&right, i)?;
        let dest = this.project_index(&dest, i)?;

        // Calculate dot product
        // Elements are floating point numbers, but we can use `from_int`
        // for the initial value because the representation of 0.0 is all zero bits.
        let mut sum = ImmTy::from_int(0u8, element_layout);
        for j in 0..items_per_chunk {
            if imm & (1 << j.checked_add(4).unwrap()) != 0 {
                let left = this.read_immediate(&this.project_index(&left, j)?)?;
                let right = this.read_immediate(&this.project_index(&right, j)?)?;

                let mul = this.wrapping_binary_op(mir::BinOp::Mul, &left, &right)?;
                sum = this.wrapping_binary_op(mir::BinOp::Add, &sum, &mul)?;
            }
        }

        // Write to destination (conditioned to imm)
        for j in 0..items_per_chunk {
            let dest = this.project_index(&dest, j)?;

            if imm & (1 << j) != 0 {
                this.write_immediate(*sum, &dest)?;
            } else {
                this.write_scalar(Scalar::from_int(0u8, element_layout.size), &dest)?;
            }
        }
    }

    Ok(())
}

/// Calculates two booleans.
///
/// The first is true when all the bits of `op & mask` are zero.
/// The second is true when `(op & mask) == mask`
fn test_bits_masked<'tcx>(
    this: &crate::MiriInterpCx<'_, 'tcx>,
    op: &OpTy<'tcx, Provenance>,
    mask: &OpTy<'tcx, Provenance>,
) -> InterpResult<'tcx, (bool, bool)> {
    assert_eq!(op.layout, mask.layout);

    let (op, op_len) = this.operand_to_simd(op)?;
    let (mask, mask_len) = this.operand_to_simd(mask)?;

    assert_eq!(op_len, mask_len);

    let mut all_zero = true;
    let mut masked_set = true;
    for i in 0..op_len {
        let op = this.project_index(&op, i)?;
        let mask = this.project_index(&mask, i)?;

        let op = this.read_scalar(&op)?.to_uint(op.layout.size)?;
        let mask = this.read_scalar(&mask)?.to_uint(mask.layout.size)?;
        all_zero &= (op & mask) == 0;
        masked_set &= (op & mask) == mask;
    }

    Ok((all_zero, masked_set))
}

/// Calculates two booleans.
///
/// The first is true when the highest bit of each element of `op & mask` is zero.
/// The second is true when the highest bit of each element of `!op & mask` is zero.
fn test_high_bits_masked<'tcx>(
    this: &crate::MiriInterpCx<'_, 'tcx>,
    op: &OpTy<'tcx, Provenance>,
    mask: &OpTy<'tcx, Provenance>,
) -> InterpResult<'tcx, (bool, bool)> {
    assert_eq!(op.layout, mask.layout);

    let (op, op_len) = this.operand_to_simd(op)?;
    let (mask, mask_len) = this.operand_to_simd(mask)?;

    assert_eq!(op_len, mask_len);

    let high_bit_offset = op.layout.field(this, 0).size.bits().checked_sub(1).unwrap();

    let mut direct = true;
    let mut negated = true;
    for i in 0..op_len {
        let op = this.project_index(&op, i)?;
        let mask = this.project_index(&mask, i)?;

        let op = this.read_scalar(&op)?.to_uint(op.layout.size)?;
        let mask = this.read_scalar(&mask)?.to_uint(mask.layout.size)?;
        direct &= (op & mask) >> high_bit_offset == 0;
        negated &= (!op & mask) >> high_bit_offset == 0;
    }

    Ok((direct, negated))
}

/// Conditionally loads from `ptr` according the high bit of each
/// element of `mask`. `ptr` does not need to be aligned.
fn mask_load<'tcx>(
    this: &mut crate::MiriInterpCx<'_, 'tcx>,
    ptr: &OpTy<'tcx, Provenance>,
    mask: &OpTy<'tcx, Provenance>,
    dest: &MPlaceTy<'tcx, Provenance>,
) -> InterpResult<'tcx, ()> {
    let (mask, mask_len) = this.operand_to_simd(mask)?;
    let (dest, dest_len) = this.mplace_to_simd(dest)?;

    assert_eq!(dest_len, mask_len);

    let mask_item_size = mask.layout.field(this, 0).size;
    let high_bit_offset = mask_item_size.bits().checked_sub(1).unwrap();

    let ptr = this.read_pointer(ptr)?;
    for i in 0..dest_len {
        let mask = this.project_index(&mask, i)?;
        let dest = this.project_index(&dest, i)?;

        if this.read_scalar(&mask)?.to_uint(mask_item_size)? >> high_bit_offset != 0 {
            let ptr = ptr.wrapping_offset(dest.layout.size * i, &this.tcx);
            // Unaligned copy, which is what we want.
            this.mem_copy(ptr, dest.ptr(), dest.layout.size, /*nonoverlapping*/ true)?;
        } else {
            this.write_scalar(Scalar::from_int(0, dest.layout.size), &dest)?;
        }
    }

    Ok(())
}

/// Conditionally stores into `ptr` according the high bit of each
/// element of `mask`. `ptr` does not need to be aligned.
fn mask_store<'tcx>(
    this: &mut crate::MiriInterpCx<'_, 'tcx>,
    ptr: &OpTy<'tcx, Provenance>,
    mask: &OpTy<'tcx, Provenance>,
    value: &OpTy<'tcx, Provenance>,
) -> InterpResult<'tcx, ()> {
    let (mask, mask_len) = this.operand_to_simd(mask)?;
    let (value, value_len) = this.operand_to_simd(value)?;

    assert_eq!(value_len, mask_len);

    let mask_item_size = mask.layout.field(this, 0).size;
    let high_bit_offset = mask_item_size.bits().checked_sub(1).unwrap();

    let ptr = this.read_pointer(ptr)?;
    for i in 0..value_len {
        let mask = this.project_index(&mask, i)?;
        let value = this.project_index(&value, i)?;

        if this.read_scalar(&mask)?.to_uint(mask_item_size)? >> high_bit_offset != 0 {
            let ptr = ptr.wrapping_offset(value.layout.size * i, &this.tcx);
            // Unaligned copy, which is what we want.
            this.mem_copy(value.ptr(), ptr, value.layout.size, /*nonoverlapping*/ true)?;
        }
    }

    Ok(())
}

/// Compute the sum of absolute differences of quadruplets of unsigned
/// 8-bit integers in `left` and `right`, and store the 16-bit results
/// in `right`. Quadruplets are selected from `left` and `right` with
/// offsets specified in `imm`.
///
/// <https://www.intel.com/content/www/us/en/docs/intrinsics-guide/index.html#text=_mm_maddubs_epi16>
/// <https://www.intel.com/content/www/us/en/docs/intrinsics-guide/index.html#text=_mm256_mpsadbw_epu8>
///
/// Each 128-bit chunk is treated independently (i.e., the value for
/// the is i-th 128-bit chunk of `dest` is calculated with the i-th
/// 128-bit chunks of `left` and `right`).
fn mpsadbw<'tcx>(
    this: &mut crate::MiriInterpCx<'_, 'tcx>,
    left: &OpTy<'tcx, Provenance>,
    right: &OpTy<'tcx, Provenance>,
    imm: &OpTy<'tcx, Provenance>,
    dest: &MPlaceTy<'tcx, Provenance>,
) -> InterpResult<'tcx, ()> {
    assert_eq!(left.layout, right.layout);
    assert_eq!(left.layout.size, dest.layout.size);

    let (num_chunks, op_items_per_chunk, left) = split_simd_to_128bit_chunks(this, left)?;
    let (_, _, right) = split_simd_to_128bit_chunks(this, right)?;
    let (_, dest_items_per_chunk, dest) = split_simd_to_128bit_chunks(this, dest)?;

    assert_eq!(op_items_per_chunk, dest_items_per_chunk.checked_mul(2).unwrap());

    let imm = this.read_scalar(imm)?.to_uint(imm.layout.size)?;
    // Bit 2 of `imm` specifies the offset for indices of `left`.
    // The offset is 0 when the bit is 0 or 4 when the bit is 1.
    let left_offset = u64::try_from((imm >> 2) & 1).unwrap().checked_mul(4).unwrap();
    // Bits 0..=1 of `imm` specify the offset for indices of
    // `right` in blocks of 4 elements.
    let right_offset = u64::try_from(imm & 0b11).unwrap().checked_mul(4).unwrap();

    for i in 0..num_chunks {
        let left = this.project_index(&left, i)?;
        let right = this.project_index(&right, i)?;
        let dest = this.project_index(&dest, i)?;

        for j in 0..dest_items_per_chunk {
            let left_offset = left_offset.checked_add(j).unwrap();
            let mut res: u16 = 0;
            for k in 0..4 {
                let left = this
                    .read_scalar(&this.project_index(&left, left_offset.checked_add(k).unwrap())?)?
                    .to_u8()?;
                let right = this
                    .read_scalar(
                        &this.project_index(&right, right_offset.checked_add(k).unwrap())?,
                    )?
                    .to_u8()?;
                res = res.checked_add(left.abs_diff(right).into()).unwrap();
            }
            this.write_scalar(Scalar::from_u16(res), &this.project_index(&dest, j)?)?;
        }
    }

    Ok(())
}

/// Multiplies packed 16-bit signed integer values, truncates the 32-bit
/// product to the 18 most significant bits by right-shifting, and then
/// divides the 18-bit value by 2 (rounding to nearest) by first adding
/// 1 and then taking the bits `1..=16`.
///
/// <https://www.intel.com/content/www/us/en/docs/intrinsics-guide/index.html#text=_mm_mulhrs_epi16>
/// <https://www.intel.com/content/www/us/en/docs/intrinsics-guide/index.html#text=_mm256_mulhrs_epi16>
fn pmulhrsw<'tcx>(
    this: &mut crate::MiriInterpCx<'_, 'tcx>,
    left: &OpTy<'tcx, Provenance>,
    right: &OpTy<'tcx, Provenance>,
    dest: &MPlaceTy<'tcx, Provenance>,
) -> InterpResult<'tcx, ()> {
    let (left, left_len) = this.operand_to_simd(left)?;
    let (right, right_len) = this.operand_to_simd(right)?;
    let (dest, dest_len) = this.mplace_to_simd(dest)?;

    assert_eq!(dest_len, left_len);
    assert_eq!(dest_len, right_len);

    for i in 0..dest_len {
        let left = this.read_scalar(&this.project_index(&left, i)?)?.to_i16()?;
        let right = this.read_scalar(&this.project_index(&right, i)?)?.to_i16()?;
        let dest = this.project_index(&dest, i)?;

        let res =
            (i32::from(left).checked_mul(right.into()).unwrap() >> 14).checked_add(1).unwrap() >> 1;

        // The result of this operation can overflow a signed 16-bit integer.
        // When `left` and `right` are -0x8000, the result is 0x8000.
        #[allow(clippy::cast_possible_truncation)]
        let res = res as i16;

        this.write_scalar(Scalar::from_i16(res), &dest)?;
    }

    Ok(())
}

/// Packs two N-bit integer vectors to a single N/2-bit integers.
///
/// The conversion from N-bit to N/2-bit should be provided by `f`.
///
/// Each 128-bit chunk is treated independently (i.e., the value for
/// the is i-th 128-bit chunk of `dest` is calculated with the i-th
/// 128-bit chunks of `left` and `right`).
fn pack_generic<'tcx>(
    this: &mut crate::MiriInterpCx<'_, 'tcx>,
    left: &OpTy<'tcx, Provenance>,
    right: &OpTy<'tcx, Provenance>,
    dest: &MPlaceTy<'tcx, Provenance>,
    f: impl Fn(Scalar<Provenance>) -> InterpResult<'tcx, Scalar<Provenance>>,
) -> InterpResult<'tcx, ()> {
    assert_eq!(left.layout, right.layout);
    assert_eq!(left.layout.size, dest.layout.size);

    let (num_chunks, op_items_per_chunk, left) = split_simd_to_128bit_chunks(this, left)?;
    let (_, _, right) = split_simd_to_128bit_chunks(this, right)?;
    let (_, dest_items_per_chunk, dest) = split_simd_to_128bit_chunks(this, dest)?;

    assert_eq!(dest_items_per_chunk, op_items_per_chunk.checked_mul(2).unwrap());

    for i in 0..num_chunks {
        let left = this.project_index(&left, i)?;
        let right = this.project_index(&right, i)?;
        let dest = this.project_index(&dest, i)?;

        for j in 0..op_items_per_chunk {
            let left = this.read_scalar(&this.project_index(&left, j)?)?;
            let right = this.read_scalar(&this.project_index(&right, j)?)?;
            let left_dest = this.project_index(&dest, j)?;
            let right_dest =
                this.project_index(&dest, j.checked_add(op_items_per_chunk).unwrap())?;

            let left_res = f(left)?;
            let right_res = f(right)?;

            this.write_scalar(left_res, &left_dest)?;
            this.write_scalar(right_res, &right_dest)?;
        }
    }

    Ok(())
}

/// Converts two 16-bit integer vectors to a single 8-bit integer
/// vector with signed saturation.
///
/// Each 128-bit chunk is treated independently (i.e., the value for
/// the is i-th 128-bit chunk of `dest` is calculated with the i-th
/// 128-bit chunks of `left` and `right`).
fn packsswb<'tcx>(
    this: &mut crate::MiriInterpCx<'_, 'tcx>,
    left: &OpTy<'tcx, Provenance>,
    right: &OpTy<'tcx, Provenance>,
    dest: &MPlaceTy<'tcx, Provenance>,
) -> InterpResult<'tcx, ()> {
    pack_generic(this, left, right, dest, |op| {
        let op = op.to_i16()?;
        let res = i8::try_from(op).unwrap_or(if op < 0 { i8::MIN } else { i8::MAX });
        Ok(Scalar::from_i8(res))
    })
}

/// Converts two 16-bit signed integer vectors to a single 8-bit
/// unsigned integer vector with saturation.
///
/// Each 128-bit chunk is treated independently (i.e., the value for
/// the is i-th 128-bit chunk of `dest` is calculated with the i-th
/// 128-bit chunks of `left` and `right`).
fn packuswb<'tcx>(
    this: &mut crate::MiriInterpCx<'_, 'tcx>,
    left: &OpTy<'tcx, Provenance>,
    right: &OpTy<'tcx, Provenance>,
    dest: &MPlaceTy<'tcx, Provenance>,
) -> InterpResult<'tcx, ()> {
    pack_generic(this, left, right, dest, |op| {
        let op = op.to_i16()?;
        let res = u8::try_from(op).unwrap_or(if op < 0 { 0 } else { u8::MAX });
        Ok(Scalar::from_u8(res))
    })
}

/// Converts two 32-bit integer vectors to a single 16-bit integer
/// vector with signed saturation.
///
/// Each 128-bit chunk is treated independently (i.e., the value for
/// the is i-th 128-bit chunk of `dest` is calculated with the i-th
/// 128-bit chunks of `left` and `right`).
fn packssdw<'tcx>(
    this: &mut crate::MiriInterpCx<'_, 'tcx>,
    left: &OpTy<'tcx, Provenance>,
    right: &OpTy<'tcx, Provenance>,
    dest: &MPlaceTy<'tcx, Provenance>,
) -> InterpResult<'tcx, ()> {
    pack_generic(this, left, right, dest, |op| {
        let op = op.to_i32()?;
        let res = i16::try_from(op).unwrap_or(if op < 0 { i16::MIN } else { i16::MAX });
        Ok(Scalar::from_i16(res))
    })
}

/// Converts two 32-bit integer vectors to a single 16-bit integer
/// vector with unsigned saturation.
///
/// Each 128-bit chunk is treated independently (i.e., the value for
/// the is i-th 128-bit chunk of `dest` is calculated with the i-th
/// 128-bit chunks of `left` and `right`).
fn packusdw<'tcx>(
    this: &mut crate::MiriInterpCx<'_, 'tcx>,
    left: &OpTy<'tcx, Provenance>,
    right: &OpTy<'tcx, Provenance>,
    dest: &MPlaceTy<'tcx, Provenance>,
) -> InterpResult<'tcx, ()> {
    pack_generic(this, left, right, dest, |op| {
        let op = op.to_i32()?;
        let res = u16::try_from(op).unwrap_or(if op < 0 { 0 } else { u16::MAX });
        Ok(Scalar::from_u16(res))
    })
}

/// Negates elements from `left` when the corresponding element in
/// `right` is negative. If an element from `right` is zero, zero
/// is writen to the corresponding output element.
/// In other words, multiplies `left` with `right.signum()`.
fn psign<'tcx>(
    this: &mut crate::MiriInterpCx<'_, 'tcx>,
    left: &OpTy<'tcx, Provenance>,
    right: &OpTy<'tcx, Provenance>,
    dest: &MPlaceTy<'tcx, Provenance>,
) -> InterpResult<'tcx, ()> {
    let (left, left_len) = this.operand_to_simd(left)?;
    let (right, right_len) = this.operand_to_simd(right)?;
    let (dest, dest_len) = this.mplace_to_simd(dest)?;

    assert_eq!(dest_len, left_len);
    assert_eq!(dest_len, right_len);

    for i in 0..dest_len {
        let dest = this.project_index(&dest, i)?;
        let left = this.read_immediate(&this.project_index(&left, i)?)?;
        let right = this.read_scalar(&this.project_index(&right, i)?)?.to_int(dest.layout.size)?;

        let res = this.wrapping_binary_op(
            mir::BinOp::Mul,
            &left,
            &ImmTy::from_int(right.signum(), dest.layout),
        )?;

        this.write_immediate(*res, &dest)?;
    }

    Ok(())
}
