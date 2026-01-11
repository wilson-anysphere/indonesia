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

    let mut sites = collect_breakpoint_sites(text);
    // `collect_breakpoint_sites` is expected to return sites in source order, but
    // we defensively sort by line to keep `map_line_breakpoints` deterministic
    // even if collection changes (e.g. syntax-tree traversal order).
    sites.sort_by_key(|site| site.line);
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

    // `idx - 1` is the *last* site before `requested_line`, but if multiple sites
    // share that line we want the earliest one to keep breakpoint mapping stable.
    let prev = idx
        .checked_sub(1)
        .and_then(|i| sites.get(i))
        .map(|site| site.line)
        .and_then(|prev_line| {
            let first_on_line = sites.partition_point(|site| site.line < prev_line);
            sites.get(first_on_line)
        });
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
        enclosing_class: chosen
            .enclosing_class
            .as_deref()
            .map(|name| name.trim().to_string()),
        enclosing_method: chosen
            .enclosing_method
            .as_deref()
            .map(|name| name.trim().to_string()),
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
        assert_eq!(
            resolved,
            vec![ResolvedBreakpoint {
                requested_line: 4,
                resolved_line: 4,
                verified: true,
                enclosing_class: Some("C".to_string()),
                enclosing_method: Some("f".to_string()),
            }]
        );

        // Line 5 is not executable (closing brace); nearest is line 4.
        let resolved = map_line_breakpoints(&db, file_id, &[5]);
        assert_eq!(
            resolved,
            vec![ResolvedBreakpoint {
                requested_line: 5,
                resolved_line: 4,
                verified: true,
                enclosing_class: Some("C".to_string()),
                enclosing_method: Some("f".to_string()),
            }]
        );
    }

    #[test]
    fn maps_constructor_body_to_init_method() {
        let java = r#"package pkg;
public class C {
  C() {
    int x = 0;
  }
}
"#;

        let mut db = InMemoryFileStore::new();
        let file_id = db.file_id_for_path("C.java");
        db.set_file_text(file_id, java.to_string());

        let resolved = map_line_breakpoints(&db, file_id, &[4]);
        assert_eq!(
            resolved,
            vec![ResolvedBreakpoint {
                requested_line: 4,
                resolved_line: 4,
                verified: true,
                enclosing_class: Some("pkg.C".to_string()),
                enclosing_method: Some("<init>".to_string()),
            }]
        );
    }

    #[test]
    fn maps_static_initializer_block_to_clinit_method() {
        let java = r#"package pkg;
class C {
  static {
    int x = 0;
  }
}
"#;

        let mut db = InMemoryFileStore::new();
        let file_id = db.file_id_for_path("C.java");
        db.set_file_text(file_id, java.to_string());

        let resolved = map_line_breakpoints(&db, file_id, &[4]);
        assert_eq!(
            resolved,
            vec![ResolvedBreakpoint {
                requested_line: 4,
                resolved_line: 4,
                verified: true,
                enclosing_class: Some("pkg.C".to_string()),
                enclosing_method: Some("<clinit>".to_string()),
            }]
        );
    }

    #[test]
    fn maps_nested_member_class_method_to_dollar_class_name() {
        let java = r#"package pkg;
class Outer {
  class Inner {
    void m() {
      int x = 0;
    }
  }
}
"#;

        let mut db = InMemoryFileStore::new();
        let file_id = db.file_id_for_path("Outer.java");
        db.set_file_text(file_id, java.to_string());

        let resolved = map_line_breakpoints(&db, file_id, &[5]);
        assert_eq!(
            resolved,
            vec![ResolvedBreakpoint {
                requested_line: 5,
                resolved_line: 5,
                verified: true,
                enclosing_class: Some("pkg.Outer$Inner".to_string()),
                enclosing_method: Some("m".to_string()),
            }]
        );
    }

    #[test]
    fn lambda_breakpoints_do_not_filter_by_enclosing_method() {
        let java = r#"package pkg;
class C {
  void f() {
    Runnable r = () -> {
      int x = 0;
    };
  }
}
"#;

        let mut db = InMemoryFileStore::new();
        let file_id = db.file_id_for_path("C.java");
        db.set_file_text(file_id, java.to_string());

        let resolved = map_line_breakpoints(&db, file_id, &[5]);
        assert_eq!(
            resolved,
            vec![ResolvedBreakpoint {
                requested_line: 5,
                resolved_line: 5,
                verified: true,
                enclosing_class: Some("pkg.C".to_string()),
                enclosing_method: None,
            }]
        );
    }

    #[test]
    fn maps_field_initializers_to_init_and_clinit() {
        let java = r#"package pkg;
class C {
  static int X = 1;
  int y = 2;
}
"#;

        let mut db = InMemoryFileStore::new();
        let file_id = db.file_id_for_path("C.java");
        db.set_file_text(file_id, java.to_string());

        let resolved = map_line_breakpoints(&db, file_id, &[3, 4]);
        assert_eq!(
            resolved,
            vec![
                ResolvedBreakpoint {
                    requested_line: 3,
                    resolved_line: 3,
                    verified: true,
                    enclosing_class: Some("pkg.C".to_string()),
                    enclosing_method: Some("<clinit>".to_string()),
                },
                ResolvedBreakpoint {
                    requested_line: 4,
                    resolved_line: 4,
                    verified: true,
                    enclosing_class: Some("pkg.C".to_string()),
                    enclosing_method: Some("<init>".to_string()),
                }
            ]
        );
    }

    #[test]
    fn chooses_earliest_site_on_a_line_when_mapping_to_prev_line() {
        let java = r#"package pkg;
class C {
  static int X = 1; int y = 2;

  void f() {
    int z = 3;
  }
}
"#;

        let mut db = InMemoryFileStore::new();
        let file_id = db.file_id_for_path("C.java");
        db.set_file_text(file_id, java.to_string());

        // Line 4 is blank; mapping should pick the nearest executable line (line 3).
        // When multiple breakpoint sites exist on that line, the mapping must
        // consistently pick the earliest one.
        let resolved = map_line_breakpoints(&db, file_id, &[4]);
        assert_eq!(
            resolved,
            vec![ResolvedBreakpoint {
                requested_line: 4,
                resolved_line: 3,
                verified: true,
                enclosing_class: Some("pkg.C".to_string()),
                enclosing_method: Some("<clinit>".to_string()),
            }]
        );
    }

    #[test]
    fn chooses_earliest_site_on_a_line_when_multiple_sites_share_the_requested_line() {
        let java = r#"package pkg;
class C {
  static int X = 1; int y = 2;
}
"#;

        let mut db = InMemoryFileStore::new();
        let file_id = db.file_id_for_path("C.java");
        db.set_file_text(file_id, java.to_string());

        // Both field initializers live on line 3; the mapper should
        // deterministically pick the earliest site on that line.
        let resolved = map_line_breakpoints(&db, file_id, &[3]);
        assert_eq!(
            resolved,
            vec![ResolvedBreakpoint {
                requested_line: 3,
                resolved_line: 3,
                verified: true,
                enclosing_class: Some("pkg.C".to_string()),
                enclosing_method: Some("<clinit>".to_string()),
            }]
        );
    }
}
