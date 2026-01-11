use std::collections::HashMap;

use nova_dap::jdwp::wire::inspect;
use nova_dap::jdwp::wire::{mock::MockJdwpServer, JdwpClient, JdwpValue};
use nova_dap::wire_format::{ObjectChildrenKind, ValueFormatter};

fn alloc_registry() -> impl FnMut(u64, ObjectChildrenKind) -> i64 {
    let mut next = 1i64;
    let mut map: HashMap<(u64, ObjectChildrenKind), i64> = HashMap::new();
    move |object_id, kind| {
        *map.entry((object_id, kind)).or_insert_with(|| {
            let v = next;
            next += 1;
            v
        })
    }
}

#[tokio::test]
async fn wire_map_and_set_previews_are_deterministic_even_if_internal_order_changes() {
    let server = MockJdwpServer::spawn().await.unwrap();
    let client = JdwpClient::connect(server.addr()).await.unwrap();
    let formatter = ValueFormatter::default();

    let map_value = JdwpValue::Object {
        tag: b'L',
        id: server.sample_hashmap_id(),
    };
    let set_value = JdwpValue::Object {
        tag: b'L',
        id: server.sample_hashset_id(),
    };

    let mut reg1 = alloc_registry();
    let map1 = formatter
        .format_value(&client, &mut reg1, &map_value, None)
        .await
        .unwrap()
        .value;
    let mut reg2 = alloc_registry();
    let map2 = formatter
        .format_value(&client, &mut reg2, &map_value, None)
        .await
        .unwrap()
        .value;
    assert_eq!(map1, map2);

    let a_pos = map1.find("\"a\"").unwrap();
    let b_pos = map1.find("\"b\"").unwrap();
    assert!(
        a_pos < b_pos,
        "expected map sample to be sorted by key: {map1}"
    );

    let mut reg3 = alloc_registry();
    let set1 = formatter
        .format_value(&client, &mut reg3, &set_value, None)
        .await
        .unwrap()
        .value;
    let mut reg4 = alloc_registry();
    let set2 = formatter
        .format_value(&client, &mut reg4, &set_value, None)
        .await
        .unwrap()
        .value;
    assert_eq!(set1, set2);

    let a_pos = set1.find("\"a\"").unwrap();
    let b_pos = set1.find("\"b\"").unwrap();
    assert!(a_pos < b_pos, "expected set sample to be sorted: {set1}");
}

#[tokio::test]
async fn wire_strings_are_escaped_and_truncated() {
    let server = MockJdwpServer::spawn().await.unwrap();
    let client = JdwpClient::connect(server.addr()).await.unwrap();
    let formatter = ValueFormatter::default();

    let value = JdwpValue::Object {
        tag: b's',
        id: server.sample_string_id(),
    };

    let mut reg = alloc_registry();
    let formatted = formatter
        .format_value(&client, &mut reg, &value, None)
        .await
        .unwrap();

    assert!(formatted.value.starts_with('"'));
    assert!(
        formatted.value.contains("\\n"),
        "expected newline escape: {}",
        formatted.value
    );
    assert!(
        formatted.value.contains("\\\""),
        "expected quote escape: {}",
        formatted.value
    );
    assert!(
        formatted.value.contains("\\\\"),
        "expected backslash escape: {}",
        formatted.value
    );
    assert!(
        formatted.value.contains('…'),
        "expected truncation ellipsis: {}",
        formatted.value
    );
}

#[tokio::test]
async fn wire_arrays_show_length_and_sample() {
    let server = MockJdwpServer::spawn().await.unwrap();
    let client = JdwpClient::connect(server.addr()).await.unwrap();
    let formatter = ValueFormatter::default();

    let value = JdwpValue::Object {
        tag: b'[',
        id: server.sample_int_array_id(),
    };

    let mut reg = alloc_registry();
    let formatted = formatter
        .format_value(&client, &mut reg, &value, None)
        .await
        .unwrap();

    assert!(
        formatted.value.contains("int[5]"),
        "expected array length: {}",
        formatted.value
    );
    assert!(
        formatted.value.contains("10, 20, 30"),
        "expected array sample: {}",
        formatted.value
    );
}

#[tokio::test]
async fn wire_hashmap_children_are_sorted_by_key() {
    let server = MockJdwpServer::spawn().await.unwrap();
    let client = JdwpClient::connect(server.addr()).await.unwrap();

    let children1 = inspect::object_children(&client, server.sample_hashmap_id())
        .await
        .unwrap();
    let children2 = inspect::object_children(&client, server.sample_hashmap_id())
        .await
        .unwrap();

    let names1: Vec<_> = children1.iter().map(|(name, _, _)| name.as_str()).collect();
    let names2: Vec<_> = children2.iter().map(|(name, _, _)| name.as_str()).collect();
    assert_eq!(names1, names2);

    let a_pos = names1.iter().position(|n| *n == "\"a\"").unwrap();
    let b_pos = names1.iter().position(|n| *n == "\"b\"").unwrap();
    assert!(a_pos < b_pos, "expected keys sorted: {names1:?}");
}
