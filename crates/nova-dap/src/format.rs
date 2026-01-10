use nova_jdwp::{JdwpClient, JdwpError, JdwpValue, ObjectKindPreview};

use crate::dap::types::VariablePresentationHint;
use crate::error::{DebugError, DebugResult};
use crate::object_registry::ObjectRegistry;

#[derive(Clone, Debug, PartialEq)]
pub struct FormattedValue {
    pub value: String,
    pub type_name: Option<String>,
    pub variables_reference: i64,
    pub presentation_hint: Option<VariablePresentationHint>,
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
    pub fn format_value(
        &self,
        jdwp: &mut impl JdwpClient,
        objects: &mut ObjectRegistry,
        value: &JdwpValue,
        static_type: Option<&str>,
    ) -> DebugResult<FormattedValue> {
        let (display, variables_reference, presentation_hint) =
            self.format_value_display(jdwp, objects, value, 0)?;

        Ok(FormattedValue {
            value: display,
            type_name: static_type.map(|s| s.to_string()).or_else(|| value_type_name(value)),
            variables_reference,
            presentation_hint,
        })
    }

    fn format_value_display(
        &self,
        jdwp: &mut impl JdwpClient,
        objects: &mut ObjectRegistry,
        value: &JdwpValue,
        depth: usize,
    ) -> DebugResult<(String, i64, Option<VariablePresentationHint>)> {
        match value {
            JdwpValue::Null => Ok(("null".to_string(), 0, None)),
            JdwpValue::Void => Ok(("void".to_string(), 0, None)),
            JdwpValue::Boolean(b) => Ok((b.to_string(), 0, None)),
            JdwpValue::Byte(v) => Ok((v.to_string(), 0, None)),
            JdwpValue::Short(v) => Ok((v.to_string(), 0, None)),
            JdwpValue::Int(v) => Ok((v.to_string(), 0, None)),
            JdwpValue::Long(v) => Ok((v.to_string(), 0, None)),
            JdwpValue::Float(v) => Ok((trim_float(*v as f64), 0, None)),
            JdwpValue::Double(v) => Ok((trim_float(*v), 0, None)),
            JdwpValue::Char(c) => Ok((format!("'{c}'"), 0, None)),
            JdwpValue::Object(obj) => self.format_object(jdwp, objects, obj.id, &obj.runtime_type, depth),
        }
    }

    fn format_object(
        &self,
        jdwp: &mut impl JdwpClient,
        objects: &mut ObjectRegistry,
        object_id: nova_jdwp::ObjectId,
        runtime_type: &str,
        depth: usize,
    ) -> DebugResult<(String, i64, Option<VariablePresentationHint>)> {
        let handle = objects.track_object(object_id, runtime_type);
        let variables_reference = handle.as_variables_reference();

        if depth >= self.max_preview_depth {
            return Ok((format!("{}{handle}", simple_type_name(runtime_type)), variables_reference, None));
        }

        let preview = match jdwp.preview_object(object_id) {
            Ok(preview) => preview,
            Err(JdwpError::InvalidObjectId(_)) => {
                objects.mark_invalid_object_id(object_id);
                let ty = objects
                    .runtime_type(handle)
                    .map(simple_type_name)
                    .unwrap_or("<object>");
                return Ok((
                    format!("{ty}{handle} <collected>"),
                    variables_reference,
                    Some(VariablePresentationHint {
                        kind: Some("virtual".to_string()),
                        attributes: Some(vec!["invalid".to_string()]),
                        visibility: None,
                        lazy: None,
                    }),
                ));
            }
            Err(err) => return Err(DebugError::from(err)),
        };

        let runtime_simple = simple_type_name(&preview.runtime_type).to_string();
        let value = match preview.kind {
            ObjectKindPreview::Plain => format!("{runtime_simple}{handle}"),
            ObjectKindPreview::String { value } => {
                let escaped = escape_java_string(&value, self.max_string_len);
                format!("\"{escaped}\"{handle}")
            }
            ObjectKindPreview::PrimitiveWrapper { ref value } => {
                let inner = self.format_inline(jdwp, objects, value, depth + 1)?;
                format!("{runtime_simple}{handle}({inner})")
            }
            ObjectKindPreview::Array {
                element_type,
                length,
                sample,
            } => {
                let sample = self.format_sample_list(jdwp, objects, &sample, depth + 1)?;
                let element = simple_type_name(&element_type);
                format!("{element}[{length}]{handle} {{{sample}}}")
            }
            ObjectKindPreview::List { size, sample } => {
                let sample = self.format_sample_list(jdwp, objects, &sample, depth + 1)?;
                format!("{runtime_simple}{handle}(size={size}) [{sample}]")
            }
            ObjectKindPreview::Set { size, sample } => {
                let sample = self.format_sample_set(jdwp, objects, &sample, depth + 1)?;
                format!("{runtime_simple}{handle}(size={size}) [{sample}]")
            }
            ObjectKindPreview::Map { size, sample } => {
                let sample = self.format_sample_map(jdwp, objects, &sample, depth + 1)?;
                format!("{runtime_simple}{handle}(size={size}) {{{sample}}}")
            }
            ObjectKindPreview::Optional { value } => match value {
                Some(inner) => {
                    let inner = self.format_inline(jdwp, objects, &inner, depth + 1)?;
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
            value,
            variables_reference,
            Some(VariablePresentationHint {
                kind: Some("data".to_string()),
                attributes: None,
                visibility: None,
                lazy: None,
            }),
        ))
    }

    fn format_inline(
        &self,
        jdwp: &mut impl JdwpClient,
        objects: &mut ObjectRegistry,
        value: &JdwpValue,
        depth: usize,
    ) -> DebugResult<String> {
        let (display, _ref, _hint) = self.format_value_display(jdwp, objects, value, depth)?;
        Ok(display)
    }

    fn format_sample_list(
        &self,
        jdwp: &mut impl JdwpClient,
        objects: &mut ObjectRegistry,
        sample: &[JdwpValue],
        depth: usize,
    ) -> DebugResult<String> {
        let mut out = Vec::new();
        for value in sample.iter().take(self.collection_sample_size) {
            out.push(self.format_inline(jdwp, objects, value, depth)?);
        }
        Ok(join_comma(out))
    }

    fn format_sample_set(
        &self,
        jdwp: &mut impl JdwpClient,
        objects: &mut ObjectRegistry,
        sample: &[JdwpValue],
        depth: usize,
    ) -> DebugResult<String> {
        let mut sample: Vec<_> = sample.iter().cloned().collect();
        sample.sort_by_key(|v| self.sort_key(jdwp, v));

        self.format_sample_list(jdwp, objects, &sample, depth)
    }

    fn format_sample_map(
        &self,
        jdwp: &mut impl JdwpClient,
        objects: &mut ObjectRegistry,
        sample: &[(JdwpValue, JdwpValue)],
        depth: usize,
    ) -> DebugResult<String> {
        let mut sample: Vec<_> = sample.iter().cloned().collect();
        sample.sort_by_key(|(k, v)| (self.sort_key(jdwp, k), self.sort_key(jdwp, v)));

        let mut out = Vec::new();
        for (k, v) in sample.into_iter().take(self.collection_sample_size) {
            let key = self.format_inline(jdwp, objects, &k, depth)?;
            let val = self.format_inline(jdwp, objects, &v, depth)?;
            out.push(format!("{key}={val}"));
        }
        Ok(join_comma(out))
    }

    fn sort_key(&self, jdwp: &mut impl JdwpClient, value: &JdwpValue) -> SortKey {
        match value {
            JdwpValue::Null => SortKey::Null,
            JdwpValue::Void => SortKey::Void,
            JdwpValue::Boolean(v) => SortKey::Bool(*v),
            JdwpValue::Byte(v) => SortKey::I128(i128::from(*v)),
            JdwpValue::Short(v) => SortKey::I128(i128::from(*v)),
            JdwpValue::Int(v) => SortKey::I128(i128::from(*v)),
            JdwpValue::Long(v) => SortKey::I128(i128::from(*v)),
            JdwpValue::Float(v) => SortKey::U64((*v).to_bits().into()),
            JdwpValue::Double(v) => SortKey::U64(v.to_bits()),
            JdwpValue::Char(c) => SortKey::Char(*c),
            JdwpValue::Object(obj) => match jdwp.preview_object(obj.id) {
                Ok(preview) => match preview.kind {
                    ObjectKindPreview::String { value } => SortKey::String(value),
                    _ => SortKey::Object(obj.id),
                },
                Err(_) => SortKey::Object(obj.id),
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum SortKey {
    Null,
    Void,
    Bool(bool),
    Char(char),
    I128(i128),
    U64(u64),
    String(String),
    Object(nova_jdwp::ObjectId),
}

fn join_comma(items: Vec<String>) -> String {
    items.join(", ")
}

fn simple_type_name(full: &str) -> &str {
    let tail = full.rsplit('.').next().unwrap_or(full);
    tail.rsplit('$').next().unwrap_or(tail)
}

fn value_type_name(value: &JdwpValue) -> Option<String> {
    Some(match value {
        JdwpValue::Null => return None,
        JdwpValue::Void => "void".to_string(),
        JdwpValue::Boolean(_) => "boolean".to_string(),
        JdwpValue::Byte(_) => "byte".to_string(),
        JdwpValue::Short(_) => "short".to_string(),
        JdwpValue::Int(_) => "int".to_string(),
        JdwpValue::Long(_) => "long".to_string(),
        JdwpValue::Float(_) => "float".to_string(),
        JdwpValue::Double(_) => "double".to_string(),
        JdwpValue::Char(_) => "char".to_string(),
        JdwpValue::Object(obj) => obj.runtime_type.clone(),
    })
}

fn trim_float(value: f64) -> String {
    if value.is_nan() || value.is_infinite() {
        return value.to_string();
    }
    // Avoid `1.0` noise for integral floats while keeping the debugger's
    // output stable and human-friendly.
    if value.fract() == 0.0 {
        format!("{:.0}", value)
    } else {
        value.to_string()
    }
}

fn escape_java_string(input: &str, max_len: usize) -> String {
    let mut out = String::new();
    let mut chars = input.chars();
    let mut used = 0usize;
    while let Some(ch) = chars.next() {
        if used >= max_len {
            out.push('…');
            break;
        }
        match ch {
            '\\' => out.push_str("\\\\"),
            '\"' => out.push_str("\\\\\""),
            '\n' => out.push_str("\\\\n"),
            '\r' => out.push_str("\\\\r"),
            '\t' => out.push_str("\\\\t"),
            _ => out.push(ch),
        }
        used += 1;
    }

    out
}
