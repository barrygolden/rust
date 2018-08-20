//! This module contains the `EvalContext` methods for executing a single step of the interpreter.
//!
//! The main entry point is the `step` method.

use rustc::mir;
use rustc::ty::layout::LayoutOf;
use rustc::mir::interpret::{EvalResult, Scalar};

use super::{EvalContext, Machine};

impl<'a, 'mir, 'tcx, M: Machine<'mir, 'tcx>> EvalContext<'a, 'mir, 'tcx, M> {
    pub fn inc_step_counter_and_detect_loops(&mut self) -> EvalResult<'tcx, ()> {
        /// The number of steps between loop detector snapshots.
        /// Should be a power of two for performance reasons.
        const DETECTOR_SNAPSHOT_PERIOD: isize = 256;

        {
            let steps = &mut self.steps_since_detector_enabled;

            *steps += 1;
            if *steps < 0 {
                return Ok(());
            }

            *steps %= DETECTOR_SNAPSHOT_PERIOD;
            if *steps != 0 {
                return Ok(());
            }
        }

        if self.loop_detector.is_empty() {
            // First run of the loop detector

            // FIXME(#49980): make this warning a lint
            self.tcx.sess.span_warn(self.frame().span,
                "Constant evaluating a complex constant, this might take some time");
        }

        self.loop_detector.observe_and_analyze(&self.machine, &self.stack, &self.memory)
    }

    /// Returns true as long as there are more things to do.
    pub fn step(&mut self) -> EvalResult<'tcx, bool> {
        if self.stack.is_empty() {
            return Ok(false);
        }

        let block = self.frame().block;
        let stmt_id = self.frame().stmt;
        let mir = self.mir();
        let basic_block = &mir.basic_blocks()[block];

        let old_frames = self.cur_frame();

        if let Some(stmt) = basic_block.statements.get(stmt_id) {
            assert_eq!(old_frames, self.cur_frame());
            self.statement(stmt)?;
            return Ok(true);
        }

        self.inc_step_counter_and_detect_loops()?;

        let terminator = basic_block.terminator();
        assert_eq!(old_frames, self.cur_frame());
        self.terminator(terminator)?;
        Ok(true)
    }

    fn statement(&mut self, stmt: &mir::Statement<'tcx>) -> EvalResult<'tcx> {
        debug!("{:?}", stmt);

        use rustc::mir::StatementKind::*;

        // Some statements (e.g. box) push new stack frames.  We have to record the stack frame number
        // *before* executing the statement.
        let frame_idx = self.cur_frame();
        self.tcx.span = stmt.source_info.span;
        self.memory.tcx.span = stmt.source_info.span;

        match stmt.kind {
            Assign(ref place, ref rvalue) => self.eval_rvalue_into_place(rvalue, place)?,

            SetDiscriminant {
                ref place,
                variant_index,
            } => {
                let dest = self.eval_place(place)?;
                self.write_discriminant_value(variant_index, dest)?;
            }

            // Mark locals as alive
            StorageLive(local) => {
                let old_val = self.storage_live(local)?;
                self.deallocate_local(old_val)?;
            }

            // Mark locals as dead
            StorageDead(local) => {
                let old_val = self.storage_dead(local);
                self.deallocate_local(old_val)?;
            }

            // No dynamic semantics attached to `ReadForMatch`; MIR
            // interpreter is solely intended for borrowck'ed code.
            ReadForMatch(..) => {}

            // Validity checks.
            Validate(op, ref places) => {
                for operand in places {
                    M::validation_op(self, op, operand)?;
                }
            }
            EndRegion(ce) => {
                M::end_region(self, Some(ce))?;
            }

            UserAssertTy(..) => {}

            // Defined to do nothing. These are added by optimization passes, to avoid changing the
            // size of MIR constantly.
            Nop => {}

            InlineAsm { .. } => return err!(InlineAsm),
        }

        self.stack[frame_idx].stmt += 1;
        Ok(())
    }

    /// Evaluate an assignment statement.
    ///
    /// There is no separate `eval_rvalue` function. Instead, the code for handling each rvalue
    /// type writes its results directly into the memory specified by the place.
    fn eval_rvalue_into_place(
        &mut self,
        rvalue: &mir::Rvalue<'tcx>,
        place: &mir::Place<'tcx>,
    ) -> EvalResult<'tcx> {
        let dest = self.eval_place(place)?;

        use rustc::mir::Rvalue::*;
        match *rvalue {
            Use(ref operand) => {
                // Avoid recomputing the layout
                let op = self.eval_operand(operand, Some(dest.layout))?;
                self.copy_op(op, dest)?;
            }

            BinaryOp(bin_op, ref left, ref right) => {
                let left = self.eval_operand_and_read_valty(left)?;
                let right = self.eval_operand_and_read_valty(right)?;
                self.binop_ignore_overflow(
                    bin_op,
                    left,
                    right,
                    dest,
                )?;
            }

            CheckedBinaryOp(bin_op, ref left, ref right) => {
                let left = self.eval_operand_and_read_valty(left)?;
                let right = self.eval_operand_and_read_valty(right)?;
                self.binop_with_overflow(
                    bin_op,
                    left,
                    right,
                    dest,
                )?;
            }

            UnaryOp(un_op, ref operand) => {
                let val = self.eval_operand_and_read_scalar(operand)?;
                let val = self.unary_op(un_op, val.not_undef()?, dest.layout)?;
                self.write_scalar(val, dest)?;
            }

            Aggregate(ref kind, ref operands) => {
                let (dest, active_field_index) = match **kind {
                    mir::AggregateKind::Adt(adt_def, variant_index, _, active_field_index) => {
                        self.write_discriminant_value(variant_index, dest)?;
                        if adt_def.is_enum() {
                            (self.place_downcast(dest, variant_index)?, active_field_index)
                        } else {
                            (dest, active_field_index)
                        }
                    }
                    _ => (dest, None)
                };

                for (i, operand) in operands.iter().enumerate() {
                    let op = self.eval_operand(operand, None)?;
                    // Ignore zero-sized fields.
                    if !op.layout.is_zst() {
                        let field_index = active_field_index.unwrap_or(i);
                        let field_dest = self.place_field(dest, field_index as u64)?;
                        self.copy_op(op, field_dest)?;
                    }
                }
            }

            Repeat(ref operand, _) => {
                let op = self.eval_operand(operand, None)?;
                let dest = self.force_allocation(dest)?;
                let length = dest.len();

                if length > 0 {
                    // write the first
                    let first = self.mplace_field(dest, 0)?;
                    self.copy_op(op, first.into())?;

                    if length > 1 {
                        // copy the rest
                        let (dest, dest_align) = first.to_scalar_ptr_align();
                        let rest = dest.ptr_offset(first.layout.size, &self)?;
                        self.memory.copy_repeatedly(
                            dest, dest_align, rest, dest_align, first.layout.size, length - 1, true
                        )?;
                    }
                }
            }

            Len(ref place) => {
                // FIXME(CTFE): don't allow computing the length of arrays in const eval
                let src = self.eval_place(place)?;
                let mplace = self.force_allocation(src)?;
                let len = mplace.len();
                let size = self.memory.pointer_size().bytes() as u8;
                self.write_scalar(
                    Scalar::Bits {
                        bits: len as u128,
                        size,
                    },
                    dest,
                )?;
            }

            Ref(_, _, ref place) => {
                let src = self.eval_place(place)?;
                let val = self.force_allocation(src)?.to_ref(&self);
                self.write_value(val, dest)?;
            }

            NullaryOp(mir::NullOp::Box, _) => {
                M::box_alloc(self, dest)?;
            }

            NullaryOp(mir::NullOp::SizeOf, ty) => {
                let ty = self.monomorphize(ty, self.substs());
                let layout = self.layout_of(ty)?;
                assert!(!layout.is_unsized(),
                        "SizeOf nullary MIR operator called for unsized type");
                let size = self.memory.pointer_size().bytes() as u8;
                self.write_scalar(
                    Scalar::Bits {
                        bits: layout.size.bytes() as u128,
                        size,
                    },
                    dest,
                )?;
            }

            Cast(kind, ref operand, cast_ty) => {
                debug_assert_eq!(self.monomorphize(cast_ty, self.substs()), dest.layout.ty);
                let src = self.eval_operand(operand, None)?;
                self.cast(src, kind, dest)?;
            }

            Discriminant(ref place) => {
                let place = self.eval_place(place)?;
                let discr_val = self.read_discriminant_value(self.place_to_op(place)?)?;
                let size = dest.layout.size.bytes() as u8;
                self.write_scalar(Scalar::Bits {
                    bits: discr_val,
                    size,
                }, dest)?;
            }
        }

        self.dump_place(*dest);

        Ok(())
    }

    fn terminator(&mut self, terminator: &mir::Terminator<'tcx>) -> EvalResult<'tcx> {
        debug!("{:?}", terminator.kind);
        self.tcx.span = terminator.source_info.span;
        self.memory.tcx.span = terminator.source_info.span;
        self.eval_terminator(terminator)?;
        if !self.stack.is_empty() {
            debug!("// {:?}", self.frame().block);
        }
        Ok(())
    }
}
