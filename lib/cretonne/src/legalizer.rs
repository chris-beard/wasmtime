//! Legalize instructions.
//!
//! A legal instruction is one that can be mapped directly to a machine code instruction for the
//! target ISA. The `legalize_function()` function takes as input any function and transforms it
//! into an equivalent function using only legal instructions.
//!
//! The characteristics of legal instructions depend on the target ISA, so any given instruction
//! can be legal for one ISA and illegal for another.
//!
//! Besides transforming instructions, the legalizer also fills out the `function.encodings` map
//! which provides a legal encoding recipe for every instruction.
//!
//! The legalizer does not deal with register allocation constraints. These constraints are derived
//! from the encoding recipes, and solved later by the register allocator.

use abi::{legalize_abi_value, ValueConversion};
use ir::{Function, Cursor, DataFlowGraph, InstructionData, Opcode, Inst, InstBuilder, Ebb, Type,
         Value, Signature, SigRef, ArgumentType};
use ir::condcodes::IntCC;
use ir::instructions::CallInfo;
use isa::{TargetIsa, Legalize};

/// Legalize `func` for `isa`.
///
/// - Transform any instructions that don't have a legal representation in `isa`.
/// - Fill out `func.encodings`.
///
pub fn legalize_function(func: &mut Function, isa: &TargetIsa) {
    legalize_signatures(func, isa);

    // TODO: This is very simplified and incomplete.
    func.encodings.resize(func.dfg.num_insts());
    let mut pos = Cursor::new(&mut func.layout);
    while let Some(_ebb) = pos.next_ebb() {
        // Keep track of the cursor position before the instruction being processed, so we can
        // double back when replacing instructions.
        let mut prev_pos = pos.position();

        while let Some(inst) = pos.next_inst() {
            let opcode = func.dfg[inst].opcode();

            // Check for ABI boundaries that need to be converted to the legalized signature.
            if opcode.is_call() && handle_call_abi(&mut func.dfg, &mut pos) {
                // Go back and legalize the inserted argument conversion instructions.
                pos.set_position(prev_pos);
                continue;
            }

            if opcode.is_return() && handle_return_abi(&mut func.dfg, &mut pos, &func.signature) {
                // Go back and legalize the inserted return value conversion instructions.
                pos.set_position(prev_pos);
                continue;
            }

            match isa.encode(&func.dfg, &func.dfg[inst]) {
                Ok(encoding) => *func.encodings.ensure(inst) = encoding,
                Err(action) => {
                    // We should transform the instruction into legal equivalents.
                    // Possible strategies are:
                    // 1. Legalize::Expand: Expand instruction into sequence of legal instructions.
                    //    Possibly iteratively. ()
                    // 2. Legalize::Narrow: Split the controlling type variable into high and low
                    //    parts. This applies both to SIMD vector types which can be halved and to
                    //    integer types such as `i64` used on a 32-bit ISA. ().
                    // 3. TODO: Promote the controlling type variable to a larger type. This
                    //    typically means expressing `i8` and `i16` arithmetic in terms if `i32`
                    //    operations on RISC targets. (It may or may not be beneficial to promote
                    //    small vector types versus splitting them.)
                    // 4. TODO: Convert to library calls. For example, floating point operations on
                    //    an ISA with no IEEE 754 support.
                    let changed = match action {
                        Legalize::Expand => expand(&mut pos, &mut func.dfg),
                        Legalize::Narrow => narrow(&mut pos, &mut func.dfg),
                    };
                    // If the current instruction was replaced, we need to double back and revisit
                    // the expanded sequence. This is both to assign encodings and possible to
                    // expand further.
                    // There's a risk of infinite looping here if the legalization patterns are
                    // unsound. Should we attempt to detect that?
                    if changed {
                        pos.set_position(prev_pos);
                    }
                }
            }

            // Remember this position in case we need to double back.
            prev_pos = pos.position();
        }
    }
}

// Include legalization patterns that were generated by `gen_legalizer.py` from the `XForms` in
// `meta/cretonne/legalize.py`.
//
// Concretely, this defines private functions `narrow()`, and `expand()`.
include!(concat!(env!("OUT_DIR"), "/legalizer.rs"));

/// Legalize all the function signatures in `func`.
///
/// This changes all signatures to be ABI-compliant with full `ArgumentLoc` annotations. It doesn't
/// change the entry block arguments, calls, or return instructions, so this can leave the function
/// in a state with type discrepancies.
fn legalize_signatures(func: &mut Function, isa: &TargetIsa) {
    isa.legalize_signature(&mut func.signature);
    for sig in func.dfg.signatures.keys() {
        isa.legalize_signature(&mut func.dfg.signatures[sig]);
    }

    if let Some(entry) = func.layout.entry_block() {
        legalize_entry_arguments(func, entry);
    }
}

/// Legalize the entry block arguments after `func`'s signature has been legalized.
///
/// The legalized signature may contain more arguments than the original signature, and the
/// argument types have been changed. This function goes through the arguments to the entry EBB and
/// replaces them with arguments of the right type for the ABI.
///
/// The original entry EBB arguments are computed from the new ABI arguments by code inserted at
/// the top of the entry block.
fn legalize_entry_arguments(func: &mut Function, entry: Ebb) {
    // Insert position for argument conversion code.
    // We want to insert instructions before the first instruction in the entry block.
    // If the entry block is empty, append instructions to it instead.
    let mut pos = Cursor::new(&mut func.layout);
    pos.goto_top(entry);
    pos.next_inst();

    // Keep track of the argument types in the ABI-legalized signature.
    let abi_types = &func.signature.argument_types;
    let mut abi_arg = 0;

    // Process the EBB arguments one at a time, possibly replacing one argument with multiple new
    // ones. We do this by detaching the entry EBB arguments first.
    let mut next_arg = func.dfg.take_ebb_args(entry);
    while let Some(arg) = next_arg {
        // Get the next argument before we mutate `arg`.
        next_arg = func.dfg.next_ebb_arg(arg);

        let arg_type = func.dfg.value_type(arg);
        if arg_type == abi_types[abi_arg].value_type {
            // No value translation is necessary, this argument matches the ABI type.
            // Just use the original EBB argument value. This is the most common case.
            func.dfg.put_ebb_arg(entry, arg);
            abi_arg += 1;
        } else {
            // Compute the value we want for `arg` from the legalized ABI arguments.
            let converted = convert_from_abi(&mut func.dfg,
                                             &mut pos,
                                             entry,
                                             &mut abi_arg,
                                             abi_types,
                                             arg_type);
            // The old `arg` is no longer an attached EBB argument, but there are probably still
            // uses of the value. Make it an alias to the converted value.
            func.dfg.change_to_alias(arg, converted);
        }
    }
}

/// Compute original value of type `ty` from the legalized ABI arguments beginning at `abi_arg`.
///
/// Update `abi_arg` to reflect the ABI arguments consumed and return the computed value.
fn convert_from_abi(dfg: &mut DataFlowGraph,
                    pos: &mut Cursor,
                    entry: Ebb,
                    abi_arg: &mut usize,
                    abi_types: &[ArgumentType],
                    ty: Type)
                    -> Value {
    // Terminate the recursion when we get the desired type.
    if ty == abi_types[*abi_arg].value_type {
        return dfg.append_ebb_arg(entry, ty);
    }

    // Reconstruct how `ty` was legalized into the argument at `abi_arg`.
    let conversion = legalize_abi_value(ty, &abi_types[*abi_arg]);

    // The conversion describes value to ABI argument. We implement the reverse conversion here.
    match conversion {
        // Construct a `ty` by concatenating two ABI integers.
        ValueConversion::IntSplit => {
            let abi_ty = ty.half_width().expect("Invalid type for conversion");
            let lo = convert_from_abi(dfg, pos, entry, abi_arg, abi_types, abi_ty);
            let hi = convert_from_abi(dfg, pos, entry, abi_arg, abi_types, abi_ty);
            dfg.ins(pos).iconcat_lohi(lo, hi)
        }
        // Construct a `ty` by concatenating two halves of a vector.
        ValueConversion::VectorSplit => {
            let abi_ty = ty.half_vector().expect("Invalid type for conversion");
            let lo = convert_from_abi(dfg, pos, entry, abi_arg, abi_types, abi_ty);
            let hi = convert_from_abi(dfg, pos, entry, abi_arg, abi_types, abi_ty);
            dfg.ins(pos).vconcat(lo, hi)
        }
        // Construct a `ty` by bit-casting from an integer type.
        ValueConversion::IntBits => {
            assert!(!ty.is_int());
            let abi_ty = Type::int(ty.bits()).expect("Invalid type for conversion");
            let arg = convert_from_abi(dfg, pos, entry, abi_arg, abi_types, abi_ty);
            dfg.ins(pos).bitcast(ty, arg)
        }
        // ABI argument is a sign-extended version of the value we want.
        ValueConversion::Sext(abi_ty) => {
            let arg = convert_from_abi(dfg, pos, entry, abi_arg, abi_types, abi_ty);
            // TODO: Currently, we don't take advantage of the ABI argument being sign-extended.
            // We could insert an `assert_sreduce` which would fold with a following `sextend` of
            // this value.
            dfg.ins(pos).ireduce(ty, arg)
        }
        ValueConversion::Uext(abi_ty) => {
            let arg = convert_from_abi(dfg, pos, entry, abi_arg, abi_types, abi_ty);
            // TODO: Currently, we don't take advantage of the ABI argument being sign-extended.
            // We could insert an `assert_ureduce` which would fold with a following `uextend` of
            // this value.
            dfg.ins(pos).ireduce(ty, arg)
        }
    }
}

/// Convert `value` to match an ABI signature by inserting instructions at `pos`.
///
/// This may require expanding the value to multiple ABI arguments. The conversion process is
/// recursive and controlled by the `put_arg` closure. When a candidate argument value is presented
/// to the closure, it will perform one of two actions:
///
/// 1. If the suggested argument has an acceptable value type, consume it by adding it to the list
///    of arguments and return `None`.
/// 2. If the suggested argument doesn't have the right value type, don't change anything, but
///    return the `ArgumentType` that is needed.
///
fn convert_to_abi<PutArg>(dfg: &mut DataFlowGraph,
                          pos: &mut Cursor,
                          value: Value,
                          put_arg: &mut PutArg)
    where PutArg: FnMut(&mut DataFlowGraph, Value) -> Option<ArgumentType>
{
    // Start by invoking the closure to either terminate the recursion or get the argument type
    // we're trying to match.
    let arg_type = match put_arg(dfg, value) {
        None => return,
        Some(t) => t,
    };

    let ty = dfg.value_type(value);
    match legalize_abi_value(ty, &arg_type) {
        ValueConversion::IntSplit => {
            let (lo, hi) = dfg.ins(pos).isplit_lohi(value);
            convert_to_abi(dfg, pos, lo, put_arg);
            convert_to_abi(dfg, pos, hi, put_arg);
        }
        ValueConversion::VectorSplit => {
            let (lo, hi) = dfg.ins(pos).vsplit(value);
            convert_to_abi(dfg, pos, lo, put_arg);
            convert_to_abi(dfg, pos, hi, put_arg);
        }
        ValueConversion::IntBits => {
            assert!(!ty.is_int());
            let abi_ty = Type::int(ty.bits()).expect("Invalid type for conversion");
            let arg = dfg.ins(pos).bitcast(abi_ty, value);
            convert_to_abi(dfg, pos, arg, put_arg);
        }
        ValueConversion::Sext(abi_ty) => {
            let arg = dfg.ins(pos).sextend(abi_ty, value);
            convert_to_abi(dfg, pos, arg, put_arg);
        }
        ValueConversion::Uext(abi_ty) => {
            let arg = dfg.ins(pos).uextend(abi_ty, value);
            convert_to_abi(dfg, pos, arg, put_arg);
        }
    }
}

/// Check if a sequence of arguments match a desired sequence of argument types.
fn check_arg_types<Args>(dfg: &DataFlowGraph, args: Args, types: &[ArgumentType]) -> bool
    where Args: IntoIterator<Item = Value>
{
    let mut n = 0;
    for arg in args {
        match types.get(n) {
            Some(&ArgumentType { value_type, .. }) => {
                if dfg.value_type(arg) != value_type {
                    return false;
                }
            }
            None => return false,
        }
        n += 1
    }

    // Also verify that the number of arguments matches.
    n == types.len()
}

/// Check if the arguments of the call `inst` match the signature.
///
/// Returns `None` if the signature matches and no changes are needed, or `Some(sig_ref)` if the
/// signature doesn't match.
fn check_call_signature(dfg: &DataFlowGraph, inst: Inst) -> Option<SigRef> {
    // Extract the signature and argument values.
    let (sig_ref, args) = match dfg[inst].analyze_call(&dfg.value_lists) {
        CallInfo::Direct(func, args) => (dfg.ext_funcs[func].signature, args),
        CallInfo::Indirect(sig_ref, args) => (sig_ref, args),
        CallInfo::NotACall => panic!("Expected call, got {:?}", dfg[inst]),
    };
    let sig = &dfg.signatures[sig_ref];

    if check_arg_types(dfg, args.iter().cloned(), &sig.argument_types[..]) &&
       check_arg_types(dfg, dfg.inst_results(inst), &sig.return_types[..]) {
        // All types check out.
        None
    } else {
        // Call types need fixing.
        Some(sig_ref)
    }
}

/// Insert ABI conversion code for the arguments to the call or return instruction at `pos`.
///
/// - `abi_args` is the number of arguments that the ABI signature requires.
/// - `get_abi_type` is a closure that can provide the desired `ArgumentType` for a given ABI
///   argument number in `0..abi_args`.
///
fn legalize_inst_arguments<ArgType>(dfg: &mut DataFlowGraph,
                                    pos: &mut Cursor,
                                    abi_args: usize,
                                    mut get_abi_type: ArgType)
    where ArgType: FnMut(&DataFlowGraph, usize) -> ArgumentType
{
    let inst = pos.current_inst().expect("Cursor must point to a call instruction");

    // Lift the value list out of the call instruction so we modify it.
    let mut vlist = dfg[inst].take_value_list().expect("Call must have a value list");

    // The value list contains all arguments to the instruction, including the callee on an
    // indirect call which isn't part of the call arguments that must match the ABI signature.
    // Figure out how many fixed values are at the front of the list. We won't touch those.
    let fixed_values = dfg[inst].opcode().constraints().fixed_value_arguments();
    let have_args = vlist.len(&dfg.value_lists) - fixed_values;

    // Grow the value list to the right size and shift all the existing arguments to the right.
    // This lets us write the new argument values into the list without overwriting the old
    // arguments.
    //
    // Before:
    //
    //    <-->              fixed_values
    //        <-----------> have_args
    //   [FFFFOOOOOOOOOOOOO]
    //
    // After grow_at():
    //
    //    <-->                     fixed_values
    //               <-----------> have_args
    //        <------------------> abi_args
    //   [FFFF-------OOOOOOOOOOOOO]
    //               ^
    //               old_arg_offset
    //
    // After writing the new arguments:
    //
    //    <-->                     fixed_values
    //        <------------------> abi_args
    //   [FFFFNNNNNNNNNNNNNNNNNNNN]
    //
    vlist.grow_at(fixed_values, abi_args - have_args, &mut dfg.value_lists);
    let old_arg_offset = fixed_values + abi_args - have_args;

    let mut abi_arg = 0;
    for old_arg in 0..have_args {
        let old_value = vlist.get(old_arg_offset + old_arg, &dfg.value_lists).unwrap();
        convert_to_abi(dfg,
                       pos,
                       old_value,
                       &mut |dfg, arg| {
            let abi_type = get_abi_type(dfg, abi_arg);
            if dfg.value_type(arg) == abi_type.value_type {
                // This is the argument type we need.
                vlist.as_mut_slice(&mut dfg.value_lists)[fixed_values + abi_arg] = arg;
                abi_arg += 1;
                None
            } else {
                // Nope, `arg` needs to be converted.
                Some(abi_type)
            }
        });
    }

    // Put the modified value list back.
    dfg[inst].put_value_list(vlist);
}

/// Insert ABI conversion code before and after the call instruction at `pos`.
///
/// Instructions inserted before the call will compute the appropriate ABI values for the
/// callee's new ABI-legalized signature. The function call arguments are rewritten in place to
/// match the new signature.
///
/// Instructions will be inserted after the call to convert returned ABI values back to the
/// original return values. The call's result values will be adapted to match the new signature.
///
/// Returns `true` if any instructions were inserted.
fn handle_call_abi(dfg: &mut DataFlowGraph, pos: &mut Cursor) -> bool {
    let inst = pos.current_inst().expect("Cursor must point to a call instruction");

    // Start by checking if the argument types already match the signature.
    let sig_ref = match check_call_signature(dfg, inst) {
        None => return false,
        Some(s) => s,
    };

    // OK, we need to fix the call arguments to match the ABI signature.
    let abi_args = dfg.signatures[sig_ref].argument_types.len();
    legalize_inst_arguments(dfg,
                            pos,
                            abi_args,
                            |dfg, abi_arg| dfg.signatures[sig_ref].argument_types[abi_arg]);

    // TODO: Convert return values.

    // Yes, we changed stuff.
    true
}

/// Insert ABI conversion code before and after the call instruction at `pos`.
///
/// Return `true` if any instructions were inserted.
fn handle_return_abi(dfg: &mut DataFlowGraph, pos: &mut Cursor, sig: &Signature) -> bool {
    let inst = pos.current_inst().expect("Cursor must point to a return instruction");

    // Check if the returned types already match the signature.
    let fixed_values = dfg[inst].opcode().constraints().fixed_value_arguments();
    if check_arg_types(dfg,
                       dfg[inst]
                           .arguments(&dfg.value_lists)
                           .iter()
                           .skip(fixed_values)
                           .cloned(),
                       &sig.return_types[..]) {
        return false;
    }

    let abi_args = sig.return_types.len();
    legalize_inst_arguments(dfg, pos, abi_args, |_, abi_arg| sig.return_types[abi_arg]);

    // Yes, we changed stuff.
    true
}
