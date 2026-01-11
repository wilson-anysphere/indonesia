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
    #[error("module-info.java is missing a module declaration")]
    MissingModuleDeclaration,
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
    let unit = CompilationUnit::cast(parse.syntax()).expect("root node is a compilation unit");
    let info = unit.module_declaration().as_ref().map(lower_module_decl);

    ModuleInfoLowerResult {
        info,
        errors: parse
            .errors
            .into_iter()
            .map(ModuleInfoLowerError::from)
            .collect(),
    }
}

/// Strict lowering wrapper that returns the first parse error.
pub fn lower_module_info_source_strict(src: &str) -> Result<ModuleInfo, ModuleInfoLowerError> {
    let parse = parse_java(src);
    if let Some(err) = parse.errors.first() {
        return Err(ModuleInfoLowerError::from(err.clone()));
    }

    let unit = CompilationUnit::cast(parse.syntax()).expect("root node is a compilation unit");
    let decl = unit
        .module_declaration()
        .ok_or(ModuleInfoLowerError::MissingModuleDeclaration)?;

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
                    let directive =
                        RequiresDirective::cast(directive).expect("requires directive kind");
                    let Some(module) = directive.module() else {
                        continue;
                    };
                    requires.push(Requires {
                        module: ModuleName::new(module.text()),
                        is_transitive: directive.is_transitive(),
                        is_static: directive.is_static(),
                    });
                }
                nova_syntax::SyntaxKind::ExportsDirective => {
                    let directive =
                        ExportsDirective::cast(directive).expect("exports directive kind");
                    let Some(package) = directive.package() else {
                        continue;
                    };
                    exports.push(Exports {
                        package: package.text(),
                        to: directive
                            .to_modules()
                            .map(|name| ModuleName::new(name.text()))
                            .collect(),
                    });
                }
                nova_syntax::SyntaxKind::OpensDirective => {
                    let directive = OpensDirective::cast(directive).expect("opens directive kind");
                    let Some(package) = directive.package() else {
                        continue;
                    };
                    opens.push(Opens {
                        package: package.text(),
                        to: directive
                            .to_modules()
                            .map(|name| ModuleName::new(name.text()))
                            .collect(),
                    });
                }
                nova_syntax::SyntaxKind::UsesDirective => {
                    let directive = UsesDirective::cast(directive).expect("uses directive kind");
                    let Some(service) = directive.service() else {
                        continue;
                    };
                    uses.push(Uses {
                        service: service.text(),
                    });
                }
                nova_syntax::SyntaxKind::ProvidesDirective => {
                    let directive =
                        ProvidesDirective::cast(directive).expect("provides directive kind");
                    let Some(service) = directive.service() else {
                        continue;
                    };
                    provides.push(Provides {
                        service: service.text(),
                        implementations: directive
                            .implementations()
                            .map(|name| name.text())
                            .collect(),
                    });
                }
                _ => {}
            }
        }
    }

    ModuleInfo {
        kind: ModuleKind::Explicit,
        name: ModuleName::new(decl.name().expect("module declarations have a name").text()),
        is_open: decl.is_open(),
        requires,
        exports,
        opens,
        uses,
        provides,
    }
}
