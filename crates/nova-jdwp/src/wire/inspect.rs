use std::collections::HashMap;

use super::{
    types::{
        FieldInfo, FieldInfoWithGeneric, JdwpValue, MethodInfo, MethodInfoWithGeneric, ObjectId,
        ReferenceTypeId, Result,
    },
    JdwpClient,
};

const ARRAY_PREVIEW_SAMPLE: usize = 3;
const ARRAY_CHILD_SAMPLE: usize = 25;
const HASHMAP_SCAN_LIMIT: usize = 64;
const HASHMAP_CHAIN_LIMIT: usize = 16;

/// JDWP `Error.INVALID_OBJECT` (the object has already been garbage collected).
pub const ERROR_INVALID_OBJECT: u16 = 20;

/// Per-connection cache for type metadata needed by the inspection layer.
///
/// This is intentionally lightweight (no eviction); the number of distinct
/// reference types encountered during a debug session is usually small.
#[derive(Debug, Default)]
pub(crate) struct InspectCache {
    pub(crate) signatures: HashMap<ReferenceTypeId, String>,
    #[allow(dead_code)]
    pub(crate) signatures_with_generic: HashMap<ReferenceTypeId, (String, Option<String>)>,
    pub(crate) fields: HashMap<ReferenceTypeId, Vec<FieldInfo>>,
    #[allow(dead_code)]
    pub(crate) fields_with_generic: HashMap<ReferenceTypeId, Vec<FieldInfoWithGeneric>>,
    #[allow(dead_code)]
    pub(crate) methods: HashMap<ReferenceTypeId, Vec<MethodInfo>>,
    #[allow(dead_code)]
    pub(crate) methods_with_generic: HashMap<ReferenceTypeId, Vec<MethodInfoWithGeneric>>,
    pub(crate) superclasses: HashMap<ReferenceTypeId, ReferenceTypeId>,
    #[allow(dead_code)]
    pub(crate) interfaces: HashMap<ReferenceTypeId, Vec<ReferenceTypeId>>,
    pub(crate) all_instance_fields: HashMap<ReferenceTypeId, Vec<FieldInfo>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ObjectPreview {
    pub runtime_type: String,
    pub kind: ObjectKindPreview,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ObjectKindPreview {
    Plain,
    String {
        value: String,
    },
    PrimitiveWrapper {
        value: Box<JdwpValue>,
    },
    Array {
        element_type: String,
        length: usize,
        sample: Vec<JdwpValue>,
    },
    List {
        size: usize,
        sample: Vec<JdwpValue>,
    },
    Set {
        size: usize,
        sample: Vec<JdwpValue>,
    },
    Map {
        size: usize,
        sample: Vec<(JdwpValue, JdwpValue)>,
    },
    Optional {
        value: Option<Box<JdwpValue>>,
    },
    Stream {
        size: Option<usize>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct InspectVariable {
    pub name: String,
    pub value: JdwpValue,
    pub static_type: Option<String>,
}

/// High-level helpers for object inspection.
///
/// This is a companion to the low-level wire client (`JdwpClient`) that
/// implements the object preview and expansion heuristics used by Nova's DAP UI.
pub struct Inspector {
    jdwp: JdwpClient,
}

impl Inspector {
    pub fn new(jdwp: JdwpClient) -> Self {
        Self { jdwp }
    }

    fn client(&self) -> &JdwpClient {
        &self.jdwp
    }

    pub async fn runtime_type_name(&mut self, object_id: ObjectId) -> Result<String> {
        let (_ref_type_tag, type_id) = self
            .client()
            .object_reference_reference_type(object_id)
            .await?;
        let signature = self
            .client()
            .reference_type_signature_cached(type_id)
            .await?;
        Ok(signature_to_type_name(&signature))
    }

    pub async fn preview_object(&mut self, object_id: ObjectId) -> Result<ObjectPreview> {
        preview_object(self.client(), object_id).await
    }

    pub async fn object_children(&mut self, object_id: ObjectId) -> Result<Vec<InspectVariable>> {
        let children = object_children(self.client(), object_id).await?;
        Ok(children
            .into_iter()
            .map(|(name, value, static_type)| InspectVariable {
                name,
                value,
                static_type,
            })
            .collect())
    }
}

pub async fn preview_object(jdwp: &JdwpClient, object_id: ObjectId) -> Result<ObjectPreview> {
    let (_ref_type_tag, type_id) = jdwp.object_reference_reference_type(object_id).await?;
    let signature = jdwp.reference_type_signature_cached(type_id).await?;
    let runtime_type = signature_to_type_name(&signature);

    if signature == "Ljava/lang/String;" {
        return Ok(ObjectPreview {
            runtime_type,
            kind: ObjectKindPreview::String {
                value: jdwp.string_reference_value(object_id).await?,
            },
        });
    }

    if signature.starts_with('[') {
        let length = jdwp.array_reference_length(object_id).await?;
        let length = length.max(0) as usize;
        let sample_len = length.min(ARRAY_PREVIEW_SAMPLE);
        let sample = if sample_len == 0 {
            Vec::new()
        } else {
            jdwp.array_reference_get_values(object_id, 0, sample_len as i32)
                .await?
        };

        let element_sig = signature.strip_prefix('[').unwrap_or(&signature);
        let element_type = signature_to_type_name(element_sig);
        return Ok(ObjectPreview {
            runtime_type,
            kind: ObjectKindPreview::Array {
                element_type,
                length,
                sample,
            },
        });
    }

    if is_primitive_wrapper(&runtime_type) {
        if let Ok(children) = instance_fields_with_type(jdwp, object_id, type_id).await {
            if let Some(value) = children
                .iter()
                .find(|(name, ..)| name == "value")
                .map(|v| v.1.clone())
            {
                return Ok(ObjectPreview {
                    runtime_type,
                    kind: ObjectKindPreview::PrimitiveWrapper {
                        value: Box::new(value),
                    },
                });
            }
        }
    }

    if runtime_type == "java.util.Optional" {
        if let Ok(children) = instance_fields_with_type(jdwp, object_id, type_id).await {
            if let Some(value) = children
                .iter()
                .find(|(name, ..)| name == "value")
                .map(|v| v.1.clone())
            {
                return Ok(ObjectPreview {
                    runtime_type,
                    kind: ObjectKindPreview::Optional {
                        value: match value {
                            v if is_null(&v) => None,
                            other => Some(Box::new(other)),
                        },
                    },
                });
            }
        }
    }

    if runtime_type == "java.util.ArrayList" {
        if let Ok(children) = instance_fields_with_type(jdwp, object_id, type_id).await {
            let size =
                children
                    .iter()
                    .find_map(|(name, value, _ty)| match (name.as_str(), value) {
                        ("size", JdwpValue::Int(size)) => Some((*size).max(0) as usize),
                        _ => None,
                    });
            let element_data =
                children
                    .iter()
                    .find_map(|(name, value, _ty)| match (name.as_str(), value) {
                        ("elementData", JdwpValue::Object { id, .. }) if *id != 0 => Some(*id),
                        _ => None,
                    });

            if let Some(size) = size {
                let sample_len = size.min(ARRAY_PREVIEW_SAMPLE);
                let sample = match (sample_len, element_data) {
                    (0, _) => Vec::new(),
                    (_, Some(array_id)) => jdwp
                        .array_reference_get_values(array_id, 0, sample_len as i32)
                        .await
                        .unwrap_or_default(),
                    _ => Vec::new(),
                };

                return Ok(ObjectPreview {
                    runtime_type,
                    kind: ObjectKindPreview::List { size, sample },
                });
            }
        }
    }

    if runtime_type == "java.util.LinkedList" {
        if let Ok(children) = instance_fields_with_type(jdwp, object_id, type_id).await {
            let size =
                children
                    .iter()
                    .find_map(|(name, value, _ty)| match (name.as_str(), value) {
                        ("size", JdwpValue::Int(size)) => Some((*size).max(0) as usize),
                        _ => None,
                    });
            let first =
                children
                    .iter()
                    .find_map(|(name, value, _ty)| match (name.as_str(), value) {
                        ("first", JdwpValue::Object { id, .. }) if *id != 0 => Some(*id),
                        _ => None,
                    });

            if let Some(size) = size {
                let mut sample = Vec::new();
                let sample_len = size.min(ARRAY_PREVIEW_SAMPLE);
                let mut node_id = match first {
                    Some(id) if sample_len > 0 => id,
                    _ => 0,
                };
                for _ in 0..HASHMAP_CHAIN_LIMIT {
                    if sample.len() >= sample_len {
                        break;
                    }
                    if node_id == 0 {
                        break;
                    }
                    let Ok(node_children) = object_children(jdwp, node_id).await else {
                        break;
                    };
                    let item = node_children
                        .iter()
                        .find(|(name, ..)| name == "item")
                        .map(|v| v.1.clone())
                        .unwrap_or_else(null_object);
                    sample.push(item);
                    node_id = node_children
                        .iter()
                        .find_map(|(name, value, _ty)| match (name.as_str(), value) {
                            ("next", JdwpValue::Object { id, .. }) => Some(*id),
                            _ => None,
                        })
                        .unwrap_or(0);
                }

                return Ok(ObjectPreview {
                    runtime_type,
                    kind: ObjectKindPreview::List { size, sample },
                });
            }
        }
    }

    if runtime_type == "java.util.HashMap" {
        if let Some((size, mut sample)) =
            hashmap_entries(jdwp, object_id, type_id, ARRAY_PREVIEW_SAMPLE).await
        {
            sort_map_sample(jdwp, &mut sample).await;
            return Ok(ObjectPreview {
                runtime_type,
                kind: ObjectKindPreview::Map { size, sample },
            });
        }
    }

    if runtime_type == "java.util.HashSet" {
        if let Ok(children) = instance_fields_with_type(jdwp, object_id, type_id).await {
            let map_id =
                children
                    .iter()
                    .find_map(|(name, value, _ty)| match (name.as_str(), value) {
                        ("map", JdwpValue::Object { id, .. }) if *id != 0 => Some(*id),
                        _ => None,
                    });

            if let Some(map_id) = map_id {
                let map_type_id = match jdwp.object_reference_reference_type(map_id).await {
                    Ok((_ref_type_tag, type_id)) => type_id,
                    Err(_) => {
                        return Ok(ObjectPreview {
                            runtime_type,
                            kind: ObjectKindPreview::Plain,
                        });
                    }
                };
                if let Some((size, sample)) =
                    hashmap_entries(jdwp, map_id, map_type_id, ARRAY_PREVIEW_SAMPLE).await
                {
                    let mut keys: Vec<_> = sample.into_iter().map(|(k, _)| k).collect();
                    sort_set_sample(jdwp, &mut keys).await;
                    return Ok(ObjectPreview {
                        runtime_type,
                        kind: ObjectKindPreview::Set { size, sample: keys },
                    });
                }
            }
        }
    }

    if runtime_type.starts_with("java.util.stream.") {
        return Ok(ObjectPreview {
            runtime_type,
            kind: ObjectKindPreview::Stream { size: None },
        });
    }

    Ok(ObjectPreview {
        runtime_type,
        kind: ObjectKindPreview::Plain,
    })
}

pub async fn object_children(
    jdwp: &JdwpClient,
    object_id: ObjectId,
) -> Result<Vec<(String, JdwpValue, Option<String>)>> {
    let (_ref_type_tag, type_id) = jdwp.object_reference_reference_type(object_id).await?;
    let signature = jdwp.reference_type_signature_cached(type_id).await?;

    if signature.starts_with('[') {
        let length = jdwp.array_reference_length(object_id).await?;
        let length = length.max(0) as usize;
        let sample_len = length.min(ARRAY_CHILD_SAMPLE);
        let element_sig = signature.strip_prefix('[').unwrap_or(&signature);
        let element_type = signature_to_type_name(element_sig);

        let mut vars = Vec::new();
        vars.push((
            "length".to_string(),
            JdwpValue::Int(length as i32),
            Some("int".to_string()),
        ));

        if sample_len > 0 {
            let values = jdwp
                .array_reference_get_values(object_id, 0, sample_len as i32)
                .await?;
            for (idx, value) in values.into_iter().enumerate() {
                vars.push((format!("[{idx}]"), value, Some(element_type.clone())));
            }
        }
        return Ok(vars);
    }

    let runtime_type = signature_to_type_name(&signature);

    match runtime_type.as_str() {
        "java.util.ArrayList" => {
            if let Some(vars) = array_list_children(jdwp, object_id, type_id).await? {
                return Ok(vars);
            }
        }
        "java.util.LinkedList" => {
            if let Some(vars) = linked_list_children(jdwp, object_id, type_id).await? {
                return Ok(vars);
            }
        }
        "java.util.HashMap" => {
            if let Some(vars) = hash_map_children(jdwp, object_id, type_id).await? {
                return Ok(vars);
            }
        }
        "java.util.HashSet" => {
            if let Some(vars) = hash_set_children(jdwp, object_id, type_id).await? {
                return Ok(vars);
            }
        }
        _ => {}
    }

    instance_fields_with_type(jdwp, object_id, type_id).await
}

async fn instance_fields_with_type(
    jdwp: &JdwpClient,
    object_id: ObjectId,
    type_id: ReferenceTypeId,
) -> Result<Vec<(String, JdwpValue, Option<String>)>> {
    let fields: Vec<FieldInfo> = jdwp
        .reference_type_all_instance_fields_cached(type_id)
        .await?;
    if fields.is_empty() {
        return Ok(Vec::new());
    }

    let field_ids: Vec<u64> = fields.iter().map(|f| f.field_id).collect();
    let values = jdwp
        .object_reference_get_values(object_id, &field_ids)
        .await?;
    Ok(fields
        .into_iter()
        .zip(values)
        .map(|(field, value)| {
            (
                field.name,
                value,
                Some(signature_to_type_name(&field.signature)),
            )
        })
        .collect())
}

async fn instance_fields(
    jdwp: &JdwpClient,
    object_id: ObjectId,
) -> Result<Vec<(String, JdwpValue, Option<String>)>> {
    let (_ref_type_tag, type_id) = jdwp.object_reference_reference_type(object_id).await?;
    instance_fields_with_type(jdwp, object_id, type_id).await
}

async fn array_list_children(
    jdwp: &JdwpClient,
    object_id: ObjectId,
    type_id: ReferenceTypeId,
) -> Result<Option<Vec<(String, JdwpValue, Option<String>)>>> {
    let fields = instance_fields_with_type(jdwp, object_id, type_id).await?;
    let Some(size) = fields
        .iter()
        .find_map(|(name, value, _ty)| match (name.as_str(), value) {
            ("size", JdwpValue::Int(size)) => Some((*size).max(0) as usize),
            _ => None,
        })
    else {
        return Ok(None);
    };
    let element_data = fields
        .iter()
        .find_map(|(name, value, _ty)| match (name.as_str(), value) {
            ("elementData", JdwpValue::Object { id, .. }) if *id != 0 => Some(*id),
            _ => None,
        });

    let mut vars = Vec::new();
    vars.push((
        "size".to_string(),
        JdwpValue::Int(size as i32),
        Some("int".to_string()),
    ));

    let array_id = element_data.unwrap_or(0);
    let sample_len = size.min(ARRAY_CHILD_SAMPLE);
    if array_id != 0 && sample_len > 0 {
        let element_type = match jdwp.object_reference_reference_type(array_id).await {
            Ok((_ref_type_tag, array_type)) => jdwp
                .reference_type_signature_cached(array_type)
                .await
                .ok()
                .and_then(|sig| sig.strip_prefix('[').map(signature_to_type_name)),
            Err(_) => None,
        };
        let values = jdwp
            .array_reference_get_values(array_id, 0, sample_len as i32)
            .await
            .unwrap_or_default();
        for (idx, value) in values.into_iter().enumerate() {
            vars.push((format!("[{idx}]"), value, element_type.clone()));
        }
    }

    Ok(Some(vars))
}

async fn linked_list_children(
    jdwp: &JdwpClient,
    object_id: ObjectId,
    type_id: ReferenceTypeId,
) -> Result<Option<Vec<(String, JdwpValue, Option<String>)>>> {
    let fields = instance_fields_with_type(jdwp, object_id, type_id).await?;
    let Some(size) = fields
        .iter()
        .find_map(|(name, value, _ty)| match (name.as_str(), value) {
            ("size", JdwpValue::Int(size)) => Some((*size).max(0) as usize),
            _ => None,
        })
    else {
        return Ok(None);
    };
    let mut node_id = fields
        .iter()
        .find_map(|(name, value, _ty)| match (name.as_str(), value) {
            ("first", JdwpValue::Object { id, .. }) if *id != 0 => Some(*id),
            _ => None,
        })
        .unwrap_or(0);

    let mut vars = Vec::new();
    vars.push((
        "size".to_string(),
        JdwpValue::Int(size as i32),
        Some("int".to_string()),
    ));

    let sample_len = size.min(ARRAY_CHILD_SAMPLE);
    for idx in 0..sample_len {
        if node_id == 0 {
            break;
        }
        let Ok(node_children) = instance_fields(jdwp, node_id).await else {
            break;
        };
        let mut item: Option<(JdwpValue, Option<String>)> = None;
        let mut next: Option<ObjectId> = None;
        for (name, value, ty) in node_children {
            match name.as_str() {
                "item" => item = Some((value, ty)),
                "next" => {
                    if let JdwpValue::Object { id, .. } = value {
                        next = Some(id);
                    }
                }
                _ => {}
            }
        }

        let (value, static_type) = item.unwrap_or_else(|| (null_object(), None));
        vars.push((format!("[{idx}]"), value, static_type));
        node_id = next.unwrap_or(0);
    }

    Ok(Some(vars))
}

async fn hash_map_children(
    jdwp: &JdwpClient,
    object_id: ObjectId,
    type_id: ReferenceTypeId,
) -> Result<Option<Vec<(String, JdwpValue, Option<String>)>>> {
    let Some((size, mut entries)) =
        hashmap_entries(jdwp, object_id, type_id, ARRAY_CHILD_SAMPLE).await
    else {
        return Ok(None);
    };

    sort_map_sample(jdwp, &mut entries).await;

    let mut vars = Vec::new();
    vars.push((
        "size".to_string(),
        JdwpValue::Int(size as i32),
        Some("int".to_string()),
    ));

    for (key, value) in entries {
        let name = map_key_display(jdwp, &key).await;
        vars.push((name, value, None));
    }

    Ok(Some(vars))
}

async fn hash_set_children(
    jdwp: &JdwpClient,
    object_id: ObjectId,
    type_id: ReferenceTypeId,
) -> Result<Option<Vec<(String, JdwpValue, Option<String>)>>> {
    let fields = instance_fields_with_type(jdwp, object_id, type_id).await?;
    let Some(map_id) = fields
        .iter()
        .find_map(|(name, value, _ty)| match (name.as_str(), value) {
            ("map", JdwpValue::Object { id, .. }) if *id != 0 => Some(*id),
            _ => None,
        })
    else {
        return Ok(None);
    };

    let map_type_id = match jdwp.object_reference_reference_type(map_id).await {
        Ok((_ref_type_tag, type_id)) => type_id,
        Err(_) => return Ok(None),
    };
    let Some((size, entries)) =
        hashmap_entries(jdwp, map_id, map_type_id, ARRAY_CHILD_SAMPLE).await
    else {
        return Ok(None);
    };

    let mut keys: Vec<_> = entries.into_iter().map(|(k, _)| k).collect();
    sort_set_sample(jdwp, &mut keys).await;

    let mut vars = Vec::new();
    vars.push((
        "size".to_string(),
        JdwpValue::Int(size as i32),
        Some("int".to_string()),
    ));

    for (idx, value) in keys.into_iter().enumerate() {
        vars.push((format!("[{idx}]"), value, None));
    }

    Ok(Some(vars))
}

async fn map_key_display(jdwp: &JdwpClient, key: &JdwpValue) -> String {
    match key {
        JdwpValue::Object { id: 0, .. } => "null".to_string(),
        JdwpValue::Object { id, tag } => {
            if *tag == b's' {
                if let Ok(value) = jdwp.string_reference_value(*id).await {
                    return format!("\"{}\"", escape_java_string(&value, 40));
                }
            }

            let is_string = match jdwp.object_reference_reference_type(*id).await {
                Ok((_ref_type_tag, type_id)) => jdwp
                    .reference_type_signature_cached(type_id)
                    .await
                    .map(|sig| sig == "Ljava/lang/String;")
                    .unwrap_or(false),
                Err(_) => false,
            };

            if is_string {
                if let Ok(value) = jdwp.string_reference_value(*id).await {
                    return format!("\"{}\"", escape_java_string(&value, 40));
                }
            }

            format!("@0x{id:x}")
        }
        JdwpValue::Boolean(v) => v.to_string(),
        JdwpValue::Byte(v) => v.to_string(),
        JdwpValue::Char(v) => {
            let ch = char::from_u32(u32::from(*v)).unwrap_or('\u{FFFD}');
            format!("'{ch}'")
        }
        JdwpValue::Short(v) => v.to_string(),
        JdwpValue::Int(v) => v.to_string(),
        JdwpValue::Long(v) => v.to_string(),
        JdwpValue::Float(v) => v.to_string(),
        JdwpValue::Double(v) => v.to_string(),
        JdwpValue::Void => "void".to_string(),
    }
}

fn escape_java_string(input: &str, max_len: usize) -> String {
    let mut out = String::new();
    for (used, ch) in input.chars().enumerate() {
        if used >= max_len {
            out.push('…');
            break;
        }
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out
}

fn is_null(value: &JdwpValue) -> bool {
    matches!(value, JdwpValue::Object { id: 0, .. })
}

fn null_object() -> JdwpValue {
    JdwpValue::Object { tag: b'L', id: 0 }
}

fn is_primitive_wrapper(runtime_type: &str) -> bool {
    matches!(
        runtime_type,
        "java.lang.Boolean"
            | "java.lang.Byte"
            | "java.lang.Character"
            | "java.lang.Double"
            | "java.lang.Float"
            | "java.lang.Integer"
            | "java.lang.Long"
            | "java.lang.Short"
    )
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum SortKey {
    Null,
    Void,
    Bool(bool),
    Char(u16),
    I128(i128),
    U64(u64),
    String(String),
    Object(ObjectId),
}

async fn hashmap_entries(
    jdwp: &JdwpClient,
    object_id: ObjectId,
    type_id: ReferenceTypeId,
    entry_limit: usize,
) -> Option<(usize, Vec<(JdwpValue, JdwpValue)>)> {
    let children = instance_fields_with_type(jdwp, object_id, type_id)
        .await
        .ok()?;
    let size = children
        .iter()
        .find_map(|(name, value, _ty)| match (name.as_str(), value) {
            ("size", JdwpValue::Int(size)) => Some((*size).max(0) as usize),
            _ => None,
        })?;
    let table_id = children
        .iter()
        .find_map(|(name, value, _ty)| match (name.as_str(), value) {
            ("table", JdwpValue::Object { id, .. }) if *id != 0 => Some(*id),
            _ => None,
        });

    let mut sample = Vec::new();
    if let Some(table_id) = table_id {
        if let Ok(table_len) = jdwp.array_reference_length(table_id).await {
            let table_len = table_len.max(0) as usize;
            let scan = table_len.min(HASHMAP_SCAN_LIMIT);
            if scan > 0 {
                if let Ok(buckets) = jdwp
                    .array_reference_get_values(table_id, 0, scan as i32)
                    .await
                {
                    for bucket in buckets {
                        if sample.len() >= entry_limit {
                            break;
                        }
                        let JdwpValue::Object {
                            id: mut node_id, ..
                        } = bucket
                        else {
                            continue;
                        };
                        if node_id == 0 {
                            continue;
                        }
                        for _ in 0..HASHMAP_CHAIN_LIMIT {
                            if sample.len() >= entry_limit {
                                break;
                            }
                            if node_id == 0 {
                                break;
                            }
                            let Ok(node_fields) = instance_fields(jdwp, node_id).await else {
                                break;
                            };
                            let key = node_fields
                                .iter()
                                .find(|(name, ..)| name == "key")
                                .map(|v| v.1.clone())
                                .unwrap_or_else(null_object);
                            let value = node_fields
                                .iter()
                                .find(|(name, ..)| name == "value")
                                .map(|v| v.1.clone())
                                .unwrap_or_else(null_object);
                            sample.push((key, value));

                            node_id = node_fields
                                .iter()
                                .find_map(|(name, value, _ty)| match (name.as_str(), value) {
                                    ("next", JdwpValue::Object { id, .. }) => Some(*id),
                                    _ => None,
                                })
                                .unwrap_or(0);
                        }
                    }
                }
            }
        }
    }

    Some((size, sample))
}

async fn sort_key(jdwp: &JdwpClient, value: &JdwpValue) -> SortKey {
    match value {
        JdwpValue::Boolean(v) => SortKey::Bool(*v),
        JdwpValue::Byte(v) => SortKey::I128(i128::from(*v)),
        JdwpValue::Char(v) => SortKey::Char(*v),
        JdwpValue::Short(v) => SortKey::I128(i128::from(*v)),
        JdwpValue::Int(v) => SortKey::I128(i128::from(*v)),
        JdwpValue::Long(v) => SortKey::I128(i128::from(*v)),
        JdwpValue::Float(v) => SortKey::U64(v.to_bits().into()),
        JdwpValue::Double(v) => SortKey::U64(v.to_bits()),
        JdwpValue::Void => SortKey::Void,
        JdwpValue::Object { id, .. } => {
            if *id == 0 {
                return SortKey::Null;
            }

            let is_string = match jdwp.object_reference_reference_type(*id).await {
                Ok((_ref_type_tag, type_id)) => jdwp
                    .reference_type_signature_cached(type_id)
                    .await
                    .map(|sig| sig == "Ljava/lang/String;")
                    .unwrap_or(false),
                Err(_) => false,
            };

            if is_string {
                match jdwp.string_reference_value(*id).await {
                    Ok(value) => SortKey::String(value),
                    Err(_) => SortKey::Object(*id),
                }
            } else {
                SortKey::Object(*id)
            }
        }
    }
}

async fn sort_set_sample(jdwp: &JdwpClient, sample: &mut [JdwpValue]) {
    let mut decorated: Vec<_> = Vec::with_capacity(sample.len());
    for value in sample.iter() {
        decorated.push((sort_key(jdwp, value).await, value.clone()));
    }
    decorated.sort_by(|(a, _), (b, _)| a.cmp(b));
    for (dst, (_key, value)) in sample.iter_mut().zip(decorated.into_iter()) {
        *dst = value;
    }
}

async fn sort_map_sample(jdwp: &JdwpClient, sample: &mut [(JdwpValue, JdwpValue)]) {
    let mut decorated = Vec::with_capacity(sample.len());
    for (k, v) in sample.iter() {
        decorated.push((
            (sort_key(jdwp, k).await, sort_key(jdwp, v).await),
            (k.clone(), v.clone()),
        ));
    }
    decorated.sort_by(|(a, _), (b, _)| a.cmp(b));
    for (dst, (_key, value)) in sample.iter_mut().zip(decorated.into_iter()) {
        *dst = value;
    }
}

pub fn signature_to_type_name(signature: &str) -> String {
    let mut sig = signature;
    let mut dims = 0usize;
    while let Some(rest) = sig.strip_prefix('[') {
        dims += 1;
        sig = rest;
    }

    let base = if let Some(class) = sig.strip_prefix('L').and_then(|s| s.strip_suffix(';')) {
        class.replace('/', ".")
    } else {
        match sig.as_bytes().first().copied() {
            Some(b'B') => "byte".to_string(),
            Some(b'C') => "char".to_string(),
            Some(b'D') => "double".to_string(),
            Some(b'F') => "float".to_string(),
            Some(b'I') => "int".to_string(),
            Some(b'J') => "long".to_string(),
            Some(b'S') => "short".to_string(),
            Some(b'Z') => "boolean".to_string(),
            Some(b'V') => "void".to_string(),
            _ => "<unknown>".to_string(),
        }
    };

    let mut out = base;
    for _ in 0..dims {
        out.push_str("[]");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::Inspector;
    use crate::wire::{mock, JdwpClient, JdwpValue};

    #[tokio::test]
    async fn object_children_includes_inherited_fields() {
        let server = mock::MockJdwpServer::spawn().await.unwrap();
        let client = JdwpClient::connect(server.addr()).await.unwrap();
        let mut inspector = Inspector::new(client);

        let children = inspector.object_children(mock::EXCEPTION_ID).await.unwrap();
        assert!(
            children.iter().any(|child| child.name == "detailMessage"),
            "expected Throwable.detailMessage to be present in object children"
        );
    }

    #[tokio::test]
    async fn object_children_prefers_most_derived_field_when_names_collide() {
        let server = mock::MockJdwpServer::spawn().await.unwrap();
        let client = JdwpClient::connect(server.addr()).await.unwrap();
        let mut inspector = Inspector::new(client);

        let children = inspector
            .object_children(mock::FIELD_HIDING_OBJECT_ID)
            .await
            .unwrap();
        let matches: Vec<_> = children
            .iter()
            .filter(|child| child.name == "hidden")
            .collect();
        assert_eq!(matches.len(), 1, "expected a single `hidden` field");

        assert_eq!(
            matches[0].value,
            JdwpValue::Int(1),
            "expected the most-derived `hidden` field value"
        );
    }
}
