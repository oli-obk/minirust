# MiniRust well-formedness requirements

The various syntactic constructs of MiniRust (types, functions, ...) come with well-formedness requirements: certain invariants need to be satisfied for this to be considered a well-formed program.
The idea is that for well-formed programs, the `step` function will never panic.
Those requirements are defined in this file.

We also define the idea of a "value being well-formed at a type".
`decode` will only ever return well-formed values, and `encode` will never panic on a well-formed value.

Note that `check_wf` functions for testing well-formedness return `Result<()>` to pass information in case an error occured.

We use the following helper function to convert Boolean checks into this form.

```rust
fn ensure_wf(b: bool, msg: &str) -> Result<()> {
    if !b { throw_ill_formed!("{}", msg); }
    ret(())
}
```

## Well-formed layouts and types

```rust
impl IntType {
    fn check_wf(self) -> Result<()> {
        // In particular, this checks that the size is at least one byte.
        ensure_wf(self.size.bytes().is_power_of_two(), "IntType: size is not power of two")
    }
}

impl Layout {
    fn check_wf<T: Target>(self) -> Result<()> {
        // We do *not* require that size is a multiple of align!
        ensure_wf(T::valid_size(self.size), "Layout: size not valid")
    }
}

impl PtrType {
    fn check_wf<T: Target>(self) -> Result<()> {
        match self {
            PtrType::Ref { pointee, mutbl: _ } | PtrType::Box { pointee } => {
                pointee.check_wf::<T>()?;
            }
            PtrType::Raw | PtrType::FnPtr(_) => ()
        }

        ret(())
    }
}

impl Type {
    fn check_wf<T: Target>(self) -> Result<()> {
        use Type::*;

        // Ensure that the size is valid and a multiple of the alignment.
        let size = self.size::<T>();
        ensure_wf(T::valid_size(size), "Type: size not valid")?;
        let align = self.align::<T>();
        ensure_wf(size.bytes() % align.bytes() == 0, "Type: size is not multiple of alignment")?;

        match self {
            Int(int_type) => {
                int_type.check_wf()?;
            }
            Bool => (),
            Ptr(ptr_type) => {
                ptr_type.check_wf::<T>()?;
            }
            Tuple { mut fields, size, align: _ } => {
                // The fields must not overlap.
                // We check fields in the order of their (absolute) offsets.
                fields.sort_by_key(|(offset, _ty)| offset);
                let mut last_end = Size::ZERO;
                for (offset, ty) in fields {
                    // Recursively check the field type.
                    ty.check_wf::<T>()?;
                    // And ensure it fits after the one we previously checked.
                    ensure_wf(offset >= last_end, "Type::Tuple: overlapping fields")?;
                    last_end = offset + ty.size::<T>();
                }
                // And they must all fit into the size.
                // The size is in turn checked to be valid for `M`, and hence all offsets are valid, too.
                ensure_wf(size >= last_end, "Type::Tuple: size of fields is bigger than total size")?;
            }
            Array { elem, count } => {
                ensure_wf(count >= 0, "Type::Array: negative amount of elements")?;
                elem.check_wf::<T>()?;
            }
            Union { fields, size, chunks, align: _ } => {
                // The fields may overlap, but they must all fit the size.
                for (offset, ty) in fields {
                    ty.check_wf::<T>()?;
                    ensure_wf(
                        size >= offset + ty.size::<T>(),
                        "Type::Union: field size does not fit union",
                    )?;
                    // This field may overlap with gaps between the chunks. That's perfectly normal
                    // when there is padding inside the field.
                    // FIXME: should we check that all the non-padding bytes of the field are in some chunk?
                    // But then we'd have to add a definition of "used (non-padding) bytes" in the spec, and then
                    // we may as well remove 'chunks' entirely and just compute the set of used bytes for
                    // encoding/decoding...
                }
                // The chunks must be sorted in their offsets and disjoint.
                // FIXME: should we relax this and allow arbitrary chunk order?
                let mut last_end = Size::ZERO;
                for (offset, size) in chunks {
                    ensure_wf(
                        offset >= last_end,
                        "Type::Union: chunks are not stored in ascending order",
                    )?;
                    last_end = offset + size;
                }
                // And they must all fit into the size.
                ensure_wf(size >= last_end, "Type::Union: chunks do not fit union")?;
            }
            Enum { variants, size, discriminator, discriminant_ty, .. } => {
                // All the variants need to be well-formed and be the size of the enum so
                // we don't have to handle different sizes in the memory representation.
                // Also their alignment may not be larger than the total enum alignment and
                // all the values written by the tagger must fit into the variant.
                for (discriminant, variant) in variants {
                    ensure_wf(
                        discriminant_ty.can_represent(discriminant),
                        "Type::Enum: invalid value for discriminant"
                    )?;

                    variant.ty.check_wf::<T>()?;
                    ensure_wf(
                        size == variant.ty.size::<T>(),
                        "Type::Enum: variant size is not the same as enum size"
                    )?;
                    ensure_wf(
                        variant.ty.align::<T>().bytes() <= align.bytes(),
                       "Type::Enum: invalid align requirement"
                    )?;
                    for (offset, (value_type, value)) in variant.tagger {
                        value_type.check_wf()?;
                        ensure_wf(value_type.can_represent(value), "Type::Enum: invalid tagger value")?;
                        ensure_wf(offset + value_type.size <= size, "Type::Enum tagger type size too big for enum")?;
                    }
                    // FIXME: check that the values written by the tagger do not overlap.
                }

                // check that all variants reached by the discriminator are valid,
                // that it never performs out-of-bounds accesses and all discriminant values
                // can be represented by the discriminant type.
                discriminator.check_wf::<T>(size, variants)?;
            }
        }

        ret(())
    }
}

impl Discriminator {
    fn check_wf<T: Target>(self, size: Size, variants: Map<Int, Variant>) -> Result<()>  {
        match self {
            Discriminator::Known(discriminant) => ensure_wf(variants.get(discriminant).is_some(), "Discriminator: invalid discriminant"),
            Discriminator::Invalid => ret(()),
            Discriminator::Branch { offset, value_type, fallback, children } => {
                // Ensure that the value we branch on is stored in bounds and that all children all valid.
                value_type.check_wf()?;
                ensure_wf(offset + value_type.size <= size, "Discriminator: branch offset exceeds size")?;
                fallback.check_wf::<T>(size, variants)?;
                for (idx, ((start, end), discriminator)) in children.into_iter().enumerate() {
                    ensure_wf(value_type.can_represent(start), "Discriminator: invalid branch start bound")?;
                    // Since the end is exclusive we only need to represent the number before the end.
                    ensure_wf(value_type.can_represent(end - Int::ONE), "Discriminator: invalid branch end bound")?;
                    ensure_wf(start < end, "Discriminator: invalid bound values")?;
                    // Ensure that the ranges don't overlap.
                    ensure_wf(children.keys().enumerate().all(|(other_idx, (other_start, other_end))| 
                                other_end <= start || other_start >= end || idx == other_idx), "Discriminator: branch ranges overlap")?;
                    discriminator.check_wf::<T>(size, variants)?;
                }
                ret(())
            }
        }
    }
}
```

## Well-formed expressions

```rust
impl Constant {
    /// Check that the constant has the expected type.
    /// Assumes that `ty` has already been checked.
    fn check_wf<T: Target>(self, ty: Type, prog: Program) -> Result<()> {
        // For now, we only support integer and boolean literals and pointers.
        // TODO: add more.
        match (self, ty) {
            (Constant::Int(i), Type::Int(int_type)) => {
                ensure_wf(int_type.can_represent(i), "Constant::Int: invalid int value")?;
            }
            (Constant::Bool(_), Type::Bool) => (),
            (Constant::GlobalPointer(relocation), Type::Ptr(_)) => {
                relocation.check_wf(prog.globals)?;
            }
            (Constant::FnPointer(fn_name), Type::Ptr(_)) => {
                ensure_wf(prog.functions.contains_key(fn_name), "Constant::FnPointer: invalid function name")?;
            }
            (Constant::PointerWithoutProvenance(addr), Type::Ptr(_)) => {
                ensure_wf(
                    addr.in_bounds(Signedness::Unsigned, T::PTR_SIZE),
                    "Constant::PointerWithoutProvenance: pointer out-of-bounds"
                )?;
            }
            _ => throw_ill_formed!("Constant: value does not match type"),
        }

        ret(())
    }
}

impl ValueExpr {
    #[allow(unused_braces)]
    fn check_wf<T: Target>(self, locals: Map<LocalName, Type>, prog: Program) -> Result<Type> {
        use ValueExpr::*;
        ret(match self {
            Constant(value, ty) => {
                ty.check_wf::<T>()?;
                value.check_wf::<T>(ty, prog)?;
                ty
            }
            Tuple(exprs, t) => {
                t.check_wf::<T>()?;

                match t {
                    Type::Tuple { fields, .. } => {
                        ensure_wf(exprs.len() == fields.len(), "ValueExpr::Tuple: invalid number of tuple fields")?;
                        for (e, (_offset, ty)) in exprs.zip(fields) {
                            let checked = e.check_wf::<T>(locals, prog)?;
                            ensure_wf(checked == ty, "ValueExpr::Tuple: invalid tuple field type")?;
                        }
                    },
                    Type::Array { elem, count } => {
                        ensure_wf(exprs.len() == count, "ValueExpr::Tuple: invalid number of array elements")?;
                        for e in exprs {
                            let checked = e.check_wf::<T>(locals, prog)?;
                            ensure_wf(checked == elem, "ValueExpr::Tuple: invalid array element type")?;
                        }
                    },
                    _ => throw_ill_formed!("ValueExpr::Tuple: expression does not match type"),
                }

                t
            }
            Union { field, expr, union_ty } => {
                union_ty.check_wf::<T>()?;

                let Type::Union { fields, .. } = union_ty else {
                    throw_ill_formed!("ValueExpr::Union: invalid type")
                };

                ensure_wf(field < fields.len(), "ValueExpr::Union: invalid field length")?;
                let (_offset, ty) = fields[field];

                let checked = expr.check_wf::<T>(locals, prog)?;
                ensure_wf(checked == ty, "ValueExpr::Union: invalid field type")?;

                union_ty
            }
            Variant { discriminant, data, enum_ty } => {
                let Type::Enum { variants, .. } = enum_ty else { 
                    throw_ill_formed!("ValueExpr::Variant: invalid type")
                };
                enum_ty.check_wf::<T>()?;
                let Some(variant) = variants.get(discriminant) else {
                    throw_ill_formed!("ValueExpr::Variant: invalid discriminant");
                };

                let checked = data.check_wf::<T>(locals, prog)?;
                ensure_wf(checked == variant.ty, "ValueExpr::Variant: invalid type")?;
                enum_ty
            }
            GetDiscriminant { place } => {
                let Type::Enum { discriminant_ty, .. } = place.check_wf::<T>(locals, prog)? else {
                    throw_ill_formed!("ValueExpr::GetDiscriminant: invalid type");
                };
                Type::Int(discriminant_ty)
            }
            Load { source } => {
                source.check_wf::<T>(locals, prog)?
            }
            AddrOf { target, ptr_ty } => {
                target.check_wf::<T>(locals, prog)?;
                // No check of how the alignment changes here -- that is purely a runtime constraint.
                Type::Ptr(ptr_ty)
            }
            UnOp { operator, operand } => {
                use lang::UnOp::*;

                let operand = operand.check_wf::<T>(locals, prog)?;
                match operator {
                    Int(_int_op) => {
                        let Type::Int(int_ty) = operand else {
                            throw_ill_formed!("UnOp::Int: invalid operand");
                        };
                        Type::Int(int_ty)
                    }
                    Bool(_bool_op) => {
                        ensure_wf(matches!(operand, Type::Bool), "UnOp::Bool: invalid operand")?;
                        Type::Bool
                    }
                    Cast(cast_op) => {
                        use lang::CastOp::*;
                        match cast_op {
                            IntToInt(int_ty) => {
                                ensure_wf(matches!(operand, Type::Int(_)), "Cast::IntToInt: invalid operand")?;
                                Type::Int(int_ty)
                            }
                            BoolToInt(int_ty) => {
                                ensure_wf(matches!(operand, Type::Bool), "Cast::BoolToInt: invalid operand")?;
                                Type::Int(int_ty)
                            }
                            Transmute(new_ty) => {
                                new_ty
                            }
                        }
                    }
                }
            }
            BinOp { operator, left, right } => {
                use lang::BinOp::*;

                let left = left.check_wf::<T>(locals, prog)?;
                let right = right.check_wf::<T>(locals, prog)?;
                match operator {
                    Int(_int_op) => {
                        let Type::Int(int_ty) = left else {
                            throw_ill_formed!("BinOp::Int: invalid left type");
                        };
                        ensure_wf(right == Type::Int(int_ty), "BinOp::Int: invalid right type")?;
                        Type::Int(int_ty)
                    }
                    IntRel(_int_rel) => {
                        let Type::Int(int_ty) = left else {
                            throw_ill_formed!("BinOp::IntRel: invalid left type");
                        };
                        ensure_wf(right == Type::Int(int_ty), "BinOp::IntRel: invalid right type")?;
                        Type::Bool
                    }
                    PtrOffset { inbounds: _ } => {
                        ensure_wf(matches!(left, Type::Ptr(_)), "BinOp::PtrOffset: invalid left type")?;
                        ensure_wf(matches!(right, Type::Int(_)), "BinOp::PtrOffset: invalid right type")?;
                        left
                    }
                    Bool(_bool_op) => {
                        ensure_wf(matches!(left, Type::Bool), "BinOp::Bool: invalid left type")?;
                        ensure_wf(matches!(right, Type::Bool), "BinOp::Bool: invalid right type")?;
                        Type::Bool
                    }
                }
            }
        })
    }
}

impl PlaceExpr {
    fn check_wf<T: Target>(self, locals: Map<LocalName, Type>, prog: Program) -> Result<Type> {
        use PlaceExpr::*;
        ret(match self {
            Local(name) => {
                match locals.get(name) {
                    None => throw_ill_formed!("PlaceExpr::Local: unknown local name"),
                    Some(local) => local,
                }
            },
            Deref { operand, ty } => {
                let op_ty = operand.check_wf::<T>(locals, prog)?;
                ensure_wf(matches!(op_ty, Type::Ptr(_)), "PlaceExpr::Deref: invalid type")?;
                // No check of how the alignment changes here -- that is purely a runtime constraint.
                ty
            }
            Field { root, field } => {
                let root = root.check_wf::<T>(locals, prog)?;
                let (_offset, field_ty) = match root {
                    Type::Tuple { fields, .. } | Type::Union { fields, .. } => {
                        match fields.get(field) {
                            None => throw_ill_formed!("PlaceExpr::Field: invalid field"),
                            Some(field) => field,
                        }
                    }
                    _ => throw_ill_formed!("PlaceExpr::Field: expression does not match type"),
                };
                field_ty
            }
            Index { root, index } => {
                let root = root.check_wf::<T>(locals, prog)?;
                let index = index.check_wf::<T>(locals, prog)?;
                ensure_wf(matches!(index, Type::Int(_)), "PlaceExpr::Index: invalid index type")?;
                match root {
                    Type::Array { elem, .. } => elem,
                    _ => throw_ill_formed!("PlaceExpr::Index: expression does not match Array type"),
                }
            }
            Downcast { root, discriminant } => {
                let root = root.check_wf::<T>(locals, prog)?;
                match root {
                    // A valid downcast points to an existing variant.
                    Type::Enum { variants, .. } => {
                        let Some(variant) = variants.get(discriminant) else {
                            throw_ill_formed!("PlaceExpr::Downcast: invalid discriminant");
                        };
                        variant.ty
                    }
                    _ => throw_ill_formed!("PlaceExpr::Downcast: invalid root type"),
                }
            }
        })
    }
}

impl ArgumentExpr {
    fn check_wf<T: Target>(self, locals: Map<LocalName, Type>, prog: Program) -> Result<Type> {
        ret(match self {
            ArgumentExpr::ByValue(value) => value.check_wf::<T>(locals, prog)?,
            ArgumentExpr::InPlace(place) => place.check_wf::<T>(locals, prog)?
        })
    }
}
```

## Well-formed functions and programs

When checking functions, we track for each program point the set of live locals (and their type) at that point.
To handle cyclic CFGs, we track the set of live locals at the beginning of each basic block.
When we first encounter a block, we add the locals that are live on the "in" edge; when we encounter a block the second time, we require the set to be the same.

```rust
impl Statement {
    /// This returns the adjusted live local mapping after the statement.
    fn check_wf<T: Target>(
        self,
        mut live_locals: Map<LocalName, Type>,
        func: Function,
        prog: Program,
    ) -> Result<Map<LocalName, Type>> {
        use Statement::*;
        ret(match self {
            Assign { destination, source } => {
                let left = destination.check_wf::<T>(live_locals, prog)?;
                let right = source.check_wf::<T>(live_locals, prog)?;
                ensure_wf(left == right, "Statement::Assign: destination and source type differ")?;
                live_locals
            }
            SetDiscriminant { destination, value } => {
                let Type::Enum { variants, .. } = destination.check_wf::<T>(live_locals, prog)? else {
                    throw_ill_formed!("Statement::SetDiscriminant: invalid type");
                };
                // We don't ensure that we can actually represent the discriminant.
                // The well-formedness checks for the type just ensure that every discriminant
                // reached by the discriminator is valid, however there we don't require that every
                // variant is represented. Setting such an unrepresented discriminant would probably
                // result in an invalid value as either the discriminator returns
                // `Discriminator::Invalid` or another variant.
                // This is fine as SetDiscriminant does not guarantee that the enum is a valid value.
                if variants.get(value) == None {
                    throw_ill_formed!("Statement::SetDiscriminant: invalid discriminant write")
                }
                live_locals
            }
            Validate { place, fn_entry: _ } => {
                place.check_wf::<T>(live_locals, prog)?;
                live_locals
            }
            Deinit { place } => {
                place.check_wf::<T>(live_locals, prog)?;
                live_locals
            }
            StorageLive(local) => {
                // Look up the type in the function, and add it to the live locals.
                // Fail if it already is live.
                let Some(ty) = func.locals.get(local) else {
                    throw_ill_formed!("Statement::StorageLive: invalid local variable")
                };
                if live_locals.try_insert(local, ty).is_err() {
                    throw_ill_formed!("Statement::StorageLive: local already live");
                };
                live_locals
            }
            StorageDead(local) => {
                if local == func.ret || func.args.any(|arg_name| local == arg_name) {
                    // Trying to mark an argument or the return local as dead.
                    throw_ill_formed!("Statement::StorageDead: trying to mark argument or return local as dead");
                }
                if live_locals.remove(local) == None {
                    throw_ill_formed!("Statement::StorageDead: local already dead");
                };
                live_locals
            }
        })
    }
}

/// Predicate to indicate if integer bin-op can be used for atomic fetch operations.
/// Needed for atomic fetch operations.
/// 
/// We limit the binops that are allowed to be atomic based on current LLVM and Rust API exposures.
fn is_atomic_binop(op: IntBinOp) -> bool {
    use IntBinOp as B;
    match op {
        B::Add | B::Sub => true,
        _ => false
    }
}

impl Terminator {
    /// Returns the successor basic blocks that need to be checked next.
    fn check_wf<T: Target>(
        self,
        live_locals: Map<LocalName, Type>,
        prog: Program,
    ) -> Result<List<BbName>> {
        use Terminator::*;
        ret(match self {
            Goto(block_name) => {
                list![block_name]
            }
            Switch { value, cases, fallback } => {
                let ty = value.check_wf::<T>(live_locals, prog)?;
                let Type::Int(switch_ty) = ty else {
                    // We only switch on integers.
                    // This is in contrast to Rust MIR where switch can work on `char`s and booleans as well.
                    // However since those are trivial casts we chose to only accept integers.
                    throw_ill_formed!("Terminator::Switch: switch is not Int")
                };

                // ensures that all cases are valid and therefore can be reached from this block.
                let mut next_blocks = List::new();
                for (case, block) in cases.iter() {
                    ensure_wf(switch_ty.can_represent(case), "Terminator::Switch: invalid basic block name")?;
                    next_blocks.push(block);
                }

                // we can also reach the fallback block.
                next_blocks.push(fallback);
                next_blocks
            }
            Unreachable => {
                list![]
            }
            Call { callee, arguments, ret, next_block } => {
                let ty = callee.check_wf::<T>(live_locals, prog)?;
                ensure_wf(matches!(ty, Type::Ptr(PtrType::FnPtr(_))), "Terminator::Call: invalid type")?;

                // Return and argument expressions must all typecheck with some type.
                ret.check_wf::<T>(live_locals, prog)?;
                for arg in arguments {
                    arg.check_wf::<T>(live_locals, prog)?;
                }

                match next_block {
                    Some(b) => list![b],
                    None => list![],
                }
            }
            Intrinsic { intrinsic, arguments, ret, next_block } => {
                // Return and argument expressions must all typecheck with some type.
                ret.check_wf::<T>(live_locals, prog)?;
                for arg in arguments {
                    arg.check_wf::<T>(live_locals, prog)?;
                }

                // Currently only AtomicFetchAndOp has special well-formedness requirements.
                match intrinsic {
                    IntrinsicOp::AtomicFetchAndOp(op) => {
                        if !is_atomic_binop(op) {
                            throw_ill_formed!("IntrinsicOp::AtomicFetchAndOp: non atomic op");
                        }
                    }
                    _ => {}
                }

                match next_block {
                    Some(b) => list![b],
                    None => list![],
                }
            }
            Return => {
                list![]
            }
        })
    }
}

impl Function {
    fn check_wf<T: Target>(self, prog: Program) -> Result<()> {
        // Ensure all locals have a valid type.
        for ty in self.locals.values() {
            ty.check_wf::<T>()?;
        }

        // Construct initially live locals.
        // Also ensures that return and argument locals must exist.
        let mut start_live: Map<LocalName, Type> = Map::new();
        let Some(ret_ty) = self.locals.get(self.ret) else {
            throw_ill_formed!("Function: return local does not exist");
        };
        if start_live.try_insert(self.ret, ret_ty).is_err() {
            throw_ill_formed!("Function: invalid return local")
        };
        for arg in self.args {
            // Also ensures that no two arguments refer to the same local.
            let Some(arg_ty) = self.locals.get(arg) else {
                throw_ill_formed!("Function: invalid arg name");
            };
            if start_live.try_insert(arg, arg_ty).is_err() {
                throw_ill_formed!("Function: invalid arg");
            };
        }

        // Check the basic blocks. They can be cyclic, so we keep a worklist of
        // which blocks we still have to check. We also track the live locals
        // they start out with.
        let mut bb_live_at_entry: Map<BbName, Map<LocalName, Type>> = Map::new();
        bb_live_at_entry.insert(self.start, start_live);
        let mut todo = list![self.start];
        while let Some(block_name) = todo.pop_front() {
            let Some(block) = self.blocks.get(block_name) else {
                throw_ill_formed!("Function: invalid block name");
            };
            let mut live_locals = bb_live_at_entry[block_name];
            // Check this block, updating the live locals along the way.
            for statement in block.statements {
                live_locals = statement.check_wf::<T>(live_locals, self, prog)?;
            }
            let successors = block.terminator.check_wf::<T>(live_locals, prog)?;
            for block_name in successors {
                if let Some(precondition) = bb_live_at_entry.get(block_name) {
                    // A block we already visited (or already have in the worklist).
                    // Make sure the set of initially live locals is consistent!
                    ensure_wf(precondition == live_locals, "Function: set of live locals is not consistent")?;
                } else {
                    // A new block.
                    bb_live_at_entry.insert(block_name, live_locals);
                    todo.push(block_name);
                }
            }
        }

        // Ensure there are no dead blocks that we failed to reach.
        for block_name in self.blocks.keys() {
            ensure_wf(bb_live_at_entry.contains_key(block_name), "Function: unreached basic block")?;
        }

        ret(())
    }
}

impl Relocation {
    // Checks whether the relocation is within bounds.
    fn check_wf(self, globals: Map<GlobalName, Global>) -> Result<()> {
        // The global we are pointing to needs to exist.
        let Some(global) = globals.get(self.name) else {
            throw_ill_formed!("Relocation: invalid global name");
        };
        let size = Size::from_bytes(global.bytes.len()).unwrap();

        // And the offset needs to be in-bounds of its size.
        ensure_wf(self.offset <= size, "Relocation: offset out-of-bounds")?;

        ret(())
    }
}

impl Program {
    fn check_wf<T: Target>(self) -> Result<()> {
        // Ensure the start function exists, has the right ABI, takes no arguments, and returns a 1-ZST.
        let Some(func) = self.functions.get(self.start) else {
            throw_ill_formed!("Program: start function does not exist");
        };
        ensure_wf(func.calling_convention == CallingConvention::C, "Program: invalid calling convention")?;
        let Some(ret_local) = func.locals.get(func.ret) else {
            throw_ill_formed!("Program: start function has no return local");
        };
        let ret_layout = ret_local.layout::<T>();
        ensure_wf(
            ret_layout.size == Size::ZERO && ret_layout.align == Align::ONE,
            "Program: start function return local has invalid layout"
        )?;
        ensure_wf(func.args.is_empty(), "Program: supplied start function with arguments")?;

        // Check all the functions.
        for function in self.functions.values() {
            function.check_wf::<T>(self)?;
        }

        // Check globals.
        for (_name, global) in self.globals {
            let size = Size::from_bytes(global.bytes.len()).unwrap();
            for (offset, relocation) in global.relocations {
                // A relocation fills `PTR_SIZE` many bytes starting at the offset, those need to fit into the size.
                ensure_wf(offset + T::PTR_SIZE <= size, "Program: invalid global pointer value")?;

                relocation.check_wf(self.globals)?;
            }
        }

        ret(())
    }
}
```

## Well-formed values

```rust
impl<M: Memory> Value<M> {
    /// We assume `ty` is itself well-formed.
    fn check_wf(self, ty: Type) -> Result<()> {
        match (self, ty) {
            (Value::Int(i), Type::Int(ity)) => {
                ensure_wf(ity.can_represent(i), "Value::Int: invalid integer value")?;
            }
            (Value::Bool(_), Type::Bool) => {},
            (Value::Ptr(ptr), Type::Ptr(ptr_ty)) => {
                ensure_wf(ptr_ty.addr_valid(ptr.addr), "Value::Ptr: invalid pointer address")?;
                ensure_wf(ptr.addr.in_bounds(Unsigned, M::T::PTR_SIZE), "Value::Ptr: pointer out-of-bounds")?;
            }
            (Value::Tuple(vals), Type::Tuple { fields, .. }) => {
                ensure_wf(vals.len() == fields.len(), "Value::Tuple: invalid number of fields")?;
                for (val, (_, ty)) in vals.zip(fields) {
                    val.check_wf(ty)?;
                }
            }
            (Value::Tuple(vals), Type::Array { elem, count }) => {
                ensure_wf(vals.len() == count, "Value::Tuple: invalid number of elements")?;
                for val in vals {
                    val.check_wf(elem)?;
                }
            }
            (Value::Union(chunk_data), Type::Union { chunks, .. }) => {
                ensure_wf(chunk_data.len() == chunks.len(), "Value::Union: invalid chunk size")?;
                for (data, (_, size)) in chunk_data.zip(chunks) {
                    ensure_wf(data.len() == size.bytes(), "Value::Union: invalid chunk data")?;
                }
            }
            (Value::Variant { discriminant, data }, Type::Enum { variants, .. }) => {
                let Some(variant) = variants.get(discriminant) else {
                    throw_ill_formed!("Value::Variant: invalid discrimant type");
                };
                data.check_wf(variant.ty)?;
            }
            _ => throw_ill_formed!("Value: value does not match type")
        }

        ret(())
    }
}
```
