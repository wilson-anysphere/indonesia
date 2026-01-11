use std::time::Duration;

use nova_dap::stream_debug::run_stream_debug;
use nova_jdwp::{
    JdwpValue, MockJdwpClient, MockObject, ObjectKindPreview, ObjectPreview, ObjectRef,
};
use nova_stream_debug::StreamDebugConfig;

#[test]
fn dap_stream_debug_runs_with_mock_jdwp() {
    let expression = "list.stream().map(x -> x).count()";

    let mut jdwp = MockJdwpClient::new();
    jdwp.set_evaluation(
        1,
        "list.stream().limit(2).collect(java.util.stream.Collectors.toList())",
        Ok(JdwpValue::Object(ObjectRef {
            id: 10,
            runtime_type: "java.util.ArrayList".to_string(),
        })),
    );
    jdwp.insert_object(
        10,
        MockObject {
            preview: ObjectPreview {
                runtime_type: "java.util.ArrayList".to_string(),
                kind: ObjectKindPreview::List {
                    size: 2,
                    sample: vec![JdwpValue::Int(1), JdwpValue::Int(2)],
                },
            },
            children: Vec::new(),
        },
    );

    jdwp.set_evaluation(
        1,
        "list.stream().map(x -> x).limit(2).collect(java.util.stream.Collectors.toList())",
        Ok(JdwpValue::Object(ObjectRef {
            id: 11,
            runtime_type: "java.util.ArrayList".to_string(),
        })),
    );
    jdwp.insert_object(
        11,
        MockObject {
            preview: ObjectPreview {
                runtime_type: "java.util.ArrayList".to_string(),
                kind: ObjectKindPreview::List {
                    size: 2,
                    sample: vec![JdwpValue::Int(1), JdwpValue::Int(2)],
                },
            },
            children: Vec::new(),
        },
    );

    jdwp.set_evaluation(
        1,
        "list.stream().map(x -> x).limit(2).count()",
        Ok(JdwpValue::Long(2)),
    );

    let cfg = StreamDebugConfig {
        max_sample_size: 2,
        max_total_time: Duration::from_secs(1),
        allow_side_effects: false,
        allow_terminal_ops: true,
    };

    let body = run_stream_debug(&mut jdwp, 1, expression, cfg).unwrap();
    assert_eq!(body.runtime.steps.len(), 1);
    assert_eq!(body.runtime.steps[0].operation, "map");
    assert_eq!(
        body.runtime.terminal.as_ref().unwrap().value.as_deref(),
        Some("2")
    );
}
