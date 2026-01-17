use serde_json::{Map, Value};

pub(crate) fn ensure_object_mut(value: &mut Value) -> Option<&mut Map<String, Value>> {
    if !value.is_object() {
        *value = Value::Object(Map::new());
    }
    value.as_object_mut()
}

pub(crate) fn ensure_object_field_mut<'a>(
    obj: &'a mut Map<String, Value>,
    key: &str,
) -> Option<&'a mut Map<String, Value>> {
    let entry = obj
        .entry(key.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !entry.is_object() {
        *entry = Value::Object(Map::new());
    }
    entry.as_object_mut()
}
