//! Lowering support for `module-info.java`.

use nova_modules::{Exports, ModuleInfo, ModuleKind, ModuleName, Opens, Provides, Requires, Uses};
use nova_syntax::{
    parse_java, AstNode, CompilationUnit, ExportsDirective, ModuleDeclaration, OpensDirective,
    ProvidesDirective, RequiresDirective, UsesDirective,
};
use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ModuleInfoLowerError {
    #[error("{message} at {range:?}")]
    Parse {
        message: String,
        range: nova_syntax::TextRange,
    },
    #[error("invalid parse root (expected a compilation unit)")]
    InvalidRoot,
    #[error("module-info.java is missing a module declaration")]
    MissingModuleDeclaration,
    #[error("module-info.java module declaration is missing a name")]
    MissingModuleName,
}

impl From<nova_syntax::ParseError> for ModuleInfoLowerError {
    fn from(value: nova_syntax::ParseError) -> Self {
        ModuleInfoLowerError::Parse {
            message: value.message,
            range: value.range,
        }
    }
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
    let parse = parse_java(src);
    let root = parse.syntax();

    let mut errors: Vec<ModuleInfoLowerError> = parse
        .errors
        .into_iter()
        .map(ModuleInfoLowerError::from)
        .collect();

    let Some(unit) = CompilationUnit::cast(root) else {
        errors.push(ModuleInfoLowerError::InvalidRoot);
        return ModuleInfoLowerResult { info: None, errors };
    };

    let Some(decl) = unit.module_declaration() else {
        errors.push(ModuleInfoLowerError::MissingModuleDeclaration);
        return ModuleInfoLowerResult { info: None, errors };
    };

    let decl_name_text = decl.name().map(|name| name.text());
    if decl_name_text.as_deref().is_none_or(str::is_empty) {
        errors.push(ModuleInfoLowerError::MissingModuleName);
    }

    let info = Some(lower_module_decl(&decl));

    ModuleInfoLowerResult { info, errors }
}

/// Strict lowering wrapper that fails fast on malformed `module-info.java` sources.
pub fn lower_module_info_source_strict(src: &str) -> Result<ModuleInfo, ModuleInfoLowerError> {
    let parse = parse_java(src);
    let root = parse.syntax();

    let unit = CompilationUnit::cast(root).ok_or(ModuleInfoLowerError::InvalidRoot)?;
    let decl = unit
        .module_declaration()
        .ok_or(ModuleInfoLowerError::MissingModuleDeclaration)?;

    let decl_name_text = decl.name().map(|name| name.text());
    if decl_name_text.as_deref().is_none_or(str::is_empty) {
        return Err(ModuleInfoLowerError::MissingModuleName);
    }

    if let Some(err) = parse.errors.first() {
        return Err(ModuleInfoLowerError::from(err.clone()));
    }

    Ok(lower_module_decl(&decl))
}

pub fn lower_module_decl(decl: &ModuleDeclaration) -> ModuleInfo {
    let mut requires = Vec::new();
    let mut exports = Vec::new();
    let mut opens = Vec::new();
    let mut uses = Vec::new();
    let mut provides = Vec::new();

    if let Some(body) = decl.body() {
        for directive in body.directives() {
            match directive.kind() {
                nova_syntax::SyntaxKind::RequiresDirective => {
                    let Some(directive) = RequiresDirective::cast(directive) else {
                        continue;
                    };
                    let Some(module) = directive.module() else {
                        continue;
                    };
                    let module = module.text();
                    if module.is_empty() {
                        continue;
                    }
                    requires.push(Requires {
                        module: ModuleName::new(module),
                        is_transitive: directive.is_transitive(),
                        is_static: directive.is_static(),
                    });
                }
                nova_syntax::SyntaxKind::ExportsDirective => {
                    let Some(directive) = ExportsDirective::cast(directive) else {
                        continue;
                    };
                    let Some(package) = directive.package() else {
                        continue;
                    };
                    let package = package.text();
                    if package.is_empty() {
                        continue;
                    }
                    exports.push(Exports {
                        package,
                        to: directive
                            .to_modules()
                            .filter_map(|name| {
                                let text = name.text();
                                (!text.is_empty()).then_some(ModuleName::new(text))
                            })
                            .collect(),
                    });
                }
                nova_syntax::SyntaxKind::OpensDirective => {
                    let Some(directive) = OpensDirective::cast(directive) else {
                        continue;
                    };
                    let Some(package) = directive.package() else {
                        continue;
                    };
                    let package = package.text();
                    if package.is_empty() {
                        continue;
                    }
                    opens.push(Opens {
                        package,
                        to: directive
                            .to_modules()
                            .filter_map(|name| {
                                let text = name.text();
                                (!text.is_empty()).then_some(ModuleName::new(text))
                            })
                            .collect(),
                    });
                }
                nova_syntax::SyntaxKind::UsesDirective => {
                    let Some(directive) = UsesDirective::cast(directive) else {
                        continue;
                    };
                    let Some(service) = directive.service() else {
                        continue;
                    };
                    let service = service.text();
                    if service.is_empty() {
                        continue;
                    }
                    uses.push(Uses { service });
                }
                nova_syntax::SyntaxKind::ProvidesDirective => {
                    let Some(directive) = ProvidesDirective::cast(directive) else {
                        continue;
                    };
                    let Some(service) = directive.service() else {
                        continue;
                    };
                    let service = service.text();
                    if service.is_empty() {
                        continue;
                    }
                    provides.push(Provides {
                        service,
                        implementations: directive
                            .implementations()
                            .filter_map(|name| {
                                let text = name.text();
                                (!text.is_empty()).then_some(text)
                            })
                            .collect(),
                    });
                }
                _ => {}
            }
        }
    }

    ModuleInfo {
        kind: ModuleKind::Explicit,
        name: ModuleName::new(
            decl.name()
                .map(|name| name.text())
                .and_then(|text| (!text.is_empty()).then_some(text))
                .unwrap_or_else(|| "<missing>".to_string()),
        ),
        is_open: decl.is_open(),
        requires,
        exports,
        opens,
        uses,
        provides,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn best_effort_lowering_missing_name_is_panic_free() {
        let res = lower_module_info_source("module { }");
        assert!(
            res.errors
                .contains(&ModuleInfoLowerError::MissingModuleName),
            "expected MissingModuleName, got {:?}",
            res.errors
        );
        let info = res
            .info
            .expect("best-effort should still return ModuleInfo");
        assert_eq!(info.name.as_str(), "<missing>");
    }

    #[test]
    fn best_effort_lowering_reports_missing_module_declaration() {
        let res = lower_module_info_source("class A {}");
        assert!(
            res.errors
                .contains(&ModuleInfoLowerError::MissingModuleDeclaration),
            "expected MissingModuleDeclaration, got {:?}",
            res.errors
        );
        assert_eq!(res.info, None);
    }

    #[test]
    fn strict_lowering_missing_name_is_err() {
        assert!(lower_module_info_source_strict("module { }").is_err());
    }

    #[test]
    fn malformed_directive_does_not_panic() {
        let res = lower_module_info_source("module m { requires ; }");
        let info = res
            .info
            .expect("should lower module header even with broken directives");
        assert_eq!(info.name.as_str(), "m");
        assert!(info.requires.is_empty());
    }
}
