use std::collections::HashMap;

use super::{
    types::{FieldInfo, JdwpValue, ObjectId, ReferenceTypeId, Result},
    JdwpClient,
};

const ARRAY_PREVIEW_SAMPLE: usize = 3;
const ARRAY_CHILD_SAMPLE: usize = 25;

/// JDWP `Error.INVALID_OBJECT` (the object has already been garbage collected).
pub const ERROR_INVALID_OBJECT: u16 = 20;

/// `Field` modifier bit for `static` fields (ignored in instance inspection).
const FIELD_MODIFIER_STATIC: u32 = 0x0008;

#[derive(Clone, Debug, PartialEq)]
pub struct ObjectPreview {
    pub runtime_type: String,
    pub kind: ObjectKindPreview,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ObjectKindPreview {
    Plain,
    String { value: String },
    PrimitiveWrapper { value: Box<JdwpValue> },
    Array {
        element_type: String,
        length: usize,
        sample: Vec<JdwpValue>,
    },
    List { size: usize, sample: Vec<JdwpValue> },
    Set { size: usize, sample: Vec<JdwpValue> },
    Map {
        size: usize,
        sample: Vec<(JdwpValue, JdwpValue)>,
    },
    Optional { value: Option<Box<JdwpValue>> },
    Stream { size: Option<usize> },
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
    signature_cache: HashMap<ReferenceTypeId, String>,
    fields_cache: HashMap<ReferenceTypeId, Vec<FieldInfo>>,
}

impl Inspector {
    pub fn new(jdwp: JdwpClient) -> Self {
        Self {
            jdwp,
            signature_cache: HashMap::new(),
            fields_cache: HashMap::new(),
        }
    }

    fn client(&self) -> &JdwpClient {
        &self.jdwp
    }

    pub async fn runtime_type_name(&mut self, object_id: ObjectId) -> Result<String> {
        let type_id = self.client().object_reference_reference_type(object_id).await?;
        let signature = self.signature_for_type(type_id).await?;
        Ok(signature_to_type_name(&signature))
    }

    pub async fn preview_object(&mut self, object_id: ObjectId) -> Result<ObjectPreview> {
        let type_id = self.client().object_reference_reference_type(object_id).await?;
        let signature = self.signature_for_type(type_id).await?;
        let runtime_type = signature_to_type_name(&signature);

        if signature == "Ljava/lang/String;" {
            return Ok(ObjectPreview {
                runtime_type,
                kind: ObjectKindPreview::String {
                    value: self.client().string_reference_value(object_id).await?,
                },
            });
        }

        if signature.starts_with('[') {
            let length = self.client().array_reference_length(object_id).await?.max(0) as usize;
            let sample_len = length.min(ARRAY_PREVIEW_SAMPLE);
            let sample = if sample_len == 0 {
                Vec::new()
            } else {
                self.client()
                    .array_reference_get_values(object_id, 0, sample_len as i32)
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

        // Primitive wrapper previews (Integer, Long, etc.) by reading their `value` field.
        if matches!(
            runtime_type.as_str(),
            "java.lang.Boolean"
                | "java.lang.Byte"
                | "java.lang.Character"
                | "java.lang.Double"
                | "java.lang.Float"
                | "java.lang.Integer"
                | "java.lang.Long"
                | "java.lang.Short"
        ) {
            if let Ok(children) = self.object_children(object_id).await {
                if let Some(value) = children
                    .iter()
                    .find(|v| v.name == "value")
                    .map(|v| v.value.clone())
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

        // Optional preview by reading its `value` field.
        if runtime_type == "java.util.Optional" {
            if let Ok(children) = self.object_children(object_id).await {
                if let Some(value) = children
                    .iter()
                    .find(|v| v.name == "value")
                    .map(|v| v.value.clone())
                {
                    return Ok(ObjectPreview {
                        runtime_type,
                        kind: ObjectKindPreview::Optional {
                            value: match value {
                                JdwpValue::Object { id: 0, .. } => None,
                                other => Some(Box::new(other)),
                            },
                        },
                    });
                }
            }
        }

        // Collection previews via best-effort field introspection for common JDK implementations.
        if runtime_type == "java.util.ArrayList" {
            if let Ok(children) = self.object_children(object_id).await {
                let size = children.iter().find_map(|v| match (&v.name[..], &v.value) {
                    ("size", JdwpValue::Int(size)) => Some((*size).max(0) as usize),
                    _ => None,
                });
                let element_data = children.iter().find_map(|v| match (&v.name[..], &v.value) {
                    ("elementData", JdwpValue::Object { id, .. }) if *id != 0 => Some(*id),
                    _ => None,
                });

                if let (Some(size), Some(array_id)) = (size, element_data) {
                    let sample_len = size.min(ARRAY_PREVIEW_SAMPLE);
                    let sample = if sample_len == 0 {
                        Vec::new()
                    } else if let Ok(values) = self
                        .client()
                        .array_reference_get_values(array_id, 0, sample_len as i32)
                        .await
                    {
                        values
                    } else {
                        Vec::new()
                    };

                    return Ok(ObjectPreview {
                        runtime_type,
                        kind: ObjectKindPreview::List { size, sample },
                    });
                }
            }
        }

        if runtime_type == "java.util.HashMap" {
            if let Ok(children) = self.object_children(object_id).await {
                let size = children.iter().find_map(|v| match (&v.name[..], &v.value) {
                    ("size", JdwpValue::Int(size)) => Some((*size).max(0) as usize),
                    _ => None,
                });
                let table = children.iter().find_map(|v| match (&v.name[..], &v.value) {
                    ("table", JdwpValue::Object { id, .. }) if *id != 0 => Some(*id),
                    _ => None,
                });

                if let (Some(size), Some(table_id)) = (size, table) {
                    let mut sample = Vec::new();
                    if let Ok(table_len) = self.client().array_reference_length(table_id).await {
                        let table_len = table_len.max(0) as usize;
                        let scan = table_len.min(64);
                        if scan > 0 {
                            if let Ok(buckets) = self
                                .client()
                                .array_reference_get_values(table_id, 0, scan as i32)
                                .await
                            {
                                for bucket in buckets {
                                    if sample.len() >= ARRAY_PREVIEW_SAMPLE {
                                        break;
                                    }
                                    let JdwpValue::Object { id: mut node_id, .. } = bucket else {
                                        continue;
                                    };
                                    if node_id == 0 {
                                        continue;
                                    }

                                    // Traverse collision chain (bounded).
                                    for _ in 0..16 {
                                        if sample.len() >= ARRAY_PREVIEW_SAMPLE {
                                            break;
                                        }
                                        let Ok(node_fields) = self.object_children(node_id).await else {
                                            break;
                                        };
                                        let key = node_fields
                                            .iter()
                                            .find(|v| v.name == "key")
                                            .map(|v| v.value.clone())
                                            .unwrap_or_else(null_value);
                                        let value = node_fields
                                            .iter()
                                            .find(|v| v.name == "value")
                                            .map(|v| v.value.clone())
                                            .unwrap_or_else(null_value);
                                        sample.push((key, value));

                                        match node_fields.iter().find(|v| v.name == "next").map(|v| &v.value) {
                                            Some(JdwpValue::Object { id: next_id, .. }) if *next_id != 0 => {
                                                node_id = *next_id
                                            }
                                            _ => break,
                                        }
                                    }
                                }
                            }
                        }
                    }

                    return Ok(ObjectPreview {
                        runtime_type,
                        kind: ObjectKindPreview::Map { size, sample },
                    });
                }
            }
        }

        if runtime_type == "java.util.HashSet" {
            if let Ok(children) = self.object_children(object_id).await {
                let map = children.iter().find_map(|v| match (&v.name[..], &v.value) {
                    ("map", JdwpValue::Object { id, .. }) if *id != 0 => Some(*id),
                    _ => None,
                });

                if let Some(map_id) = map {
                    // Reuse HashMap's preview by pulling the keys out of the sampled entries.
                    if let Ok(ObjectPreview {
                        kind: ObjectKindPreview::Map { size, sample },
                        ..
                    }) = self.preview_object(map_id).await
                    {
                        let sample = sample.into_iter().map(|(k, _)| k).collect();
                        return Ok(ObjectPreview {
                            runtime_type,
                            kind: ObjectKindPreview::Set { size, sample },
                        });
                    }
                }
            }
        }

        Ok(ObjectPreview {
            runtime_type,
            kind: ObjectKindPreview::Plain,
        })
    }

    pub async fn object_children(&mut self, object_id: ObjectId) -> Result<Vec<InspectVariable>> {
        let type_id = self.client().object_reference_reference_type(object_id).await?;
        let signature = self.signature_for_type(type_id).await?;

        if signature.starts_with('[') {
            let length = self.client().array_reference_length(object_id).await?.max(0) as usize;
            let sample_len = length.min(ARRAY_CHILD_SAMPLE);
            let element_sig = signature.strip_prefix('[').unwrap_or(&signature);
            let element_type = signature_to_type_name(element_sig);
            let mut vars = Vec::new();
            vars.push(InspectVariable {
                name: "length".to_string(),
                value: JdwpValue::Int(length as i32),
                static_type: Some("int".to_string()),
            });
            if sample_len > 0 {
                let values = self
                    .client()
                    .array_reference_get_values(object_id, 0, sample_len as i32)
                    .await?;
                for (idx, value) in values.into_iter().enumerate() {
                    vars.push(InspectVariable {
                        name: format!("[{idx}]"),
                        value,
                        static_type: Some(element_type.clone()),
                    });
                }
            }
            return Ok(vars);
        }

        let fields: Vec<_> = self
            .fields_for_type(type_id)
            .await?
            .into_iter()
            .filter(|field| field.mod_bits & FIELD_MODIFIER_STATIC == 0)
            .collect();

        if fields.is_empty() {
            return Ok(Vec::new());
        }

        let field_ids: Vec<_> = fields.iter().map(|f| f.field_id).collect();
        let values = self
            .client()
            .object_reference_get_values(object_id, &field_ids)
            .await?;

        Ok(fields
            .into_iter()
            .zip(values)
            .map(|(field, value)| InspectVariable {
                name: field.name,
                value,
                static_type: Some(signature_to_type_name(&field.signature)),
            })
            .collect())
    }

    async fn signature_for_type(&mut self, type_id: ReferenceTypeId) -> Result<String> {
        if let Some(sig) = self.signature_cache.get(&type_id) {
            return Ok(sig.clone());
        }

        let sig = self.client().reference_type_signature(type_id).await?;
        self.signature_cache.insert(type_id, sig.clone());
        Ok(sig)
    }

    async fn fields_for_type(&mut self, type_id: ReferenceTypeId) -> Result<Vec<FieldInfo>> {
        if let Some(fields) = self.fields_cache.get(&type_id) {
            return Ok(fields.clone());
        }

        let fields = self.client().reference_type_fields(type_id).await?;
        self.fields_cache.insert(type_id, fields.clone());
        Ok(fields)
    }
}

fn null_value() -> JdwpValue {
    JdwpValue::Object { tag: b'L', id: 0 }
}

fn signature_to_type_name(signature: &str) -> String {
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
