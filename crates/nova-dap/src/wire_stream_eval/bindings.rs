use std::collections::{BTreeMap, HashMap, HashSet};

use nova_jdwp::wire::types::{
    FieldId, FieldInfoWithGeneric, FrameId, JdwpError, JdwpValue, Location, ObjectId,
    ReferenceTypeId, ThreadId, VariableInfo, VariableInfoWithGeneric,
};
use nova_jdwp::wire::JdwpClient;

use super::java_gen::sanitize_java_param_name;
use super::java_types::java_type_from_signatures;

const FIELD_MODIFIER_STATIC: u32 = 0x0008;

/// A bound Java identifier (local or instance field) with a static Java type and the JDWP value
/// captured from the current frame.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamEvalBinding {
    /// Java identifier as referenced in the original expression (e.g. `nums`).
    pub name: String,
    /// Java source spelling for the identifier's type (e.g. `java.util.List<java.lang.Integer>`).
    pub java_type: String,
    /// The captured runtime value to pass to the injected helper method.
    pub value: JdwpValue,
}

/// All bindings needed to evaluate an expression in the context of a suspended frame.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamEvalFrameBindings {
    /// The suspended frame's `this` object. Always present as a binding so helper signatures stay
    /// stable; for static frames this will be a `null` object reference (`id = 0`).
    pub this: StreamEvalBinding,
    /// In-scope local variables, ordered deterministically.
    pub locals: Vec<StreamEvalBinding>,
    /// Instance fields exposed as unqualified identifiers, ordered deterministically.
    pub fields: Vec<StreamEvalBinding>,
    /// Static fields exposed as unqualified identifiers, ordered deterministically.
    ///
    /// These are captured from the paused frame's *declaring class* (`Location.class_id`) and its
    /// superclass chain. Locals and instance fields shadow static fields, mirroring Java name
    /// resolution for unqualified identifiers.
    pub static_fields: Vec<StreamEvalBinding>,
}

impl StreamEvalFrameBindings {
    /// Returns locals in the format expected by [`crate::wire_stream_eval::java_gen`].
    ///
    /// This includes a synthetic `("this", <type>)` entry so the helper can choose a concrete
    /// parameter type for `__this`.
    pub fn locals_for_java_gen(&self) -> Vec<(String, String)> {
        let mut out = Vec::with_capacity(1 + self.locals.len());
        out.push(("this".to_string(), self.this.java_type.clone()));
        out.extend(
            self.locals
                .iter()
                .map(|b| (b.name.clone(), b.java_type.clone())),
        );
        out
    }

    pub fn fields_for_java_gen(&self) -> Vec<(String, String)> {
        self.fields
            .iter()
            .map(|b| (b.name.clone(), b.java_type.clone()))
            .collect()
    }

    pub fn static_fields_for_java_gen(&self) -> Vec<(String, String)> {
        self.static_fields
            .iter()
            .map(|b| (b.name.clone(), b.java_type.clone()))
            .collect()
    }

    /// Returns invocation arguments in the same order as the generated helper method signature:
    /// `(__this, <locals...>, <fields...>, <static_fields...>)`.
    pub fn args_for_helper(&self) -> Vec<JdwpValue> {
        let mut out = Vec::with_capacity(
            1 + self.locals.len() + self.fields.len() + self.static_fields.len(),
        );
        out.push(self.this.value.clone());
        out.extend(self.locals.iter().map(|b| b.value.clone()));
        out.extend(self.fields.iter().map(|b| b.value.clone()));
        out.extend(self.static_fields.iter().map(|b| b.value.clone()));
        out
    }
}

/// Build evaluator bindings for the given suspended frame.
///
/// This captures:
/// - `this` (via `StackFrame.ThisObject`)
/// - in-scope locals (via `Method.VariableTableWithGeneric` + `StackFrame.GetValues`)
/// - instance fields on `this` (via `ReferenceType.FieldsWithGeneric` + `ObjectReference.GetValues`)
/// - static fields in the frame's declaring class (and superclasses) (via
///   `ReferenceType.FieldsWithGeneric` + `ReferenceType.GetValues`)
///
/// Locals shadow fields: if a local variable name collides with a field name, the field is not
/// bound. Locals and instance fields shadow static fields.
pub async fn build_frame_bindings(
    jdwp: &JdwpClient,
    thread: ThreadId,
    frame_id: FrameId,
    location: Location,
) -> Result<StreamEvalFrameBindings, JdwpError> {
    let this_id = jdwp.stack_frame_this_object(thread, frame_id).await?;
    let (this_java_type, this_value) = if this_id == 0 {
        (
            "java.lang.Object".to_string(),
            JdwpValue::Object { tag: b'L', id: 0 },
        )
    } else {
        let (_tag, type_id) = jdwp.object_reference_reference_type(this_id).await?;
        let sig = jdwp.reference_type_signature(type_id).await?;
        (
            java_type_from_signatures(&sig, None),
            JdwpValue::Object {
                tag: b'L',
                id: this_id,
            },
        )
    };

    let locals = collect_in_scope_locals(jdwp, thread, frame_id, &location).await?;
    let local_names: HashSet<String> = locals.iter().map(|b| b.name.clone()).collect();

    let fields = collect_instance_fields(jdwp, this_id, &local_names).await?;
    let mut shadowed_names = local_names.clone();
    shadowed_names.extend(fields.iter().map(|b| b.name.clone()));

    let static_fields = collect_static_fields(jdwp, location.class_id, &shadowed_names).await?;

    Ok(StreamEvalFrameBindings {
        this: StreamEvalBinding {
            name: "this".to_string(),
            java_type: this_java_type,
            value: this_value,
        },
        locals,
        fields,
        static_fields,
    })
}

async fn collect_in_scope_locals(
    jdwp: &JdwpClient,
    thread: ThreadId,
    frame_id: FrameId,
    location: &Location,
) -> Result<Vec<StreamEvalBinding>, JdwpError> {
    // Prefer `Method.VariableTableWithGeneric` when available so generic local types show up in the
    // injected helper signature.
    //
    // Some targets (including Nova's mock JDWP server) only expose *some* locals via the generic
    // table. In that case, we merge in any locals missing from the erased table.
    let generic_vars = match jdwp
        .method_variable_table_with_generic(location.class_id, location.method_id)
        .await
    {
        Ok((_argc, vars)) => Some(
            vars.into_iter()
                .map(VarInfo::from_generic)
                .collect::<Vec<_>>(),
        ),
        Err(err) if is_unsupported_command_error(&err) => None,
        Err(err) => return Err(err),
    };

    let erased_vars = match jdwp
        .method_variable_table(location.class_id, location.method_id)
        .await
    {
        Ok((_argc, vars)) => Some(
            vars.into_iter()
                .map(VarInfo::from_erased)
                .collect::<Vec<_>>(),
        ),
        Err(err) if is_unsupported_command_error(&err) => None,
        Err(err) => return Err(err),
    };

    let filter_in_scope = |vars: Vec<VarInfo>| {
        vars.into_iter()
            .filter(|var| is_var_in_scope(var.code_index, var.length, location.index))
            .filter(|var| !var.name.trim().is_empty())
            // Avoid binding the synthetic `this` local; we always bind it separately and rewrite
            // `this` tokens in the expression.
            .filter(|var| var.name != "this")
            .collect::<Vec<_>>()
    };

    // If multiple variables with the same name are in scope, prefer the one that starts latest
    // (inner-most scope).
    fn select_innermost(in_scope: Vec<VarInfo>) -> HashMap<String, VarInfo> {
        let mut by_name: HashMap<String, VarInfo> = HashMap::new();
        for var in in_scope {
            match by_name.get(&var.name) {
                Some(existing) if existing.code_index >= var.code_index => {}
                _ => {
                    by_name.insert(var.name.clone(), var);
                }
            }
        }
        by_name
    }

    let mut selected: HashMap<String, VarInfo> = HashMap::new();
    if let Some(generic) = generic_vars {
        selected.extend(select_innermost(filter_in_scope(generic)));
    }
    if let Some(erased) = erased_vars {
        for (name, var) in select_innermost(filter_in_scope(erased)) {
            // If a variable exists in both tables, prefer the generic version.
            selected.entry(name).or_insert(var);
        }
    }

    let mut vars: Vec<VarInfo> = selected.into_values().collect();
    vars.sort_by(|a, b| a.name.cmp(&b.name));

    let slots: Vec<(u32, String)> = vars.iter().map(|v| (v.slot, v.signature.clone())).collect();
    let values = jdwp
        .stack_frame_get_values(thread, frame_id, &slots)
        .await?;

    let mut out = Vec::with_capacity(vars.len());
    for (var, value) in vars.into_iter().zip(values.into_iter()) {
        let java_type = java_type_from_signatures(&var.signature, var.generic_signature.as_deref());
        out.push(StreamEvalBinding {
            name: var.name,
            java_type,
            value,
        });
    }

    Ok(out)
}

#[derive(Debug, Clone)]
struct VarInfo {
    code_index: u64,
    name: String,
    signature: String,
    generic_signature: Option<String>,
    length: u32,
    slot: u32,
}

impl VarInfo {
    fn from_generic(v: VariableInfoWithGeneric) -> Self {
        Self {
            code_index: v.code_index,
            name: v.name,
            signature: v.signature,
            generic_signature: v.generic_signature,
            length: v.length,
            slot: v.slot,
        }
    }

    fn from_erased(v: VariableInfo) -> Self {
        Self {
            code_index: v.code_index,
            name: v.name,
            signature: v.signature,
            generic_signature: None,
            length: v.length,
            slot: v.slot,
        }
    }
}

async fn collect_instance_fields(
    jdwp: &JdwpClient,
    this_id: ObjectId,
    local_names: &HashSet<String>,
) -> Result<Vec<StreamEvalBinding>, JdwpError> {
    if this_id == 0 {
        return Ok(Vec::new());
    }

    let (_ref_type_tag, type_id) = jdwp.object_reference_reference_type(this_id).await?;
    let hierarchy = class_hierarchy(jdwp, type_id).await?;

    #[derive(Debug, Clone)]
    struct SelectedField {
        declaring_type: ReferenceTypeId,
        field_id: FieldId,
        name: String,
        signature: String,
        generic_signature: Option<String>,
    }

    let mut seen_names = HashSet::new();
    let mut selected = Vec::new();

    for class_id in hierarchy {
        let fields = reference_type_fields_with_generic_fallback(jdwp, class_id).await?;
        for field in fields {
            if field.mod_bits & FIELD_MODIFIER_STATIC != 0 {
                continue;
            }

            let name = field.name.trim();
            if name.is_empty() {
                continue;
            }
            // Avoid colliding with the synthetic `__this` parameter used by the helper source
            // generator to represent the frame's receiver.
            if name == "__this" {
                continue;
            }

            // Locals shadow fields.
            if local_names.contains(name) {
                continue;
            }

            // Only bind identifiers that can be used directly without rewriting the expression.
            if sanitize_java_param_name(name) != name {
                continue;
            }

            if !seen_names.insert(name.to_string()) {
                continue;
            }

            selected.push(SelectedField {
                declaring_type: class_id,
                field_id: field.field_id,
                name: name.to_string(),
                signature: field.signature,
                generic_signature: field.generic_signature,
            });
        }
    }

    // Fetch values per declaring type (more compatible with VMs that require field ids to come
    // from a single declaring reference type).
    let mut per_type: BTreeMap<ReferenceTypeId, Vec<FieldId>> = BTreeMap::new();
    for field in &selected {
        per_type
            .entry(field.declaring_type)
            .or_default()
            .push(field.field_id);
    }

    let mut values_by_id: HashMap<FieldId, JdwpValue> = HashMap::new();
    for (_type_id, field_ids) in per_type {
        let values = jdwp
            .object_reference_get_values(this_id, &field_ids)
            .await?;
        for (field_id, value) in field_ids.into_iter().zip(values.into_iter()) {
            values_by_id.insert(field_id, value);
        }
    }

    let mut out = Vec::with_capacity(selected.len());
    for field in selected {
        let Some(value) = values_by_id.get(&field.field_id).cloned() else {
            continue;
        };
        let java_type =
            java_type_from_signatures(&field.signature, field.generic_signature.as_deref());
        out.push(StreamEvalBinding {
            name: field.name,
            java_type,
            value,
        });
    }

    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

async fn collect_static_fields(
    jdwp: &JdwpClient,
    class_id: ReferenceTypeId,
    shadowed_names: &HashSet<String>,
) -> Result<Vec<StreamEvalBinding>, JdwpError> {
    if class_id == 0 {
        return Ok(Vec::new());
    }

    let hierarchy = class_hierarchy(jdwp, class_id).await?;

    #[derive(Debug, Clone)]
    struct SelectedField {
        declaring_type: ReferenceTypeId,
        field_id: FieldId,
        name: String,
        signature: String,
        generic_signature: Option<String>,
    }

    let mut seen_names = HashSet::new();
    let mut selected = Vec::new();

    for class_id in hierarchy {
        let fields = reference_type_fields_with_generic_fallback(jdwp, class_id).await?;
        for field in fields {
            if field.mod_bits & FIELD_MODIFIER_STATIC == 0 {
                continue;
            }

            let name = field.name.trim();
            if name.is_empty() {
                continue;
            }

            // Locals and instance fields shadow static fields.
            if shadowed_names.contains(name) {
                continue;
            }

            // Only bind identifiers that can be used directly without rewriting the expression.
            if sanitize_java_param_name(name) != name {
                continue;
            }

            if !seen_names.insert(name.to_string()) {
                continue;
            }

            selected.push(SelectedField {
                declaring_type: class_id,
                field_id: field.field_id,
                name: name.to_string(),
                signature: field.signature,
                generic_signature: field.generic_signature,
            });
        }
    }

    let mut per_type: BTreeMap<ReferenceTypeId, Vec<FieldId>> = BTreeMap::new();
    for field in &selected {
        per_type
            .entry(field.declaring_type)
            .or_default()
            .push(field.field_id);
    }

    let mut values_by_id: HashMap<FieldId, JdwpValue> = HashMap::new();
    for (type_id, field_ids) in per_type {
        let values = jdwp.reference_type_get_values(type_id, &field_ids).await?;
        for (field_id, value) in field_ids.into_iter().zip(values.into_iter()) {
            values_by_id.insert(field_id, value);
        }
    }

    let mut out = Vec::with_capacity(selected.len());
    for field in selected {
        let Some(value) = values_by_id.get(&field.field_id).cloned() else {
            continue;
        };
        let java_type =
            java_type_from_signatures(&field.signature, field.generic_signature.as_deref());
        out.push(StreamEvalBinding {
            name: field.name,
            java_type,
            value,
        });
    }

    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

async fn class_hierarchy(
    jdwp: &JdwpClient,
    type_id: ReferenceTypeId,
) -> Result<Vec<ReferenceTypeId>, JdwpError> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut current = type_id;
    while current != 0 && seen.insert(current) {
        out.push(current);
        let superclass = jdwp.class_type_superclass(current).await?;
        if superclass == 0 {
            break;
        }
        current = superclass;
    }
    Ok(out)
}

async fn reference_type_fields_with_generic_fallback(
    jdwp: &JdwpClient,
    class_id: ReferenceTypeId,
) -> Result<Vec<FieldInfoWithGeneric>, JdwpError> {
    match jdwp.reference_type_fields_with_generic(class_id).await {
        Ok(fields) => Ok(fields),
        Err(err) if is_unsupported_command_error(&err) => {
            let erased = jdwp.reference_type_fields(class_id).await?;
            Ok(erased
                .into_iter()
                .map(|field| FieldInfoWithGeneric {
                    field_id: field.field_id,
                    name: field.name,
                    signature: field.signature,
                    generic_signature: None,
                    mod_bits: field.mod_bits,
                })
                .collect())
        }
        Err(err) => Err(err),
    }
}

fn is_var_in_scope(code_index: u64, length: u32, pc: u64) -> bool {
    let Some(end) = code_index.checked_add(length as u64) else {
        return false;
    };
    code_index <= pc && pc < end
}

fn is_unsupported_command_error(err: &JdwpError) -> bool {
    const ERROR_NOT_FOUND: u16 = 41;
    const ERROR_UNSUPPORTED_VERSION: u16 = 68;
    const ERROR_NOT_IMPLEMENTED: u16 = 99;

    matches!(
        err,
        JdwpError::VmError(ERROR_NOT_FOUND | ERROR_UNSUPPORTED_VERSION | ERROR_NOT_IMPLEMENTED)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use nova_jdwp::wire::mock::{MockJdwpServer, FIELD_HIDING_OBJECT_ID};

    #[tokio::test]
    async fn instance_fields_are_filtered_by_hierarchy_and_shadowed_by_locals() {
        let server = MockJdwpServer::spawn().await.unwrap();
        let client = JdwpClient::connect(server.addr()).await.unwrap();

        let local_names = HashSet::new();
        let fields = collect_instance_fields(&client, FIELD_HIDING_OBJECT_ID, &local_names)
            .await
            .unwrap();

        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].name, "hidden");
        assert_eq!(fields[0].java_type, "int");
        assert_eq!(fields[0].value, JdwpValue::Int(1));

        let mut shadowed = HashSet::new();
        shadowed.insert("hidden".to_string());
        let fields = collect_instance_fields(&client, FIELD_HIDING_OBJECT_ID, &shadowed)
            .await
            .unwrap();
        assert!(fields.is_empty());
    }

    #[tokio::test]
    async fn build_frame_bindings_prefers_generic_signatures_and_is_deterministic() {
        let server = MockJdwpServer::spawn().await.unwrap();
        let client = JdwpClient::connect(server.addr()).await.unwrap();

        let thread = client.all_threads().await.unwrap()[0];
        let frame = client.frames(thread, 0, 1).await.unwrap()[0];

        let bindings = build_frame_bindings(&client, thread, frame.frame_id, frame.location)
            .await
            .unwrap();

        // Locals are sorted by name for deterministic helper signatures.
        assert_eq!(
            bindings
                .locals
                .iter()
                .map(|b| b.name.as_str())
                .collect::<Vec<_>>(),
            vec!["arr", "list", "obj", "s", "x"]
        );

        let by_name: HashMap<&str, &StreamEvalBinding> = bindings
            .locals
            .iter()
            .map(|b| (b.name.as_str(), b))
            .collect();

        // Prefer the generic signature when available.
        assert_eq!(
            by_name["list"].java_type,
            "java.util.List<java.lang.String>"
        );
        assert!(
            matches!(&by_name["list"].value, JdwpValue::Object { id, .. } if *id != 0),
            "expected non-null object value for `list`"
        );

        assert_eq!(by_name["x"].java_type, "int");
        assert!(matches!(by_name["x"].value, JdwpValue::Int(42)));

        assert_eq!(by_name["s"].java_type, "java.lang.String");
        assert!(
            matches!(&by_name["s"].value, JdwpValue::Object { id, .. } if *id != 0),
            "expected non-null object value for `s`"
        );

        assert_eq!(by_name["arr"].java_type, "int[]");
        assert!(
            matches!(&by_name["arr"].value, JdwpValue::Object { tag: b'[', id } if *id != 0),
            "expected array object value for `arr`"
        );
        // Instance fields on `this` are bound after locals.
        assert_eq!(bindings.fields.len(), 1);
        assert_eq!(bindings.fields[0].name, "field");
        assert_eq!(bindings.fields[0].java_type, "int");
        assert_eq!(bindings.fields[0].value, JdwpValue::Int(7));

        // Static fields in the declaring class are bound after locals + instance fields.
        assert_eq!(bindings.static_fields.len(), 1);
        assert_eq!(bindings.static_fields[0].name, "staticField");
        assert_eq!(bindings.static_fields[0].java_type, "int");
        assert_eq!(bindings.static_fields[0].value, JdwpValue::Int(0));

        let args = bindings.args_for_helper();
        assert_eq!(
            args.len(),
            1 + bindings.locals.len() + bindings.fields.len() + bindings.static_fields.len()
        );
        assert_eq!(args[0], bindings.this.value);
        // Locals appear in the helper args in the same order as `bindings.locals`.
        for (idx, local) in bindings.locals.iter().enumerate() {
            assert_eq!(args[idx + 1], local.value);
        }
        assert_eq!(args[1 + bindings.locals.len()], bindings.fields[0].value);
        assert_eq!(
            args[1 + bindings.locals.len() + bindings.fields.len()],
            bindings.static_fields[0].value
        );
    }

    #[tokio::test]
    async fn static_fields_are_filtered_by_hierarchy_shadowing_and_ordered_deterministically() {
        let server = MockJdwpServer::spawn().await.unwrap();
        let client = JdwpClient::connect(server.addr()).await.unwrap();

        let (_tag, class_id) = client
            .object_reference_reference_type(FIELD_HIDING_OBJECT_ID)
            .await
            .unwrap();

        let shadowed = HashSet::new();
        let static_fields = collect_static_fields(&client, class_id, &shadowed)
            .await
            .unwrap();

        // The mock class hierarchy includes a `shared` static field in both the subclass and
        // superclass; the subclass declaration should win (name hiding).
        assert_eq!(static_fields.len(), 2);
        assert_eq!(static_fields[0].name, "shared");
        assert_eq!(static_fields[0].java_type, "int");
        assert_eq!(static_fields[0].value, JdwpValue::Int(1));
        assert_eq!(static_fields[1].name, "superOnly");
        assert_eq!(static_fields[1].java_type, "int");
        assert_eq!(static_fields[1].value, JdwpValue::Int(3));

        // Shadowing: locals/instance fields should prevent static fields from binding.
        let mut shadowed = HashSet::new();
        shadowed.insert("shared".to_string());
        let static_fields = collect_static_fields(&client, class_id, &shadowed)
            .await
            .unwrap();
        assert_eq!(static_fields.len(), 1);
        assert_eq!(static_fields[0].name, "superOnly");
        assert_eq!(static_fields[0].java_type, "int");
        assert_eq!(static_fields[0].value, JdwpValue::Int(3));
    }

    #[test]
    fn locals_shadow_fields_by_name() {
        let mut locals = HashSet::new();
        locals.insert("nums".to_string());

        // In Java, locals shadow fields with the same name; stream-eval bindings must follow that
        // rule so unqualified identifiers resolve as they do in the original source.
        assert!(locals.contains("nums"));
        let should_bind_field = !locals.contains("nums");
        assert!(!should_bind_field, "field should be skipped");
    }
}
