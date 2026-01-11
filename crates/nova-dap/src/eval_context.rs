#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvalContext {
    Watch,
    Hover,
    Repl,
    Other,
}

impl EvalContext {
    pub fn from_dap_context(context: Option<&str>) -> Self {
        let Some(context) = context else {
            return Self::Other;
        };

        if context.eq_ignore_ascii_case("watch") {
            Self::Watch
        } else if context.eq_ignore_ascii_case("hover") {
            Self::Hover
        } else if context.eq_ignore_ascii_case("repl") {
            Self::Repl
        } else {
            Self::Other
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvalOptions {
    pub allow_side_effects: bool,
}

impl EvalOptions {
    pub fn for_context(context: EvalContext) -> Self {
        let allow_side_effects = matches!(context, EvalContext::Repl);
        Self { allow_side_effects }
    }

    pub fn from_dap_context(context: Option<&str>) -> Self {
        Self::for_context(EvalContext::from_dap_context(context))
    }
}
