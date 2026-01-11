use nova_core::Line;
use nova_db::{FileId, InMemoryFileStore};
use nova_ide::semantics::{collect_breakpoint_sites, BreakpointSite};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBreakpoint {
    pub requested_line: Line,
    pub resolved_line: Line,
    pub verified: bool,
    pub enclosing_class: Option<String>,
    pub enclosing_method: Option<String>,
}

/// Map user-requested breakpoint lines to the closest executable statement start.
///
/// This function is pure and depends only on the `InMemoryFileStore` file text plus
/// Nova semantic helpers (`nova-ide`).
pub fn map_line_breakpoints(
    db: &InMemoryFileStore,
    file_id: FileId,
    requested_lines: &[Line],
) -> Vec<ResolvedBreakpoint> {
    let Some(text) = db.file_text(file_id) else {
        return requested_lines
            .iter()
            .map(|&line| ResolvedBreakpoint {
                requested_line: line,
                resolved_line: line,
                verified: false,
                enclosing_class: None,
                enclosing_method: None,
            })
            .collect();
    };

    let sites = collect_breakpoint_sites(text);
    requested_lines
        .iter()
        .map(|&requested_line| resolve_one(&sites, requested_line))
        .collect()
}

fn resolve_one(sites: &[BreakpointSite], requested_line: Line) -> ResolvedBreakpoint {
    if sites.is_empty() {
        return ResolvedBreakpoint {
            requested_line,
            resolved_line: requested_line,
            verified: false,
            enclosing_class: None,
            enclosing_method: None,
        };
    }

    let idx = sites.partition_point(|site| site.line < requested_line);

    let prev = idx.checked_sub(1).and_then(|i| sites.get(i));
    let next = sites.get(idx);

    let chosen = match (prev, next) {
        (Some(prev), Some(next)) => {
            let up = requested_line.saturating_sub(prev.line);
            let down = next.line.saturating_sub(requested_line);
            if down <= up {
                next
            } else {
                prev
            }
        }
        (Some(prev), None) => prev,
        (None, Some(next)) => next,
        (None, None) => unreachable!("sites is non-empty"),
    };

    ResolvedBreakpoint {
        requested_line,
        resolved_line: chosen.line,
        verified: true,
        enclosing_class: chosen.enclosing_class.clone(),
        enclosing_method: chosen.enclosing_method.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_blank_lines_to_next_statement_start() {
        let java = r#"
public class Foo {
  public void bar() {
    int x = 0;

    x++;
  }
}
"#;

        let mut db = InMemoryFileStore::new();
        let file_id = db.file_id_for_path("Foo.java");
        db.set_file_text(file_id, java.to_string());

        let resolved = map_line_breakpoints(&db, file_id, &[5]);
        assert_eq!(
            resolved,
            vec![ResolvedBreakpoint {
                requested_line: 5,
                resolved_line: 6,
                verified: true,
                enclosing_class: Some("Foo".to_string()),
                enclosing_method: Some("bar".to_string()),
            }]
        );
    }

    #[test]
    fn prefers_nearest_statement_start() {
        let java = r#"class C {
  void f() {
    int a = 1;
    int b = 2;
  }
}"#;

        let mut db = InMemoryFileStore::new();
        let file_id = db.file_id_for_path("C.java");
        db.set_file_text(file_id, java.to_string());

        // Line 4 is exactly a statement start.
        let resolved = map_line_breakpoints(&db, file_id, &[4]);
        assert_eq!(resolved[0].resolved_line, 4);
        assert!(resolved[0].verified);

        // Line 5 is not executable (closing brace); nearest is line 4.
        let resolved = map_line_breakpoints(&db, file_id, &[5]);
        assert_eq!(resolved[0].resolved_line, 4);
        assert!(resolved[0].verified);
    }
}
