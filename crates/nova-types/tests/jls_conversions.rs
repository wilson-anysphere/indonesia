use nova_types::{
    assignment_conversion, binary_numeric_promotion, cast_conversion, method_invocation_conversion,
    unary_numeric_promotion, ConversionStep, PrimitiveType, Type, TypeStore, TypeWarning,
    TypeEnv, UncheckedReason,
};

use pretty_assertions::assert_eq;

#[test]
fn numeric_promotions() {
    assert_eq!(
        unary_numeric_promotion(PrimitiveType::Byte),
        Some(PrimitiveType::Int)
    );
    assert_eq!(
        unary_numeric_promotion(PrimitiveType::Double),
        Some(PrimitiveType::Double)
    );
    assert_eq!(unary_numeric_promotion(PrimitiveType::Boolean), None);

    assert_eq!(
        binary_numeric_promotion(PrimitiveType::Int, PrimitiveType::Double),
        Some(PrimitiveType::Double)
    );
    assert_eq!(
        binary_numeric_promotion(PrimitiveType::Short, PrimitiveType::Long),
        Some(PrimitiveType::Long)
    );
}

#[test]
fn boxing_and_widening_reference() {
    let env = TypeStore::with_minimal_jdk();

    let int_ty = Type::Primitive(PrimitiveType::Int);
    let integer_ty = Type::class(env.well_known().integer, vec![]);
    let object_ty = Type::class(env.well_known().object, vec![]);

    let c1 = method_invocation_conversion(&env, &int_ty, &integer_ty).unwrap();
    assert_eq!(c1.steps, vec![ConversionStep::Boxing]);

    let c2 = method_invocation_conversion(&env, &int_ty, &object_ty).unwrap();
    assert_eq!(
        c2.steps,
        vec![ConversionStep::Boxing, ConversionStep::WideningReference]
    );
}

#[test]
fn widening_then_boxing_to_different_wrapper() {
    let env = TypeStore::with_minimal_jdk();

    let int_ty = Type::Primitive(PrimitiveType::Int);
    let long_wrapper = Type::class(env.class_id("java.lang.Long").unwrap(), vec![]);

    let conv = method_invocation_conversion(&env, &int_ty, &long_wrapper).unwrap();
    assert_eq!(
        conv.steps,
        vec![ConversionStep::WideningPrimitive, ConversionStep::Boxing]
    );
}

#[test]
fn unboxing_and_widening_primitive() {
    let env = TypeStore::with_minimal_jdk();

    let integer_ty = Type::class(env.well_known().integer, vec![]);
    let long_ty = Type::Primitive(PrimitiveType::Long);

    let conv = method_invocation_conversion(&env, &integer_ty, &long_ty).unwrap();
    assert_eq!(
        conv.steps,
        vec![ConversionStep::Unboxing, ConversionStep::WideningPrimitive]
    );
}

#[test]
fn raw_type_conversions_produce_unchecked_warning() {
    let env = TypeStore::with_minimal_jdk();
    let list_id = env.class_id("java.util.List").unwrap();

    let list_string = Type::class(list_id, vec![Type::class(env.well_known().string, vec![])]);
    let raw_list = Type::class(list_id, vec![]);

    let conv = assignment_conversion(&env, &raw_list, &list_string).unwrap();
    assert!(conv
        .warnings
        .contains(&TypeWarning::Unchecked(UncheckedReason::RawConversion)));

    let conv2 = assignment_conversion(&env, &list_string, &raw_list).unwrap();
    assert!(conv2
        .warnings
        .contains(&TypeWarning::Unchecked(UncheckedReason::RawConversion)));
}

#[test]
fn cast_allows_numeric_narrowing() {
    let env = TypeStore::with_minimal_jdk();

    let int_ty = Type::Primitive(PrimitiveType::Int);
    let long_ty = Type::Primitive(PrimitiveType::Long);
    let conv = cast_conversion(&env, &long_ty, &int_ty).unwrap();
    assert_eq!(conv.steps, vec![ConversionStep::NarrowingPrimitive]);

    // Boxing is allowed for casts too.
    let obj_ty = Type::class(env.well_known().object, vec![]);
    let conv = cast_conversion(&env, &int_ty, &obj_ty).unwrap();
    assert!(conv.steps.contains(&ConversionStep::Boxing));
}
