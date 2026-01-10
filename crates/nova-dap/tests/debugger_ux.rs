use nova_dap::{DebugSession, ObjectHandle, PINNED_SCOPE_REF};
use nova_jdwp::{
    JdwpValue, MockJdwpClient, MockObject, ObjectKindPreview, ObjectPreview, ObjectRef, StopReason,
    StoppedEvent,
};

fn string_object(id: u64, value: &str) -> (u64, MockObject) {
    (
        id,
        MockObject {
            preview: ObjectPreview {
                runtime_type: "java.lang.String".to_string(),
                kind: ObjectKindPreview::String {
                    value: value.to_string(),
                },
            },
            children: Vec::new(),
        },
    )
}

#[test]
fn formatting_is_stable_for_maps_and_sets() {
    let mut jdwp_a = MockJdwpClient::new();
    let (key_a_id, key_a) = string_object(10, "a");
    let (key_b_id, key_b) = string_object(11, "b");
    jdwp_a.insert_object(key_a_id, key_a);
    jdwp_a.insert_object(key_b_id, key_b);

    jdwp_a.insert_object(
        1,
        MockObject {
            preview: ObjectPreview {
                runtime_type: "java.util.HashMap".to_string(),
                kind: ObjectKindPreview::Map {
                    size: 2,
                    // Deliberately reversed to ensure deterministic sorting.
                    sample: vec![
                        (
                            JdwpValue::Object(ObjectRef {
                                id: key_b_id,
                                runtime_type: "java.lang.String".to_string(),
                            }),
                            JdwpValue::Int(1),
                        ),
                        (
                            JdwpValue::Object(ObjectRef {
                                id: key_a_id,
                                runtime_type: "java.lang.String".to_string(),
                            }),
                            JdwpValue::Int(2),
                        ),
                    ],
                },
            },
            children: Vec::new(),
        },
    );

    jdwp_a.insert_object(
        2,
        MockObject {
            preview: ObjectPreview {
                runtime_type: "java.util.HashSet".to_string(),
                kind: ObjectKindPreview::Set {
                    size: 2,
                    sample: vec![
                        JdwpValue::Object(ObjectRef {
                            id: key_b_id,
                            runtime_type: "java.lang.String".to_string(),
                        }),
                        JdwpValue::Object(ObjectRef {
                            id: key_a_id,
                            runtime_type: "java.lang.String".to_string(),
                        }),
                    ],
                },
            },
            children: Vec::new(),
        },
    );

    jdwp_a.set_evaluation(
        1,
        "m",
        Ok(JdwpValue::Object(ObjectRef {
            id: 1,
            runtime_type: "java.util.HashMap".to_string(),
        })),
    );
    jdwp_a.set_evaluation(
        1,
        "s",
        Ok(JdwpValue::Object(ObjectRef {
            id: 2,
            runtime_type: "java.util.HashSet".to_string(),
        })),
    );

    let mut session_a = DebugSession::new(jdwp_a);
    let map_a = session_a.evaluate(1, "m").unwrap();
    let set_a = session_a.evaluate(1, "s").unwrap();

    // Now run the same values with opposite sample ordering and ensure the
    // formatted output (including stable handles) matches.
    let mut jdwp_b = MockJdwpClient::new();
    let (key_a_id, key_a) = string_object(10, "a");
    let (key_b_id, key_b) = string_object(11, "b");
    jdwp_b.insert_object(key_a_id, key_a);
    jdwp_b.insert_object(key_b_id, key_b);

    jdwp_b.insert_object(
        1,
        MockObject {
            preview: ObjectPreview {
                runtime_type: "java.util.HashMap".to_string(),
                kind: ObjectKindPreview::Map {
                    size: 2,
                    sample: vec![
                        (
                            JdwpValue::Object(ObjectRef {
                                id: key_a_id,
                                runtime_type: "java.lang.String".to_string(),
                            }),
                            JdwpValue::Int(2),
                        ),
                        (
                            JdwpValue::Object(ObjectRef {
                                id: key_b_id,
                                runtime_type: "java.lang.String".to_string(),
                            }),
                            JdwpValue::Int(1),
                        ),
                    ],
                },
            },
            children: Vec::new(),
        },
    );

    jdwp_b.insert_object(
        2,
        MockObject {
            preview: ObjectPreview {
                runtime_type: "java.util.HashSet".to_string(),
                kind: ObjectKindPreview::Set {
                    size: 2,
                    sample: vec![
                        JdwpValue::Object(ObjectRef {
                            id: key_a_id,
                            runtime_type: "java.lang.String".to_string(),
                        }),
                        JdwpValue::Object(ObjectRef {
                            id: key_b_id,
                            runtime_type: "java.lang.String".to_string(),
                        }),
                    ],
                },
            },
            children: Vec::new(),
        },
    );

    jdwp_b.set_evaluation(
        1,
        "m",
        Ok(JdwpValue::Object(ObjectRef {
            id: 1,
            runtime_type: "java.util.HashMap".to_string(),
        })),
    );
    jdwp_b.set_evaluation(
        1,
        "s",
        Ok(JdwpValue::Object(ObjectRef {
            id: 2,
            runtime_type: "java.util.HashSet".to_string(),
        })),
    );

    let mut session_b = DebugSession::new(jdwp_b);
    let map_b = session_b.evaluate(1, "m").unwrap();
    let set_b = session_b.evaluate(1, "s").unwrap();

    assert_eq!(map_a.result, map_b.result);
    assert_eq!(set_a.result, set_b.result);
}

#[test]
fn object_pinning_keeps_handles_inspectable_across_steps_and_handles_gc() {
    let mut jdwp = MockJdwpClient::new();

    jdwp.insert_object(
        100,
        MockObject {
            preview: ObjectPreview {
                runtime_type: "com.example.Foo".to_string(),
                kind: ObjectKindPreview::Plain,
            },
            children: vec![nova_jdwp::JdwpVariable {
                name: "x".to_string(),
                value: JdwpValue::Int(1),
                static_type: Some("int".to_string()),
                evaluate_name: Some("foo.x".to_string()),
            }],
        },
    );

    jdwp.set_evaluation(
        1,
        "foo",
        Ok(JdwpValue::Object(ObjectRef {
            id: 100,
            runtime_type: "com.example.Foo".to_string(),
        })),
    );

    let mut session = DebugSession::new(jdwp);
    let evaluated = session.evaluate(1, "foo").unwrap();

    let handle = ObjectHandle::from_variables_reference(evaluated.variables_reference).unwrap();
    session.pin_object(handle).unwrap();

    assert!(session.jdwp_mut().is_collection_disabled(100));
    assert_eq!(session.jdwp_mut().disable_collection_calls, vec![100]);

    let pinned = session.variables(PINNED_SCOPE_REF).unwrap();
    assert_eq!(pinned.len(), 1);
    assert_eq!(pinned[0].name, handle.to_string());
    assert!(pinned[0]
        .presentation_hint
        .as_ref()
        .unwrap()
        .attributes
        .as_ref()
        .unwrap()
        .contains(&"pinned".to_string()));

    let fields = session.variables(handle.as_variables_reference()).unwrap();
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].name, "x");
    assert_eq!(fields[0].value, "1");
    assert_eq!(fields[0].type_.as_deref(), Some("int"));
    assert_eq!(fields[0].evaluate_name.as_deref(), Some("foo.x"));

    // Simulate the JVM collecting the object and ensure we don't crash or leak
    // the pinned handle.
    session.jdwp_mut().collect_object(100);

    let fields_after_gc = session.variables(handle.as_variables_reference()).unwrap();
    assert_eq!(fields_after_gc.len(), 1);
    assert_eq!(fields_after_gc[0].name, "<collected>");

    session.unpin_object(handle).unwrap();
}

#[test]
fn step_emits_output_events_for_return_values_and_expression_values() {
    let mut jdwp = MockJdwpClient::new();
    let (str_id, str_obj) = string_object(200, "hello");
    jdwp.insert_object(str_id, str_obj);

    jdwp.push_step(Ok(StoppedEvent {
        thread_id: 1,
        reason: StopReason::Step,
        return_value: Some(JdwpValue::Int(42)),
        expression_value: Some(JdwpValue::Object(ObjectRef {
            id: str_id,
            runtime_type: "java.lang.String".to_string(),
        })),
    }));

    let mut session = DebugSession::new(jdwp);
    let output = session.step_out(1).unwrap();

    assert_eq!(output.output.len(), 2);
    assert!(output.output[0].output.contains("Return value: 42"));
    assert!(output.output[1].output.contains("Expression value: \"hello\""));
}

