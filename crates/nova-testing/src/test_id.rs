/// Parsed representation of a Nova test identifier.
///
/// In single-module workspaces, Nova historically used plain test IDs:
/// - `com.example.TestClass`
/// - `com.example.TestClass#testMethod`
///
/// In multi-module workspaces, Nova uses a module-qualified format to avoid
/// ID collisions across modules:
/// - `{moduleRelPath}::{fqcn}[#{method}]`
///
/// `moduleRelPath` is the module root relative to the workspace root, using `/`
/// separators. The workspace root itself is encoded as `"."`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualifiedTestId {
    pub module: Option<String>,
    pub test: String,
}

/// Parse a potentially module-qualified Nova test ID.
///
/// Accepts:
/// - qualified: `{moduleRelPath}::{testId}`
/// - legacy: `{testId}`
pub fn parse_qualified_test_id(id: &str) -> QualifiedTestId {
    let Some((module, test)) = id.split_once("::") else {
        return QualifiedTestId {
            module: None,
            test: id.to_string(),
        };
    };

    if module.trim().is_empty() {
        return QualifiedTestId {
            module: None,
            test: test.to_string(),
        };
    }

    if test.trim().is_empty() {
        return QualifiedTestId {
            module: None,
            test: id.to_string(),
        };
    }

    QualifiedTestId {
        module: Some(module.to_string()),
        test: test.to_string(),
    }
}

pub(crate) fn qualify_test_id(module_rel_path: &str, test_id: &str) -> String {
    let module = if module_rel_path.trim().is_empty() {
        "."
    } else {
        module_rel_path
    };
    format!("{module}::{test_id}")
}
