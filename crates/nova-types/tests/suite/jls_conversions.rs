use nova_types::{
    assignment_conversion, assignment_conversion_with_const, binary_numeric_promotion,
    cast_conversion, conversion_cost, method_invocation_conversion, unary_numeric_promotion,
    ConstValue, ConversionCost, ConversionStep, PrimitiveType, Type, TypeEnv, TypeStore,
    TypeWarning, UncheckedReason,
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
fn assignment_allows_constant_narrowing() {
    let env = TypeStore::with_minimal_jdk();
    let int_ty = Type::Primitive(PrimitiveType::Int);
    let byte_ty = Type::Primitive(PrimitiveType::Byte);

    assert!(assignment_conversion(&env, &int_ty, &byte_ty).is_none());

    let conv = assignment_conversion_with_const(&env, &int_ty, &byte_ty, Some(ConstValue::Int(1)))
        .unwrap();
    assert_eq!(conv.steps, vec![ConversionStep::NarrowingPrimitive]);

    assert!(
        assignment_conversion_with_const(&env, &int_ty, &byte_ty, Some(ConstValue::Int(128)),)
            .is_none()
    );
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

#[test]
fn parameterized_casts_are_unchecked() {
    let env = TypeStore::with_minimal_jdk();
    let list_id = env.class_id("java.util.List").unwrap();

    let list_string = Type::class(list_id, vec![Type::class(env.well_known().string, vec![])]);
    let list_integer = Type::class(list_id, vec![Type::class(env.well_known().integer, vec![])]);
    let raw_list = Type::class(list_id, vec![]);

    let conv = cast_conversion(&env, &list_string, &list_integer).unwrap();
    assert!(conv
        .warnings
        .contains(&TypeWarning::Unchecked(UncheckedReason::UncheckedCast)));

    let conv_raw = cast_conversion(&env, &raw_list, &list_string).unwrap();
    assert!(conv_raw
        .warnings
        .contains(&TypeWarning::Unchecked(UncheckedReason::RawConversion)));
}

#[test]
fn intersection_casts_preserve_component_warnings() {
    let env = TypeStore::with_minimal_jdk();
    let list_id = env.class_id("java.util.List").unwrap();

    let list_string = Type::class(list_id, vec![Type::class(env.well_known().string, vec![])]);
    let list_int = Type::class(list_id, vec![Type::class(env.well_known().integer, vec![])]);

    let serializable = env.well_known().serializable;
    let target = Type::Intersection(vec![list_int.clone(), Type::class(serializable, vec![])]);

    let conv = cast_conversion(&env, &list_string, &target).unwrap();
    assert!(conv
        .warnings
        .contains(&TypeWarning::Unchecked(UncheckedReason::UncheckedCast)));
}

#[test]
fn conversion_cost_ordering() {
    let env = TypeStore::with_minimal_jdk();

    let int_ty = Type::Primitive(PrimitiveType::Int);
    let long_ty = Type::Primitive(PrimitiveType::Long);
    let integer_ty = Type::class(env.well_known().integer, vec![]);
    let list_id = env.class_id("java.util.List").unwrap();
    let list_string = Type::class(list_id, vec![Type::class(env.well_known().string, vec![])]);
    let raw_list = Type::class(list_id, vec![]);

    let identity = method_invocation_conversion(&env, &int_ty, &int_ty).unwrap();
    let widening = method_invocation_conversion(&env, &int_ty, &long_ty).unwrap();
    let boxing = method_invocation_conversion(&env, &int_ty, &integer_ty).unwrap();
    let unchecked = assignment_conversion(&env, &list_string, &raw_list).unwrap();
    let narrowing = cast_conversion(&env, &long_ty, &int_ty).unwrap();

    assert!(conversion_cost(&identity) < conversion_cost(&widening));
    assert!(conversion_cost(&widening) < conversion_cost(&boxing));
    assert!(conversion_cost(&boxing) < conversion_cost(&unchecked));
    assert!(conversion_cost(&unchecked) < conversion_cost(&narrowing));

    // Sanity: make sure we hit the intended buckets.
    assert_eq!(conversion_cost(&identity), ConversionCost::Identity);
    assert_eq!(conversion_cost(&unchecked), ConversionCost::Unchecked);
    assert_eq!(conversion_cost(&narrowing), ConversionCost::Narrowing);
}
