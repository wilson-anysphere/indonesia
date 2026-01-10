use nova_jdwp::{FrameId, JdwpClient, JdwpError, JdwpValue, StepKind, ThreadId};

use crate::dap::types::{EvaluateResult, OutputEvent, Scope, Variable, VariablePresentationHint};
use crate::error::{DebugError, DebugResult};
use crate::format::{FormattedValue, ValueFormatter};
use crate::object_registry::{ObjectHandle, ObjectRegistry, PINNED_SCOPE_REF};

pub struct DebugSession<C> {
    jdwp: C,
    formatter: ValueFormatter,
    objects: ObjectRegistry,
}

impl<C: JdwpClient> DebugSession<C> {
    pub fn new(jdwp: C) -> Self {
        Self {
            jdwp,
            formatter: ValueFormatter::default(),
            objects: ObjectRegistry::new(),
        }
    }

    /// Mutable access to the underlying JDWP connection. This is primarily
    /// useful in tests with [`nova_jdwp::MockJdwpClient`].
    pub fn jdwp_mut(&mut self) -> &mut C {
        &mut self.jdwp
    }

    pub fn format_value(&mut self, value: &JdwpValue) -> DebugResult<String> {
        Ok(self
            .formatter
            .format_value(&mut self.jdwp, &mut self.objects, value, None)?
            .value)
    }

    pub fn scopes(&self, locals_variables_reference: i64) -> Vec<Scope> {
        vec![
            Scope {
                name: "Locals".to_string(),
                variables_reference: locals_variables_reference,
                expensive: false,
                presentation_hint: None,
            },
            Scope {
                name: "Pinned Objects".to_string(),
                variables_reference: PINNED_SCOPE_REF,
                expensive: false,
                presentation_hint: Some("pinned".to_string()),
            },
        ]
    }

    pub fn variables(&mut self, variables_reference: i64) -> DebugResult<Vec<Variable>> {
        if variables_reference == PINNED_SCOPE_REF {
            return Ok(self.pinned_variables()?);
        }

        let handle = self
            .objects
            .handle_from_variables_reference(variables_reference)
            .ok_or(DebugError::UnknownVariablesReference(variables_reference))?;

        let object_id = self
            .objects
            .object_id(handle)
            .ok_or(DebugError::UnknownObjectHandle(variables_reference))?;

        match self.jdwp.object_children(object_id) {
            Ok(children) => children
                .into_iter()
                .map(|child| {
                    let name = child.name;
                    let evaluate_name = child.evaluate_name.clone().or_else(|| Some(name.clone()));
                    let formatted = self
                        .formatter
                        .format_value(
                            &mut self.jdwp,
                            &mut self.objects,
                            &child.value,
                            child.static_type.as_deref(),
                        )?;
                    Ok(Variable {
                        name,
                        value: formatted.value,
                        type_: formatted.type_name,
                        variables_reference: formatted.variables_reference,
                        evaluate_name,
                        presentation_hint: formatted.presentation_hint,
                    })
                })
                .collect(),
            Err(JdwpError::NotImplemented) => Ok(Vec::new()),
            Err(JdwpError::InvalidObjectId(_)) => {
                self.objects.mark_invalid_object_id(object_id);
                Ok(vec![Variable {
                    name: "<collected>".to_string(),
                    value: "<collected>".to_string(),
                    type_: None,
                    variables_reference: 0,
                    evaluate_name: None,
                    presentation_hint: Some(VariablePresentationHint {
                        kind: Some("virtual".to_string()),
                        attributes: Some(vec!["invalid".to_string()]),
                        visibility: None,
                        lazy: None,
                    }),
                }])
            }
            Err(err) => Err(DebugError::from(err)),
        }
    }

    pub fn evaluate(&mut self, frame_id: FrameId, expression: &str) -> DebugResult<EvaluateResult> {
        let value = match self.jdwp.evaluate(expression, frame_id) {
            Ok(value) => value,
            Err(JdwpError::NotImplemented) => {
                return Ok(EvaluateResult {
                    result: "Evaluation is not implemented yet".to_string(),
                    type_: None,
                    variables_reference: 0,
                    evaluate_name: Some(expression.to_string()),
                    presentation_hint: Some(VariablePresentationHint {
                        kind: Some("virtual".to_string()),
                        attributes: Some(vec!["invalid".to_string()]),
                        visibility: None,
                        lazy: None,
                    }),
                })
            }
            Err(err) => return Err(DebugError::from(err)),
        };
        let FormattedValue {
            value,
            type_name,
            variables_reference,
            presentation_hint,
        } = self
            .formatter
            .format_value(&mut self.jdwp, &mut self.objects, &value, None)?;

        Ok(EvaluateResult {
            result: value,
            type_: type_name,
            variables_reference,
            evaluate_name: Some(expression.to_string()),
            presentation_hint,
        })
    }

    pub fn pin_object(&mut self, handle: ObjectHandle) -> DebugResult<()> {
        let object_id = self
            .objects
            .object_id(handle)
            .ok_or(DebugError::UnknownObjectHandle(handle.as_variables_reference()))?;

        match self.jdwp.disable_collection(object_id) {
            Ok(()) | Err(JdwpError::NotImplemented) => {}
            Err(JdwpError::InvalidObjectId(_)) => self.objects.mark_invalid_object_id(object_id),
            Err(err) => return Err(DebugError::from(err)),
        };

        self.objects.pin(handle);
        Ok(())
    }

    pub fn unpin_object(&mut self, handle: ObjectHandle) -> DebugResult<()> {
        if !self.objects.is_pinned(handle) {
            return Ok(());
        }

        if let Some(object_id) = self.objects.object_id(handle) {
            match self.jdwp.enable_collection(object_id) {
                Ok(()) | Err(JdwpError::NotImplemented) => {}
                Err(JdwpError::InvalidObjectId(_)) => {
                    // Already collected; treat unpin as successful.
                    self.objects.mark_invalid_object_id(object_id)
                }
                Err(err) => return Err(DebugError::from(err)),
            };
        }

        self.objects.unpin(handle);
        Ok(())
    }

    pub fn step_over(&mut self, thread_id: ThreadId) -> DebugResult<StepOutput> {
        self.step(thread_id, StepKind::Over)
    }

    pub fn step_in(&mut self, thread_id: ThreadId) -> DebugResult<StepOutput> {
        self.step(thread_id, StepKind::Into)
    }

    pub fn step_out(&mut self, thread_id: ThreadId) -> DebugResult<StepOutput> {
        self.step(thread_id, StepKind::Out)
    }

    fn step(&mut self, thread_id: ThreadId, kind: StepKind) -> DebugResult<StepOutput> {
        let stopped = self.jdwp.step(thread_id, kind)?;
        let mut output = Vec::new();

        if let Some(return_value) = &stopped.return_value {
            let formatted =
                self.formatter
                    .format_value(&mut self.jdwp, &mut self.objects, return_value, None)?;
            output.push(OutputEvent {
                category: Some("console".to_string()),
                output: format!("Return value: {}\n", formatted.value),
            });
        }

        if let Some(expr_value) = &stopped.expression_value {
            let formatted =
                self.formatter
                    .format_value(&mut self.jdwp, &mut self.objects, expr_value, None)?;
            output.push(OutputEvent {
                category: Some("console".to_string()),
                output: format!("Expression value: {}\n", formatted.value),
            });
        }

        Ok(StepOutput { stopped, output })
    }

    fn pinned_variables(&mut self) -> DebugResult<Vec<Variable>> {
        let mut vars = Vec::new();

        let pinned: Vec<_> = self.objects.pinned_handles().collect();
        for handle in pinned {
            let object_id = match self.objects.object_id(handle) {
                Some(id) => id,
                None => continue,
            };

            let runtime_type = self
                .objects
                .runtime_type(handle)
                .unwrap_or("<object>")
                .to_string();

            let formatted = self.formatter.format_value(
                &mut self.jdwp,
                &mut self.objects,
                &JdwpValue::Object(nova_jdwp::ObjectRef {
                    id: object_id,
                    runtime_type,
                }),
                None,
            )?;

            vars.push(Variable {
                name: handle.to_string(),
                value: formatted.value,
                type_: formatted.type_name,
                variables_reference: formatted.variables_reference,
                evaluate_name: Some(format!("__novaPinned[{}]", handle.as_u32())),
                presentation_hint: Some(VariablePresentationHint {
                    kind: Some("data".to_string()),
                    attributes: Some(vec!["pinned".to_string()]),
                    visibility: None,
                    lazy: None,
                }),
            });
        }

        Ok(vars)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct StepOutput {
    pub stopped: nova_jdwp::StoppedEvent,
    pub output: Vec<OutputEvent>,
}
