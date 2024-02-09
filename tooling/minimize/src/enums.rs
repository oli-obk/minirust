use crate::*;
mod rs {
    pub use crate::rs::*;
    pub use crate::rustc_target::abi::{Variants, FieldsShape, Primitive, TagEncoding, VariantIdx};
}

use crate::rustc_middle::ty::layout::PrimitiveExt;

pub fn translate_enum<'tcx>(
    ty: rs::Ty<'tcx>,
    adt_def: rs::AdtDef<'tcx>,
    sref: rs::GenericArgsRef<'tcx>,
    tcx: rs::TyCtxt<'tcx>,
) -> Type {
    let a = rs::ParamEnv::reveal_all().and(ty);
    let layout = tcx.layout_of(a).unwrap().layout;
    let size = translate_size(layout.size());
    let align = translate_align(layout.align().abi);

    let Type::Int(discriminant_ty) = translate_ty(ty.discriminant_ty(tcx), tcx) else {
        panic!("Discriminant type is not integer!")
    };

    let (variants, discriminator) = match layout.variants() {
        rs::Variants::Single { index } => {
            let fields = translate_fields(layout.fields(), adt_def.variant(*index), sref, tcx);
            let variants = [(Int::ZERO, Variant { ty: Type::Tuple { fields, size, align }, tagger: Map::new() })];
            let discriminator = Discriminator::Known(Int::ZERO);
            (variants.into_iter().collect::<Map<Int, Variant>>(), discriminator)
        },
        rs::Variants::Multiple {
            tag,
            tag_encoding,
            tag_field,
            variants,
        } => {

            // compute the offset of the tag for the tagger and discriminator construction
            let tag_offset: Offset = translate_size(layout.fields().offset(*tag_field));
            let Type::Int(tag_ty) = translate_ty(tag.primitive().to_int_ty(tcx), tcx) else {
                panic!("enum tag has invalid primitive type")
            };

            // translate the variants
            let mut translated_variants = Map::new();
            let mut discriminator_branches = Map::new();
            for (variant_idx, variant_def) in adt_def.variants().iter_enumerated() {
                let fields = translate_fields(&variants[variant_idx].fields, &variant_def, sref, tcx);
                let discr = adt_def.discriminant_for_variant(tcx, variant_idx);
                let discr_int = int_from_bits(discr.val, discriminant_ty);
                match tag_encoding {
                    rs::TagEncoding::Direct => {
                        // direct tagging places the discriminant in the tag for all variants
                        let tagger = [(tag_offset, (tag_ty, discr_int))].into_iter().collect::<Map<Offset, (IntType, Int)>>();
                        let variant = Variant { ty: Type::Tuple { fields, size, align }, tagger };
                        translated_variants.insert(discr_int, variant);
                        discriminator_branches.insert((discr_int, discr_int), Discriminator::Known(discr_int));
                    },
                    rs::TagEncoding::Niche { untagged_variant, niche_variants, niche_start } if *untagged_variant != variant_idx => {
                        // this is a tagged variant, meaning that it writes its tag and has a discriminator branch entry.
                        let discr_int = int_from_bits(discr.val, tag_ty);
                        let tag_int = (discr_int - Int::from(niche_variants.start().as_usize()) + Int::from(*niche_start)).modulo(tag_ty.signed, tag_ty.size);
                        let tagger = [(tag_offset, (tag_ty, tag_int))].into_iter().collect::<Map<_, _>>();
                        discriminator_branches.insert((tag_int, tag_int), Discriminator::Known(discr_int));
                        translated_variants.insert(discr_int, Variant { ty: Type::Tuple { fields, size, align }, tagger });
                    }
                    rs::TagEncoding::Niche { .. } => {
                        // this is the untagged variant
                        // we don't add it to the discriminator branches as it will be the fallback.
                        translated_variants.insert(discr_int, Variant { ty: Type::Tuple { fields, size, align }, tagger: Map::new() });
                    }
                };
            }

            let fallback = match tag_encoding {
                // Direct tagging: all other tag values are invalid.
                rs::TagEncoding::Direct => GcCow::new(Discriminator::Invalid),

                // Niche tagging: The fallback is the untagged variant.
                // We still need to add the invalid tag range to the children.
                rs::TagEncoding::Niche { untagged_variant, .. } => {
                    let tag_valid_range = tag.valid_range(&tcx);
                    let start = int_from_bits(tag_valid_range.start, tag_ty);
                    let end = int_from_bits(tag_valid_range.end, tag_ty);
                    if start <= end {
                        // The range of valid values is continuous, so the invalid values are between the ends of the range and the domain.
                        let rsize = rs::Size::from_bits(tag_ty.size.bits().try_to_u8().unwrap());
                        let min = if tag_ty.signed == Signedness::Signed { Int::from(rsize.signed_int_min()) } else { Int::ZERO };
                        let max = if tag_ty.signed == Signedness::Signed { Int::from(rsize.signed_int_max()) } else { Int::from(rsize.unsigned_int_max()) };
                        if end < max {
                            discriminator_branches.insert((end + Int::ONE, max), Discriminator::Invalid);
                        }
                        if min < start {
                            discriminator_branches.insert((min, start - Int::ONE), Discriminator::Invalid);
                        }
                    }
                    else if end + Int::ONE < start {
                        // The range of valid values wraps around, so the invalid values are between end and start (exclusive).
                        discriminator_branches.insert((end + Int::ONE, start - Int::ONE), Discriminator::Invalid);
                    } else {}

                    GcCow::new(Discriminator::Known(untagged_variant.as_usize().into()))
                }
            };
            let discriminator = Discriminator::Branch {
                offset: tag_offset,
                value_type: tag_ty,
                fallback,
                children: discriminator_branches
            };

            (translated_variants, discriminator)
        },
    };


    Type::Enum {
        variants,
        discriminator,
        discriminant_ty,
        size,
        align,
    }
}


/// Constructs the fields of a given variant.
fn translate_fields<'tcx>(
    shape: &rs::FieldsShape,
    variant: &rs::VariantDef,
    sref: rs::GenericArgsRef<'tcx>,
    tcx: rs::TyCtxt<'tcx>,
) -> List<(Offset, Type)> {
    variant.fields
           .iter_enumerated()
           .map(|(i, field)| {
                let ty = field.ty(tcx, sref);
                let ty = translate_ty(ty, tcx);
                let offset = shape.offset(i.into());
                let offset = translate_size(offset);

                (offset, ty)
    }).collect()
}

pub fn int_from_bits(bits: u128, ity: IntType) -> Int {
    let rs_size = rs::Size::from_bits(ity.size.bits().try_to_u8().unwrap());
    if ity.signed == Signedness::Unsigned {
        Int::from(rs_size.truncate(bits))
    } else {
        let signed_val = rs_size.sign_extend(bits) as i128;
        Int::from(signed_val)
    }
}

pub fn discriminant_for_variant<'tcx>(ty: rs::Ty<'tcx>, tcx: rs::TyCtxt<'tcx>, variant_idx: rs::VariantIdx) -> Int {
    let rs::TyKind::Adt(adt_def, _) = ty.kind() else {
        panic!("Getting discriminant for a variant of a non-enum type!")
    };
    assert!(adt_def.is_enum());
    let Type::Int(discriminant_ty) = translate_ty(ty.discriminant_ty(tcx), tcx) else {
        panic!("Discriminant type is not integer!")
    };
    let discriminant = adt_def.discriminant_for_variant(tcx, variant_idx);
    int_from_bits(discriminant.val, discriminant_ty)
}
