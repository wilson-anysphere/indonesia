//! Lowering support for `module-info.java`.

use nova_modules::{Exports, ModuleInfo, ModuleKind, ModuleName, Opens, Provides, Requires, Uses};
use nova_syntax::{
    parse_module_info, parse_module_info_lossy, ModuleDecl, ModuleDirective, ModuleInfoParseError,
};
use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ModuleInfoLowerError {
    #[error(transparent)]
    Parse(#[from] ModuleInfoParseError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleInfoLowerResult {
    pub info: Option<ModuleInfo>,
    pub errors: Vec<ModuleInfoLowerError>,
}

/// Lower a `module-info.java` source string into a best-effort [`ModuleInfo`].
///
/// This never fails: parse errors are returned alongside the lowered structure so IDE
/// features can continue operating on partially-correct code.
pub fn lower_module_info_source(src: &str) -> ModuleInfoLowerResult {
    let (decl, errors) = parse_module_info_lossy(src);
    ModuleInfoLowerResult {
        info: decl.as_ref().map(lower_module_decl),
        errors: errors.into_iter().map(ModuleInfoLowerError::from).collect(),
    }
}

/// Strict lowering wrapper that returns the first parse error.
pub fn lower_module_info_source_strict(src: &str) -> Result<ModuleInfo, ModuleInfoLowerError> {
    let decl = parse_module_info(src)?;
    Ok(lower_module_decl(&decl))
}

pub fn lower_module_decl(decl: &ModuleDecl) -> ModuleInfo {
    let mut requires = Vec::new();
    let mut exports = Vec::new();
    let mut opens = Vec::new();
    let mut uses = Vec::new();
    let mut provides = Vec::new();

    for directive in &decl.directives {
        match directive {
            ModuleDirective::Requires(r) => requires.push(Requires {
                module: ModuleName::new(r.module.as_str()),
                is_transitive: r.is_transitive,
                is_static: r.is_static,
            }),
            ModuleDirective::Exports(e) => exports.push(Exports {
                package: e.package.as_str().to_string(),
                to: e.to.iter().map(|m| ModuleName::new(m.as_str())).collect(),
            }),
            ModuleDirective::Opens(o) => opens.push(Opens {
                package: o.package.as_str().to_string(),
                to: o.to.iter().map(|m| ModuleName::new(m.as_str())).collect(),
            }),
            ModuleDirective::Uses(u) => uses.push(Uses {
                service: u.service.as_str().to_string(),
            }),
            ModuleDirective::Provides(p) => provides.push(Provides {
                service: p.service.as_str().to_string(),
                implementations: p
                    .implementations
                    .iter()
                    .map(|imp| imp.as_str().to_string())
                    .collect(),
            }),
        }
    }

    ModuleInfo {
        kind: ModuleKind::Explicit,
        name: ModuleName::new(decl.name.as_str()),
        is_open: decl.is_open,
        requires,
        exports,
        opens,
        uses,
        provides,
    }
}
