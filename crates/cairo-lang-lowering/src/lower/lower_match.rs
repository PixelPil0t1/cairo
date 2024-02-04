use std::vec;

use cairo_lang_debug::DebugWithDb;
use cairo_lang_filesystem::flag::Flag;
use cairo_lang_filesystem::ids::FlagId;
use cairo_lang_semantic as semantic;
use cairo_lang_semantic::corelib;
use cairo_lang_utils::try_extract_matches;
use cairo_lang_utils::unordered_hash_map::{Entry, UnorderedHashMap};
use itertools::{zip_eq, Itertools};
use num_traits::ToPrimitive;
use semantic::corelib::{core_felt252_ty, unit_ty};
use semantic::items::enm::SemanticEnumEx;
use semantic::types::{peel_snapshots, wrap_in_snapshots};
use semantic::{
    ConcreteTypeId, GenericArgumentId, MatchArmSelector, Pattern, PatternEnumVariant, PatternId,
    TypeLongId, ValueSelectorArm,
};

use super::block_builder::{BlockBuilder, SealedBlockBuilder};
use super::context::{
    lowering_flow_error_to_sealed_block, LoweredExpr, LoweredExprExternEnum, LoweringContext,
    LoweringFlowError,
};
use super::external::extern_facade_expr;
use super::{
    alloc_empty_block, generators, lower_expr_literal, lower_tail_expr,
    lowered_expr_to_block_scope_end,
};
use crate::diagnostic::LoweringDiagnosticKind::*;
use crate::ids::{LocationId, SemanticFunctionIdEx};
use crate::lower::context::{LoweringResult, VarRequest};
use crate::lower::{
    create_subscope, create_subscope_with_bound_refs, lower_expr, lower_single_pattern,
    match_extern_arm_ref_args_bind, match_extern_variant_arm_input_types,
};
use crate::{
    FlatBlockEnd, MatchArm, MatchEnumInfo, MatchEnumValue, MatchExternInfo, MatchInfo, VarUsage,
    VariableId,
};

/// Information about the enum of a match statement. See [extract_concrete_enum].
struct ExtractedEnumDetails {
    concrete_enum_id: semantic::ConcreteEnumId,
    concrete_variants: Vec<semantic::ConcreteVariant>,
    n_snapshots: usize,
}

/// Extracts concrete enum and variants from a match expression. Assumes it is indeed a concrete
/// enum.
fn extract_concrete_enum(
    ctx: &mut LoweringContext<'_, '_>,
    matched_expr: &semantic::Expr,
) -> Result<ExtractedEnumDetails, LoweringFlowError> {
    let ty = matched_expr.ty();
    let (n_snapshots, long_ty) = peel_snapshots(ctx.db.upcast(), ty);

    // Semantic model should have made sure the type is an enum.
    let TypeLongId::Concrete(ConcreteTypeId::Enum(concrete_enum_id)) = long_ty else {
        return Err(LoweringFlowError::Failed(ctx.diagnostics.report(
            matched_expr.stable_ptr().untyped(),
            UnsupportedMatchedType(long_ty.format(ctx.db.upcast())),
        )));
    };
    let concrete_variants =
        ctx.db.concrete_enum_variants(concrete_enum_id).map_err(LoweringFlowError::Failed)?;

    Ok(ExtractedEnumDetails { concrete_enum_id, concrete_variants, n_snapshots })
}

/// Extracts concrete enums and variants from a match expression on a tuple of enums.
fn extract_concrete_enum_tuple(
    ctx: &mut LoweringContext<'_, '_>,
    matched_expr: &semantic::Expr,
    types: &[semantic::TypeId],
) -> Result<Vec<ExtractedEnumDetails>, LoweringFlowError> {
    types
        .iter()
        .map(|ty| {
            let (n_snapshots, long_ty) = peel_snapshots(ctx.db.upcast(), *ty);
            let TypeLongId::Concrete(ConcreteTypeId::Enum(concrete_enum_id)) = long_ty else {
                return Err(LoweringFlowError::Failed(
                    ctx.diagnostics
                        .report(matched_expr.stable_ptr().untyped(), UnsupportedMatchedValueTuple),
                ));
            };
            let concrete_variants = ctx
                .db
                .concrete_enum_variants(concrete_enum_id)
                .map_err(LoweringFlowError::Failed)?;
            Ok(ExtractedEnumDetails { concrete_enum_id, concrete_variants, n_snapshots })
        })
        .collect()
}

/// The arm and pattern indices of a pattern in a match arm with an or list.
#[derive(Debug, Clone)]
struct PatternPath {
    arm_index: usize,
    pattern_index: usize,
}

/// Returns an option containing the PatternPath of the underscore pattern, if it exists.
fn get_underscore_pattern_path(
    ctx: &mut LoweringContext<'_, '_>,
    arms: &[semantic::MatchArm],
) -> Option<PatternPath> {
    let otherwise_variant = arms
        .iter()
        .enumerate()
        .map(|(arm_index, arm)| {
            arm.patterns
                .iter()
                .position(|pattern| {
                    matches!(ctx.function_body.patterns[*pattern], semantic::Pattern::Otherwise(_))
                })
                .map(|pattern_index| PatternPath { arm_index, pattern_index })
        })
        .find(|option| option.is_some())??;

    for arm in arms.iter().skip(otherwise_variant.arm_index + 1) {
        for pattern in arm.patterns.iter() {
            let pattern = ctx.function_body.patterns[*pattern].clone();
            ctx.diagnostics.report(pattern.stable_ptr().untyped(), UnreachableMatchArm);
        }
    }
    for pattern in
        arms[otherwise_variant.arm_index].patterns.iter().skip(otherwise_variant.pattern_index + 1)
    {
        let pattern = ctx.function_body.patterns[*pattern].clone();
        ctx.diagnostics.report(pattern.stable_ptr().untyped(), UnreachableMatchArm);
    }

    Some(otherwise_variant)
}

/// Returns a map from variants to their corresponding pattern path in a match statement.
fn get_variant_to_arm_map<'a>(
    ctx: &mut LoweringContext<'_, '_>,
    arms: impl Iterator<Item = &'a semantic::MatchArm>,
    concrete_enum_id: semantic::ConcreteEnumId,
) -> LoweringResult<UnorderedHashMap<semantic::ConcreteVariant, PatternPath>> {
    let mut map = UnorderedHashMap::default();
    for (arm_index, arm) in arms.enumerate() {
        for (pattern_index, pattern) in arm.patterns.iter().enumerate() {
            let pattern = ctx.function_body.patterns[*pattern].clone();

            if let semantic::Pattern::Otherwise(_) = pattern {
                break;
            }

            let enum_pattern = try_extract_matches!(&pattern, semantic::Pattern::EnumVariant)
                .ok_or_else(|| {
                    LoweringFlowError::Failed(
                        ctx.diagnostics
                            .report(pattern.stable_ptr().untyped(), UnsupportedMatchArmNotAVariant),
                    )
                })?
                .clone();

            if enum_pattern.variant.concrete_enum_id != concrete_enum_id {
                return Err(LoweringFlowError::Failed(
                    ctx.diagnostics
                        .report(pattern.stable_ptr().untyped(), UnsupportedMatchArmNotAVariant),
                ));
            }

            match map.entry(enum_pattern.variant.clone()) {
                Entry::Occupied(_) => {
                    ctx.diagnostics.report(pattern.stable_ptr().untyped(), UnreachableMatchArm);
                }
                Entry::Vacant(entry) => {
                    entry.insert(PatternPath { arm_index, pattern_index });
                }
            };
        }
    }
    Ok(map)
}

/// Represents a path in a match tree.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
struct MatchingPath {
    /// The variants per member of the tuple matched until this point.
    variants: Vec<semantic::ConcreteVariant>,
}

/// A helper function for [get_variants_to_arm_map_tuple] Inserts the pattern path to the map for
/// each variants list it can match.
fn insert_tuple_path_patterns(
    ctx: &mut LoweringContext<'_, '_>,
    patterns: &[PatternId],
    pattern_path: &PatternPath,
    extracted_enums_details: &[ExtractedEnumDetails],
    mut path: MatchingPath,
    map: &mut UnorderedHashMap<MatchingPath, PatternPath>,
) -> LoweringResult<()> {
    let index = path.variants.len();

    // if the path is the same length as the tuple's patterns, we have reached the end of the path
    if index == patterns.len() {
        match map.entry(path) {
            Entry::Occupied(_) => {}
            Entry::Vacant(entry) => {
                entry.insert(pattern_path.clone());
            }
        };
        return Ok(());
    }

    let pattern = ctx.function_body.patterns[patterns[index]].clone();

    match pattern {
        Pattern::EnumVariant(enum_pattern) => {
            if enum_pattern.variant.concrete_enum_id
                != extracted_enums_details[index].concrete_enum_id
            {
                return Err(LoweringFlowError::Failed(
                    ctx.diagnostics
                        .report(enum_pattern.stable_ptr.untyped(), UnsupportedMatchArmNotAVariant),
                ));
            }
            path.variants.push(enum_pattern.variant);
            insert_tuple_path_patterns(
                ctx,
                patterns,
                pattern_path,
                extracted_enums_details,
                path,
                map,
            )
        }
        Pattern::Otherwise(_) => {
            extracted_enums_details[index].concrete_variants.iter().try_for_each(|variant| {
                // TODO(TomerStarkware): Remove the match on the variant options in this case if
                // there's no other conflicting arm.
                let mut path = path.clone();
                path.variants.push(variant.clone());
                insert_tuple_path_patterns(
                    ctx,
                    patterns,
                    pattern_path,
                    extracted_enums_details,
                    path,
                    map,
                )
            })
        }
        _ => Err(LoweringFlowError::Failed(
            ctx.diagnostics.report(pattern.stable_ptr().untyped(), UnsupportedMatchArmNotAVariant),
        )),
    }
}

/// Returns a map from a matching paths to their corresponding pattern path in a match statement.
fn get_variants_to_arm_map_tuple<'a>(
    ctx: &mut LoweringContext<'_, '_>,
    arms: impl Iterator<Item = &'a semantic::MatchArm>,
    extracted_enums_details: &[ExtractedEnumDetails],
) -> LoweringResult<UnorderedHashMap<MatchingPath, PatternPath>> {
    let mut map = UnorderedHashMap::default();
    for (arm_index, arm) in arms.enumerate() {
        for (pattern_index, pattern) in arm.patterns.iter().enumerate() {
            let pattern = ctx.function_body.patterns[*pattern].clone();
            if let semantic::Pattern::Otherwise(_) = pattern {
                break;
            }
            let patterns =
                try_extract_matches!(&pattern, semantic::Pattern::Tuple).ok_or_else(|| {
                    LoweringFlowError::Failed(
                        ctx.diagnostics
                            .report(pattern.stable_ptr().untyped(), UnsupportedMatchArmNotAVariant),
                    )
                })?;

            let map_size = map.len();
            insert_tuple_path_patterns(
                ctx,
                &patterns.field_patterns,
                &PatternPath { arm_index, pattern_index },
                extracted_enums_details,
                MatchingPath::default(),
                &mut map,
            )?;
            if map.len() == map_size {
                ctx.diagnostics.report(pattern.stable_ptr().untyped(), UnreachableMatchArm);
            }
        }
    }
    Ok(map)
}

/// Information needed to lower a match on tuple expression.
struct LoweringMatchTupleContext {
    /// The location of the match expression.
    match_location: LocationId,
    /// The index of the underscore pattern, if it exists.
    otherwise_variant: Option<PatternPath>,
    /// A map from variants vector to their corresponding pattern path.
    variants_map: UnorderedHashMap<MatchingPath, PatternPath>,
    /// The tuple's destructured inputs.
    match_inputs: Vec<VarUsage>,
    /// The number of snapshots of the tuple.
    n_snapshots_outer: usize,
    /// The current variants path.
    current_path: MatchingPath,
    /// The current variants' variable ids.
    current_var_ids: Vec<VariableId>,
}

/// Lowers the arm of a match on a tuple expression.
fn lower_tuple_match_arm(
    ctx: &mut LoweringContext<'_, '_>,
    mut builder: BlockBuilder,
    arms: &[semantic::MatchArm],
    match_tuple_ctx: &mut LoweringMatchTupleContext,
    leaves_builders: &mut Vec<MatchLeafBuilder>,
) -> LoweringResult<()> {
    let pattern_path = match_tuple_ctx
        .variants_map
        .get(&match_tuple_ctx.current_path)
        .or(match_tuple_ctx.otherwise_variant.as_ref())
        .ok_or_else(|| {
            LoweringFlowError::Failed(ctx.diagnostics.report_by_location(
                match_tuple_ctx.match_location.get(ctx.db),
                MissingMatchArm(format!(
                    "({})",
                    match_tuple_ctx.current_path.variants
                        .iter()
                        .map(|variant| variant.id.name(ctx.db.upcast()))
                        .join(", ")
                )),
            ))
        })?;
    let pattern = &arms[pattern_path.arm_index].patterns[pattern_path.pattern_index];
    let pattern = ctx.function_body.patterns[*pattern].clone();
    let patterns = try_extract_matches!(&pattern, semantic::Pattern::Tuple).ok_or_else(|| {
        LoweringFlowError::Failed(
            ctx.diagnostics.report(pattern.stable_ptr().untyped(), UnsupportedMatchArmNotATuple),
        )
    })?;
    let lowering_inner_pattern_result = patterns
        .field_patterns
        .iter()
        .enumerate()
        .map(|(index, pattern)| {
            let pattern = &ctx.function_body.patterns[*pattern];
            match pattern {
                Pattern::EnumVariant(PatternEnumVariant {
                    inner_pattern: Some(inner_pattern),
                    ..
                }) => {
                    let inner_pattern = ctx.function_body.patterns[*inner_pattern].clone();
                    let pattern_location = ctx.get_location(inner_pattern.stable_ptr().untyped());

                    let variant_expr = LoweredExpr::AtVariable(VarUsage {
                        var_id: match_tuple_ctx.current_var_ids[index],
                        location: pattern_location,
                    });

                    lower_single_pattern(ctx, &mut builder, inner_pattern, variant_expr)
                }
                Pattern::EnumVariant(PatternEnumVariant { inner_pattern: None, .. })
                | Pattern::Otherwise(_) => Ok(()),
                _ => unreachable!(
                    "function `get_variant_to_arm_map` should have reported every other pattern \
                     type"
                ),
            }
        })
        .collect::<LoweringResult<Vec<_>>>()
        .map(|_| ());
    leaves_builders.push(MatchLeafBuilder {
        builder,
        arm_index: pattern_path.arm_index,
        lowerin_result: lowering_inner_pattern_result,
    });
    Ok(())
}

/// Lowers a full decision tree for a match on a tuple expression.
fn lower_full_match_tree(
    ctx: &mut LoweringContext<'_, '_>,
    builder: &mut BlockBuilder,
    arms: &[semantic::MatchArm],
    match_tuple_ctx: &mut LoweringMatchTupleContext,
    extracted_enums_details: &[ExtractedEnumDetails],
    leaves_builders: &mut Vec<MatchLeafBuilder>,
) -> LoweringResult<MatchInfo> {
    let index = match_tuple_ctx.current_path.variants.len();
    let mut arm_var_ids = vec![];
    let block_ids = extracted_enums_details[index]
        .concrete_variants
        .iter()
        .map(|concrete_variant| {
            let mut subscope = create_subscope_with_bound_refs(ctx, builder);
            let block_id = subscope.block_id;
            let var_id = ctx.new_var(VarRequest {
                ty: wrap_in_snapshots(
                    ctx.db.upcast(),
                    concrete_variant.ty,
                    extracted_enums_details[index].n_snapshots + match_tuple_ctx.n_snapshots_outer,
                ),
                location: match_tuple_ctx.match_location,
            });
            arm_var_ids.push(vec![var_id]);

            match_tuple_ctx.current_path.variants.push(concrete_variant.clone());
            match_tuple_ctx.current_var_ids.push(var_id);
            let result = if index + 1 == extracted_enums_details.len() {
                lower_tuple_match_arm(ctx, subscope, arms, match_tuple_ctx, leaves_builders)
            } else {
                lower_full_match_tree(
                    ctx,
                    &mut subscope,
                    arms,
                    match_tuple_ctx,
                    extracted_enums_details,
                    leaves_builders,
                )
                .map(|match_info| {
                    subscope.finalize(ctx, FlatBlockEnd::Match { info: match_info });
                })
            }
            .map(|_| block_id);
            match_tuple_ctx.current_path.variants.pop();
            match_tuple_ctx.current_var_ids.pop();
            result
        })
        .collect::<Vec<_>>()
        .into_iter()
        .collect::<LoweringResult<Vec<_>>>()?;
    let match_info = MatchInfo::Enum(MatchEnumInfo {
        concrete_enum_id: extracted_enums_details[index].concrete_enum_id,
        input: match_tuple_ctx.match_inputs[index],
        arms: zip_eq(
            zip_eq(&extracted_enums_details[index].concrete_variants, block_ids),
            arm_var_ids,
        )
        .map(|((variant_id, block_id), var_ids)| MatchArm {
            arm_selector: MatchArmSelector::VariantId(variant_id.clone()),
            block_id,
            var_ids,
        })
        .collect(),
        location: match_tuple_ctx.match_location,
    });
    Ok(match_info)
}

/// Lowers an expression of type [semantic::ExprMatch] where the matched expression is a tuple of
/// enums.
fn lower_expr_match_tuple(
    ctx: &mut LoweringContext<'_, '_>,
    builder: &mut BlockBuilder,
    expr: LoweredExpr,
    matched_expr: &semantic::Expr,
    n_snapshots_outer: usize,
    types: &[semantic::TypeId],
    arms: &[semantic::MatchArm],
) -> LoweringResult<LoweredExpr> {
    let location = expr.location();
    let match_inputs_exprs = if let LoweredExpr::Tuple { exprs, .. } = expr {
        exprs
    } else {
        let reqs = types
            .iter()
            .map(|ty| VarRequest {
                ty: wrap_in_snapshots(ctx.db.upcast(), *ty, n_snapshots_outer),
                location,
            })
            .collect();
        generators::StructDestructure {
            input: expr.as_var_usage(ctx, builder)?.var_id,
            var_reqs: reqs,
        }
        .add(ctx, &mut builder.statements)
        .into_iter()
        .map(|var_id| {
            LoweredExpr::AtVariable(VarUsage {
                var_id,
                // The variable is used immediately after the destructure, so the usage
                // location is the same as the definition location.
                location: ctx.variables[var_id].location,
            })
        })
        .collect()
    };

    let match_inputs = match_inputs_exprs
        .into_iter()
        .map(|expr| expr.as_var_usage(ctx, builder))
        .collect::<LoweringResult<Vec<_>>>()?;
    let extracted_enums_details = extract_concrete_enum_tuple(ctx, matched_expr, types)?;

    let otherwise_variant = get_underscore_pattern_path(ctx, arms);

    let variants_map = get_variants_to_arm_map_tuple(
        ctx,
        arms.iter().take(
            otherwise_variant
                .as_ref()
                .map(|PatternPath { arm_index, .. }| *arm_index)
                .unwrap_or(arms.len()),
        ),
        extracted_enums_details.as_slice(),
    )?;

    let mut arms_vec = vec![];
    let mut match_tuple_ctx = LoweringMatchTupleContext {
        match_location: location,
        otherwise_variant,
        variants_map,
        match_inputs,
        n_snapshots_outer,
        current_path: MatchingPath::default(),
        current_var_ids: vec![],
    };
    let match_info = lower_full_match_tree(
        ctx,
        builder,
        arms,
        &mut match_tuple_ctx,
        &extracted_enums_details,
        &mut arms_vec,
    )?;
    let empty_match_info = MatchInfo::Enum(MatchEnumInfo {
        concrete_enum_id: extracted_enums_details[0].concrete_enum_id,
        input: match_tuple_ctx.match_inputs[0],
        arms: vec![],
        location,
    });
    let sealed_blocks = group_match_arms(ctx, empty_match_info, location, arms, arms_vec)?;

    builder.merge_and_end_with_match(ctx, match_info, sealed_blocks, location)
}

/// Lowers an expression of type [semantic::ExprMatch].
pub fn lower_expr_match(
    ctx: &mut LoweringContext<'_, '_>,
    expr: &semantic::ExprMatch,
    builder: &mut BlockBuilder,
) -> LoweringResult<LoweredExpr> {
    log::trace!("Lowering a match expression: {:?}", expr.debug(&ctx.expr_formatter));
    let location = ctx.get_location(expr.stable_ptr.untyped());
    let lowered_expr = lower_expr(ctx, builder, expr.matched_expr)?;

    let matched_expr = ctx.function_body.exprs[expr.matched_expr].clone();
    let ty = matched_expr.ty();
    let (n_snapshots, long_type_id) = peel_snapshots(ctx.db.upcast(), ty);

    if let Some(types) = try_extract_matches!(long_type_id, TypeLongId::Tuple) {
        return lower_expr_match_tuple(
            ctx,
            builder,
            lowered_expr,
            &matched_expr,
            n_snapshots,
            &types,
            &expr.arms,
        );
    }

    if ty == ctx.db.core_felt252_ty() {
        let match_input = lowered_expr.as_var_usage(ctx, builder)?;
        return lower_expr_match_felt252(ctx, expr, match_input, builder);
    }
    if let Some(convert_function) =
        corelib::get_convert_to_felt252_libfunc_name_by_type(ctx.db.upcast(), ty)
    {
        let match_input = lowered_expr.as_var_usage(ctx, builder)?;
        let ret_ty = corelib::core_felt252_ty(ctx.db.upcast());
        let call_result = generators::Call {
            function: convert_function.lowered(ctx.db),
            inputs: vec![match_input],
            extra_ret_tys: vec![],
            ret_tys: vec![ret_ty],
            location,
        }
        .add(ctx, &mut builder.statements);

        return lower_expr_match_felt252(
            ctx,
            expr,
            call_result.returns.into_iter().next().unwrap(),
            builder,
        );
    }

    // TODO(spapini): Use diagnostics.
    // TODO(spapini): Handle more than just enums.
    if let LoweredExpr::ExternEnum(extern_enum) = lowered_expr {
        return lower_optimized_extern_match(ctx, builder, extern_enum, &expr.arms);
    }

    let ExtractedEnumDetails { concrete_enum_id, concrete_variants, n_snapshots } =
        extract_concrete_enum(ctx, &matched_expr)?;
    let match_input = lowered_expr.as_var_usage(ctx, builder)?;

    // Merge arm blocks.
    let otherwise_variant = get_underscore_pattern_path(ctx, &expr.arms);
    let variant_map = get_variant_to_arm_map(
        ctx,
        expr.arms.iter().take(
            otherwise_variant
                .as_ref()
                .map(|PatternPath { arm_index, .. }| *arm_index)
                .unwrap_or(expr.arms.len()),
        ),
        concrete_enum_id,
    )?;

    let mut arm_var_ids = vec![];
    let mut block_ids = vec![];
    let varinats_block_builders = concrete_variants
        .iter()
        .map(|concrete_variant| {
            let PatternPath { arm_index, pattern_index } = variant_map
                .get(concrete_variant)
                .or(otherwise_variant.as_ref())
                .ok_or_else(|| {
                    LoweringFlowError::Failed(ctx.diagnostics.report(
                        expr.stable_ptr.untyped(),
                        MissingMatchArm(format!("{}", concrete_variant.id.name(ctx.db.upcast()))),
                    ))
                })?;
            let arm = &expr.arms[*arm_index];

            let mut subscope = create_subscope(ctx, builder);

            let pattern = &ctx.function_body.patterns[arm.patterns[*pattern_index]];
            let block_id = subscope.block_id;
            block_ids.push(block_id);

            let lowering_inner_pattern_result = match pattern {
                Pattern::EnumVariant(PatternEnumVariant {
                    inner_pattern: Some(inner_pattern),
                    ..
                }) => {
                    let inner_pattern = ctx.function_body.patterns[*inner_pattern].clone();
                    let pattern_location = ctx.get_location(inner_pattern.stable_ptr().untyped());

                    let var_id = ctx.new_var(VarRequest {
                        ty: wrap_in_snapshots(ctx.db.upcast(), concrete_variant.ty, n_snapshots),
                        location: pattern_location,
                    });
                    arm_var_ids.push(vec![var_id]);
                    let variant_expr =
                        LoweredExpr::AtVariable(VarUsage { var_id, location: pattern_location });

                    lower_single_pattern(ctx, &mut subscope, inner_pattern, variant_expr)
                }
                Pattern::EnumVariant(PatternEnumVariant { inner_pattern: None, .. })
                | Pattern::Otherwise(_) => {
                    let var_id = ctx.new_var(VarRequest {
                        ty: wrap_in_snapshots(ctx.db.upcast(), concrete_variant.ty, n_snapshots),
                        location: ctx.get_location(pattern.stable_ptr().untyped()),
                    });
                    arm_var_ids.push(vec![var_id]);
                    Ok(())
                }
                _ => unreachable!(
                    "function `get_variant_to_arm_map` should have reported every other pattern \
                     type"
                ),
            };
            Ok(MatchLeafBuilder {
                arm_index: *arm_index,
                lowerin_result: lowering_inner_pattern_result,
                builder: subscope,
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .collect::<LoweringResult<Vec<_>>>()?;

    let empty_match_info = MatchInfo::Enum(MatchEnumInfo {
        concrete_enum_id,
        input: match_input,
        arms: vec![],
        location,
    });

    let sealed_blocks =
        group_match_arms(ctx, empty_match_info, location, &expr.arms, varinats_block_builders)?;

    let match_info = MatchInfo::Enum(MatchEnumInfo {
        concrete_enum_id,
        input: match_input,
        arms: zip_eq(zip_eq(concrete_variants, block_ids), arm_var_ids)
            .map(|((variant_id, block_id), var_ids)| MatchArm {
                arm_selector: MatchArmSelector::VariantId(variant_id),
                block_id,
                var_ids,
            })
            .collect(),
        location,
    });
    builder.merge_and_end_with_match(ctx, match_info, sealed_blocks, location)
}

/// Lowers a match expression on a LoweredExpr::ExternEnum lowered expression.
fn lower_optimized_extern_match(
    ctx: &mut LoweringContext<'_, '_>,
    builder: &mut BlockBuilder,
    extern_enum: LoweredExprExternEnum,
    match_arms: &[semantic::MatchArm],
) -> LoweringResult<LoweredExpr> {
    log::trace!("Started lowering of an optimized extern match.");
    let location = extern_enum.location;
    let concrete_variants = ctx
        .db
        .concrete_enum_variants(extern_enum.concrete_enum_id)
        .map_err(LoweringFlowError::Failed)?;

    // Merge arm blocks.
    let otherwise_variant = get_underscore_pattern_path(ctx, match_arms);

    let variant_map = get_variant_to_arm_map(
        ctx,
        match_arms.iter().take(
            otherwise_variant
                .as_ref()
                .map(|PatternPath { arm_index, .. }| *arm_index)
                .unwrap_or(match_arms.len()),
        ),
        extern_enum.concrete_enum_id,
    )?;
    let mut arm_var_ids = vec![];
    let mut block_ids = vec![];

    let varinats_block_builders = concrete_variants
        .iter()
        .map(|concrete_variant| {
            let mut subscope = create_subscope(ctx, builder);
            let block_id = subscope.block_id;
            block_ids.push(block_id);

            let input_tys =
                match_extern_variant_arm_input_types(ctx, concrete_variant.ty, &extern_enum);
            let mut input_vars = input_tys
                .into_iter()
                .map(|ty| ctx.new_var(VarRequest { ty, location }))
                .collect_vec();
            arm_var_ids.push(input_vars.clone());

            // Bind the arm inputs to implicits and semantic variables.
            match_extern_arm_ref_args_bind(ctx, &mut input_vars, &extern_enum, &mut subscope);

            let variant_expr = extern_facade_expr(ctx, concrete_variant.ty, input_vars, location);

            let PatternPath { arm_index, pattern_index } = variant_map
                .get(concrete_variant)
                .or(otherwise_variant.as_ref())
                .ok_or_else(|| {
                    LoweringFlowError::Failed(ctx.diagnostics.report_by_location(
                        location.get(ctx.db),
                        MissingMatchArm(format!("{}", concrete_variant.id.name(ctx.db.upcast()))),
                    ))
                })?;

            let arm = &match_arms[*arm_index];
            let pattern = &ctx.function_body.patterns[arm.patterns[*pattern_index]];

            let lowering_inner_pattern_result = match pattern {
                Pattern::EnumVariant(PatternEnumVariant {
                    inner_pattern: Some(inner_pattern),
                    ..
                }) => lower_single_pattern(
                    ctx,
                    &mut subscope,
                    ctx.function_body.patterns[*inner_pattern].clone(),
                    variant_expr,
                ),
                Pattern::EnumVariant(PatternEnumVariant { inner_pattern: None, .. })
                | Pattern::Otherwise(_) => Ok(()),
                _ => unreachable!(
                    "function `get_variant_to_arm_map` should have reported every other pattern \
                     type"
                ),
            };
            Ok(MatchLeafBuilder {
                arm_index: *arm_index,
                lowerin_result: lowering_inner_pattern_result,
                builder: subscope,
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .collect::<LoweringResult<Vec<_>>>()?;

    let empty_match_info = MatchInfo::Extern(MatchExternInfo {
        function: extern_enum.function.lowered(ctx.db),
        inputs: vec![],
        arms: vec![],
        location,
    });
    let sealed_blocks =
        group_match_arms(ctx, empty_match_info, location, match_arms, varinats_block_builders)?;
    let match_info = MatchInfo::Extern(MatchExternInfo {
        function: extern_enum.function.lowered(ctx.db),
        inputs: extern_enum.inputs,
        arms: zip_eq(zip_eq(concrete_variants, block_ids), arm_var_ids)
            .map(|((variant_id, block_id), var_ids)| MatchArm {
                arm_selector: MatchArmSelector::VariantId(variant_id),
                block_id,
                var_ids,
            })
            .collect(),
        location,
    });
    builder.merge_and_end_with_match(ctx, match_info, sealed_blocks, location)
}

/// Represents a leaf in match tree, with the arm index it belongs to.
struct MatchLeafBuilder {
    arm_index: usize,
    lowerin_result: LoweringResult<()>,
    builder: BlockBuilder,
}
/// Groups match arms of different variants to their corresponding arms blocks and lowers
/// the arms expression.
fn group_match_arms(
    ctx: &mut LoweringContext<'_, '_>,
    empty_match_info: MatchInfo,
    location: LocationId,
    arms: &[semantic::MatchArm],
    varinats_block_builders: Vec<MatchLeafBuilder>,
) -> LoweringResult<Vec<SealedBlockBuilder>> {
    varinats_block_builders
        .into_iter()
        .sorted_by_key(|MatchLeafBuilder { arm_index, .. }| *arm_index)
        .group_by(|MatchLeafBuilder { arm_index, .. }| *arm_index)
        .into_iter()
        .map(|(arm_index, group)| {
            let arm = &arms[arm_index];
            let mut lowering_inner_pattern_results_and_subscopes = group
                .map(|MatchLeafBuilder { lowerin_result, builder, .. }| (lowerin_result, builder))
                .collect::<Vec<_>>();

            // If the arm has only one pattern, there is no need to create a parent scope.
            if lowering_inner_pattern_results_and_subscopes.len() == 1 {
                let (lowering_inner_pattern_result, subscope) =
                    lowering_inner_pattern_results_and_subscopes.pop().unwrap();

                return match lowering_inner_pattern_result {
                    Ok(_) => {
                        // Lower the arm expression.
                        lower_tail_expr(ctx, subscope, arm.expression)
                    }
                    Err(err) => lowering_flow_error_to_sealed_block(ctx, subscope, err),
                }
                .map_err(LoweringFlowError::Failed);
            }

            // A parent block builder where the the variables of each pattern are introduced.
            // The parent block should have the same semantics and changed_member_paths as any of
            // the child blocks.
            let mut outer_subscope = lowering_inner_pattern_results_and_subscopes[0]
                .1
                .sibling_block_builder(alloc_empty_block(ctx));

            let sealed_blocks: Vec<_> = lowering_inner_pattern_results_and_subscopes
                .into_iter()
                .map(|(lowering_inner_pattern_result, subscope)| {
                    // Use the first pattern for the location of the for variable assignment block.
                    let pattern = &ctx.function_body.patterns[arm.patterns[0]];
                    match lowering_inner_pattern_result {
                        Ok(_) => lowered_expr_to_block_scope_end(
                            ctx,
                            subscope,
                            Ok(LoweredExpr::Tuple {
                                exprs: vec![],
                                location: ctx.get_location(pattern.stable_ptr().untyped()),
                            }),
                        ),
                        Err(err) => lowering_flow_error_to_sealed_block(ctx, subscope, err),
                    }
                    .map_err(LoweringFlowError::Failed)
                })
                .collect::<LoweringResult<Vec<_>>>()?;

            outer_subscope.merge_and_end_with_match(
                ctx,
                empty_match_info.clone(),
                sealed_blocks,
                location,
            )?;
            lower_tail_expr(ctx, outer_subscope, arm.expression).map_err(LoweringFlowError::Failed)
        })
        .collect()
}

/// Lowers the [semantic::MatchArm] of an expression of type [semantic::ExprMatch] where the matched
/// expression is a felt252.
fn lower_expr_felt252_arm(
    ctx: &mut LoweringContext<'_, '_>,
    expr: &semantic::ExprMatch,
    match_input: VarUsage,
    builder: &mut BlockBuilder,
    arm_index: usize,
    pattern_index: usize,
    branches_block_builders: &mut Vec<MatchLeafBuilder>,
) -> LoweringResult<MatchInfo> {
    if pattern_index == expr.arms[arm_index].patterns.len() {
        return lower_expr_felt252_arm(
            ctx,
            expr,
            match_input,
            builder,
            arm_index + 1,
            0,
            branches_block_builders,
        );
    }

    let location = ctx.get_location(expr.stable_ptr.untyped());
    let arm = &expr.arms[arm_index];
    let semantic_db = ctx.db.upcast();

    let main_block = create_subscope_with_bound_refs(ctx, builder);
    let main_block_id = main_block.block_id;

    let mut else_block = create_subscope_with_bound_refs(ctx, builder);
    let block_else_id = else_block.block_id;

    let pattern = &ctx.function_body.patterns[arm.patterns[pattern_index]];
    let semantic::Pattern::Literal(semantic::PatternLiteral { literal, .. }) = pattern else {
        return Err(LoweringFlowError::Failed(
            ctx.diagnostics.report(pattern.stable_ptr().untyped(), UnsupportedMatchArmNotALiteral),
        ));
    };

    let if_input = if literal.value == 0.into() {
        match_input
    } else {
        let ret_ty = corelib::core_felt252_ty(ctx.db.upcast());
        // TODO(TomerStarkware): Use the same type of literal as the input, without the cast to
        // felt252.
        let lowered_arm_val = lower_expr_literal(
            ctx,
            &semantic::ExprLiteral {
                stable_ptr: literal.stable_ptr,
                value: literal.value.clone(),
                ty: ret_ty,
            },
            builder,
        )?
        .as_var_usage(ctx, builder)?;

        let call_result = generators::Call {
            function: corelib::felt252_sub(ctx.db.upcast()).lowered(ctx.db),
            inputs: vec![match_input, lowered_arm_val],
            extra_ret_tys: vec![],
            ret_tys: vec![ret_ty],
            location,
        }
        .add(ctx, &mut builder.statements);
        call_result.returns.into_iter().next().unwrap()
    };

    let non_zero_type =
        corelib::core_nonzero_ty(semantic_db, corelib::core_felt252_ty(semantic_db));
    let else_block_input_var_id = ctx.new_var(VarRequest { ty: non_zero_type, location });

    let match_info = MatchInfo::Extern(MatchExternInfo {
        function: corelib::core_felt252_is_zero(semantic_db).lowered(ctx.db),
        inputs: vec![if_input],
        arms: vec![
            MatchArm {
                arm_selector: MatchArmSelector::VariantId(corelib::jump_nz_zero_variant(
                    semantic_db,
                )),
                block_id: main_block_id,
                var_ids: vec![],
            },
            MatchArm {
                arm_selector: MatchArmSelector::VariantId(corelib::jump_nz_nonzero_variant(
                    semantic_db,
                )),
                block_id: block_else_id,
                var_ids: vec![else_block_input_var_id],
            },
        ],
        location,
    });
    branches_block_builders.push(MatchLeafBuilder {
        arm_index,
        lowerin_result: Ok(()),
        builder: main_block,
    });
    if pattern_index + 1 == expr.arms[arm_index].patterns.len() && arm_index == expr.arms.len() - 2
    {
        branches_block_builders.push(MatchLeafBuilder {
            arm_index: arm_index + 1,
            lowerin_result: Ok(()),
            builder: else_block,
        });
    } else {
        let match_info = lower_expr_felt252_arm(
            ctx,
            expr,
            match_input,
            &mut else_block,
            arm_index,
            pattern_index + 1,
            branches_block_builders,
        )?;

        // we can use finalize here because the else block is an inner block of the match expression
        // and does not have sibling block it goes to.
        else_block.finalize(ctx, FlatBlockEnd::Match { info: match_info });
    }
    Ok(match_info)
}

/// lowers an expression of type [semantic::ExprMatch] where the matched expression is a felt252,
/// using an index enum.
fn lower_expr_match_index_enum(
    ctx: &mut LoweringContext<'_, '_>,
    expr: &semantic::ExprMatch,
    match_input: VarUsage,
    builder: &BlockBuilder,
    literals_to_arm_map: &UnorderedHashMap<usize, usize>,
    branches_block_builders: &mut Vec<MatchLeafBuilder>,
) -> LoweringResult<MatchInfo> {
    let location = ctx.get_location(expr.stable_ptr.untyped());
    let semantic_db = ctx.db.upcast();
    let unit_type = unit_ty(semantic_db);
    let mut arm_var_ids = vec![];
    let mut block_ids = vec![];

    for index in 0..literals_to_arm_map.len() {
        let subscope = create_subscope_with_bound_refs(ctx, builder);
        let block_id = subscope.block_id;
        block_ids.push(block_id);

        let arm_index = literals_to_arm_map[&index];

        let var_id = ctx.new_var(VarRequest { ty: unit_type, location });
        arm_var_ids.push(vec![var_id]);

        // Lower the arm expression.
        branches_block_builders.push(MatchLeafBuilder {
            arm_index,
            lowerin_result: Ok(()),
            builder: subscope,
        });
    }

    let arms = zip_eq(block_ids, arm_var_ids)
        .enumerate()
        .map(|(value, (block_id, var_ids))| MatchArm {
            arm_selector: MatchArmSelector::Value(ValueSelectorArm { value }),
            block_id,
            var_ids,
        })
        .collect();
    let match_info = MatchInfo::Value(MatchEnumValue {
        num_of_arms: literals_to_arm_map.len(),
        arms,
        input: match_input,
        location,
    });
    Ok(match_info)
}

/// Lowers an expression of type [semantic::ExprMatch] where the matched expression is a felt252.
/// using an index enum to create a jump table.
fn lower_expr_match_felt252(
    ctx: &mut LoweringContext<'_, '_>,
    expr: &semantic::ExprMatch,
    match_input: VarUsage,
    builder: &mut BlockBuilder,
) -> LoweringResult<LoweredExpr> {
    log::trace!("Lowering a match-felt252 expression.");
    if expr.arms.is_empty() {
        return Err(LoweringFlowError::Failed(
            ctx.diagnostics.report(expr.stable_ptr.untyped(), NonExhaustiveMatchFelt252),
        ));
    }
    let mut max = 0;
    let mut literals_to_arm_map = UnorderedHashMap::default();
    let mut otherwise_exist = false;
    for (arm_index, arm) in expr.arms.iter().enumerate() {
        for pattern in arm.patterns.iter() {
            let pattern = &ctx.function_body.patterns[*pattern];
            if otherwise_exist {
                return Err(LoweringFlowError::Failed(
                    ctx.diagnostics.report(pattern.stable_ptr().untyped(), UnreachableMatchArm),
                ));
            }
            match pattern {
                semantic::Pattern::Literal(semantic::PatternLiteral { literal, .. }) => {
                    let Some(literal) = literal.value.to_usize() else {
                        return Err(LoweringFlowError::Failed(
                            ctx.diagnostics.report(
                                expr.stable_ptr.untyped(),
                                UnsupportedMatchArmNonSequential,
                            ),
                        ));
                    };
                    if otherwise_exist || literals_to_arm_map.insert(literal, arm_index).is_some() {
                        return Err(LoweringFlowError::Failed(
                            ctx.diagnostics
                                .report(pattern.stable_ptr().untyped(), UnreachableMatchArm),
                        ));
                    }
                    if literal > max {
                        max = literal;
                    }
                }
                semantic::Pattern::Otherwise(_) => otherwise_exist = true,
                _ => {
                    return Err(LoweringFlowError::Failed(
                        ctx.diagnostics
                            .report(pattern.stable_ptr().untyped(), UnsupportedMatchArmNotALiteral),
                    ));
                }
            }
        }
    }

    if !otherwise_exist {
        return Err(LoweringFlowError::Failed(
            ctx.diagnostics.report(expr.stable_ptr.untyped(), NonExhaustiveMatchFelt252),
        ));
    }
    if max + 1 != literals_to_arm_map.len() {
        return Err(LoweringFlowError::Failed(
            ctx.diagnostics.report(expr.stable_ptr.untyped(), UnsupportedMatchArmNonSequential),
        ));
    };
    let location = ctx.get_location(expr.stable_ptr.untyped());

    let mut arms_vec = vec![];

    let empty_match_info = MatchInfo::Extern(MatchExternInfo {
        function: corelib::core_felt252_is_zero(ctx.db.upcast()).lowered(ctx.db),
        inputs: vec![match_input],
        arms: vec![],
        location,
    });

    if max <= numeric_match_optimization_threshold(ctx) {
        let match_info =
            lower_expr_felt252_arm(ctx, expr, match_input, builder, 0, 0, &mut arms_vec)?;

        let sealed_blocks =
            group_match_arms(ctx, empty_match_info, location, &expr.arms, arms_vec)?;

        return builder.merge_and_end_with_match(ctx, match_info, sealed_blocks, location);
    }
    let semantic_db = ctx.db.upcast();

    let felt252_ty = core_felt252_ty(semantic_db);
    let bounded_int_ty = corelib::bounded_int_ty(semantic_db, 0.into(), max.into());

    let function_id =
        corelib::core_downcast(semantic_db, felt252_ty, bounded_int_ty).lowered(ctx.db);

    let in_range_block_input_var_id = ctx.new_var(VarRequest { ty: bounded_int_ty, location });

    let in_range_block = create_subscope_with_bound_refs(ctx, builder);
    let in_range_block_id = in_range_block.block_id;
    let inner_match_info = lower_expr_match_index_enum(
        ctx,
        expr,
        VarUsage { var_id: in_range_block_input_var_id, location: match_input.location },
        &in_range_block,
        &literals_to_arm_map,
        &mut arms_vec,
    )?;
    in_range_block.finalize(ctx, FlatBlockEnd::Match { info: inner_match_info });

    let otherwise_block = create_subscope_with_bound_refs(ctx, builder);
    let otherwise_block_id = otherwise_block.block_id;

    arms_vec.push(MatchLeafBuilder {
        arm_index: expr.arms.len() - 1,
        lowerin_result: Ok(()),
        builder: otherwise_block,
    });

    let match_info = MatchInfo::Extern(MatchExternInfo {
        function: function_id,
        inputs: vec![match_input],
        arms: vec![
            MatchArm {
                arm_selector: MatchArmSelector::VariantId(corelib::option_some_variant(
                    semantic_db,
                    GenericArgumentId::Type(bounded_int_ty),
                )),
                block_id: in_range_block_id,
                var_ids: vec![in_range_block_input_var_id],
            },
            MatchArm {
                arm_selector: MatchArmSelector::VariantId(corelib::option_none_variant(
                    semantic_db,
                    GenericArgumentId::Type(bounded_int_ty),
                )),
                block_id: otherwise_block_id,
                var_ids: vec![],
            },
        ],
        location,
    });
    let sealed_blocks = group_match_arms(ctx, empty_match_info, location, &expr.arms, arms_vec)?;
    builder.merge_and_end_with_match(ctx, match_info, sealed_blocks, location)
}

/// Returns the threshold for the number of arms for optimising numeric match expressions, by using
/// a jump table instead of an if-else construct.
fn numeric_match_optimization_threshold(ctx: &mut LoweringContext<'_, '_>) -> usize {
    // Use [usize::max] as the default value, so that the optimization is not used by default.
    // TODO(TomerStarkware): Set the default to be optimal on `sierra-minor-update` branch.
    ctx.db
        .get_flag(FlagId::new(ctx.db.upcast(), "numeric_match_optimization_min_arms_threshold"))
        .map(|flag| match *flag {
            Flag::NumericMatchOptimizationMinArmsThreshold(threshold) => threshold,
            _ => panic!("Wrong type flag `{flag:?}`."),
        })
        .unwrap_or(usize::MAX)
}
