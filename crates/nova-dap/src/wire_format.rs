use nova_jdwp::wire::{
    inspect::{self, ObjectKindPreview},
    JdwpClient, JdwpError, JdwpValue, ObjectId,
};

use crate::dap::types::VariablePresentationHint;

#[derive(Clone, Debug, PartialEq)]
pub struct FormattedValue {
    pub value: String,
    pub type_name: Option<String>,
    pub variables_reference: i64,
    pub presentation_hint: Option<VariablePresentationHint>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ObjectChildrenKind {
    ObjectFields,
    ArrayElements,
}

#[derive(Clone, Debug)]
pub struct ValueFormatter {
    max_string_len: usize,
    collection_sample_size: usize,
    max_preview_depth: usize,
}

impl Default for ValueFormatter {
    fn default() -> Self {
        Self {
            max_string_len: 80,
            collection_sample_size: 3,
            max_preview_depth: 2,
        }
    }
}

impl ValueFormatter {
    pub async fn format_value<F>(
        &self,
        jdwp: &JdwpClient,
        objects: &mut F,
        value: &JdwpValue,
        static_type: Option<&str>,
    ) -> Result<FormattedValue, JdwpError>
    where
        F: FnMut(ObjectId, ObjectChildrenKind) -> i64 + Send,
    {
        let (display, variables_reference, presentation_hint, runtime_type) =
            self.format_value_display(jdwp, objects, value, 0).await?;

        Ok(FormattedValue {
            value: display,
            type_name: static_type
                .map(|s| s.to_string())
                .or(runtime_type)
                .or_else(|| value_type_name(value)),
            variables_reference,
            presentation_hint,
        })
    }

    #[async_recursion::async_recursion]
    async fn format_value_display<F>(
        &self,
        jdwp: &JdwpClient,
        objects: &mut F,
        value: &JdwpValue,
        depth: usize,
    ) -> Result<
        (
            String,
            i64,
            Option<VariablePresentationHint>,
            Option<String>,
        ),
        JdwpError,
    >
    where
        F: FnMut(ObjectId, ObjectChildrenKind) -> i64 + Send,
    {
        match value {
            JdwpValue::Boolean(b) => Ok((b.to_string(), 0, None, None)),
            JdwpValue::Byte(v) => Ok((v.to_string(), 0, None, None)),
            JdwpValue::Char(c) => {
                let ch = char::from_u32(u32::from(*c)).unwrap_or('\u{FFFD}');
                Ok((format!("'{ch}'"), 0, None, None))
            }
            JdwpValue::Short(v) => Ok((v.to_string(), 0, None, None)),
            JdwpValue::Int(v) => Ok((v.to_string(), 0, None, None)),
            JdwpValue::Long(v) => Ok((v.to_string(), 0, None, None)),
            JdwpValue::Float(v) => Ok((trim_float(f64::from(*v)), 0, None, None)),
            JdwpValue::Double(v) => Ok((trim_float(*v), 0, None, None)),
            JdwpValue::Void => Ok(("void".to_string(), 0, None, None)),
            JdwpValue::Object { id, tag } => {
                self.format_object(jdwp, objects, *id, *tag, depth).await
            }
        }
    }

    async fn format_object<F>(
        &self,
        jdwp: &JdwpClient,
        objects: &mut F,
        object_id: ObjectId,
        tag: u8,
        depth: usize,
    ) -> Result<
        (
            String,
            i64,
            Option<VariablePresentationHint>,
            Option<String>,
        ),
        JdwpError,
    >
    where
        F: FnMut(ObjectId, ObjectChildrenKind) -> i64 + Send,
    {
        if object_id == 0 {
            return Ok(("null".to_string(), 0, None, None));
        }

        let child_kind = if tag == b'[' {
            ObjectChildrenKind::ArrayElements
        } else {
            ObjectChildrenKind::ObjectFields
        };
        let variables_reference = objects(object_id, child_kind);
        let handle = format!("@0x{object_id:x}");

        if depth >= self.max_preview_depth {
            return Ok((format!("object{handle}"), variables_reference, None, None));
        }

        let preview = match inspect::preview_object(jdwp, object_id).await {
            Ok(preview) => preview,
            Err(JdwpError::VmError(20)) => {
                return Ok((
                    format!("object{handle} <collected>"),
                    variables_reference,
                    Some(VariablePresentationHint {
                        kind: Some("virtual".to_string()),
                        attributes: Some(vec!["invalid".to_string()]),
                        visibility: None,
                        lazy: None,
                    }),
                    None,
                ));
            }
            Err(_err) => {
                return Ok((
                    format!("object{handle}"),
                    variables_reference,
                    Some(VariablePresentationHint {
                        kind: Some("data".to_string()),
                        attributes: None,
                        visibility: None,
                        lazy: None,
                    }),
                    None,
                ));
            }
        };

        let runtime_simple = simple_type_name(&preview.runtime_type).to_string();
        let display = match preview.kind {
            ObjectKindPreview::Plain => format!("{runtime_simple}{handle}"),
            ObjectKindPreview::String { value } => {
                let escaped = escape_java_string(&value, self.max_string_len);
                format!("\"{escaped}\"{handle}")
            }
            ObjectKindPreview::PrimitiveWrapper { ref value } => {
                let inner = self.format_inline(jdwp, objects, value, depth + 1).await?;
                format!("{runtime_simple}{handle}({inner})")
            }
            ObjectKindPreview::Array {
                element_type,
                length,
                sample,
            } => {
                let sample = self
                    .format_sample_list(jdwp, objects, &sample, depth + 1)
                    .await?;
                let element = simple_type_name(&element_type);
                format!("{element}[{length}]{handle} {{{sample}}}")
            }
            ObjectKindPreview::List { size, sample } => {
                let sample = self
                    .format_sample_list(jdwp, objects, &sample, depth + 1)
                    .await?;
                format!("{runtime_simple}{handle}(size={size}) [{sample}]")
            }
            ObjectKindPreview::Set { size, sample } => {
                let sample = self
                    .format_sample_list(jdwp, objects, &sample, depth + 1)
                    .await?;
                format!("{runtime_simple}{handle}(size={size}) [{sample}]")
            }
            ObjectKindPreview::Map { size, sample } => {
                let sample = self
                    .format_sample_map(jdwp, objects, &sample, depth + 1)
                    .await?;
                format!("{runtime_simple}{handle}(size={size}) {{{sample}}}")
            }
            ObjectKindPreview::Optional { value } => match value {
                Some(inner) => {
                    let inner = self.format_inline(jdwp, objects, &inner, depth + 1).await?;
                    format!("{runtime_simple}{handle}[{inner}]")
                }
                None => format!("{runtime_simple}{handle}.empty"),
            },
            ObjectKindPreview::Stream { size } => match size {
                Some(size) => format!("{runtime_simple}{handle}(size={size})"),
                None => format!("{runtime_simple}{handle}(size=unknown)"),
            },
        };

        Ok((
            display,
            variables_reference,
            Some(VariablePresentationHint {
                kind: Some("data".to_string()),
                attributes: None,
                visibility: None,
                lazy: None,
            }),
            Some(preview.runtime_type),
        ))
    }

    async fn format_inline<F>(
        &self,
        jdwp: &JdwpClient,
        objects: &mut F,
        value: &JdwpValue,
        depth: usize,
    ) -> Result<String, JdwpError>
    where
        F: FnMut(ObjectId, ObjectChildrenKind) -> i64 + Send,
    {
        let (display, _ref, _hint, _ty) = self
            .format_value_display(jdwp, objects, value, depth)
            .await?;
        Ok(display)
    }

    async fn format_sample_list<F>(
        &self,
        jdwp: &JdwpClient,
        objects: &mut F,
        sample: &[JdwpValue],
        depth: usize,
    ) -> Result<String, JdwpError>
    where
        F: FnMut(ObjectId, ObjectChildrenKind) -> i64 + Send,
    {
        let mut out = Vec::new();
        for value in sample.iter().take(self.collection_sample_size) {
            out.push(self.format_inline(jdwp, objects, value, depth).await?);
        }
        Ok(out.join(", "))
    }

    async fn format_sample_map<F>(
        &self,
        jdwp: &JdwpClient,
        objects: &mut F,
        sample: &[(JdwpValue, JdwpValue)],
        depth: usize,
    ) -> Result<String, JdwpError>
    where
        F: FnMut(ObjectId, ObjectChildrenKind) -> i64 + Send,
    {
        let mut out = Vec::new();
        for (k, v) in sample.iter().take(self.collection_sample_size) {
            let key = self.format_inline(jdwp, objects, k, depth).await?;
            let val = self.format_inline(jdwp, objects, v, depth).await?;
            out.push(format!("{key}={val}"));
        }
        Ok(out.join(", "))
    }
}

fn simple_type_name(full: &str) -> &str {
    let tail = full.rsplit('.').next().unwrap_or(full);
    tail.rsplit('$').next().unwrap_or(tail)
}

fn value_type_name(value: &JdwpValue) -> Option<String> {
    Some(match value {
        JdwpValue::Object { .. } => return None,
        JdwpValue::Void => "void".to_string(),
        JdwpValue::Boolean(_) => "boolean".to_string(),
        JdwpValue::Byte(_) => "byte".to_string(),
        JdwpValue::Char(_) => "char".to_string(),
        JdwpValue::Short(_) => "short".to_string(),
        JdwpValue::Int(_) => "int".to_string(),
        JdwpValue::Long(_) => "long".to_string(),
        JdwpValue::Float(_) => "float".to_string(),
        JdwpValue::Double(_) => "double".to_string(),
    })
}

fn trim_float(value: f64) -> String {
    if value.is_nan() || value.is_infinite() {
        return value.to_string();
    }
    if value.fract() == 0.0 {
        format!("{:.0}", value)
    } else {
        value.to_string()
    }
}

fn escape_java_string(input: &str, max_len: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for ch in input.chars() {
        if used >= max_len {
            out.push('…');
            break;
        }
        match ch {
            '\\' => out.push_str("\\\\"),
            '\"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
        used += 1;
    }
    out
}
