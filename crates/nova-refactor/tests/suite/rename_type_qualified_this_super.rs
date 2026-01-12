use std::collections::BTreeMap;

use nova_refactor::{apply_workspace_edit, rename_type, FileId, RenameTypeParams};

#[test]
fn rename_type_updates_qualified_this_expression() {
    let file = FileId::new("Test.java");
    let src = r#"class Outer { class Inner { void m(){ Outer.this.toString(); } } }"#;

    let mut files = BTreeMap::new();
    files.insert(file.clone(), src.to_string());

    let offset = src.find("class Outer").unwrap() + "class ".len() + 1;
    let edit = rename_type(
        &files,
        RenameTypeParams {
            file: file.clone(),
            offset,
            new_name: "Renamed".into(),
        },
    )
    .unwrap();

    let after_files = apply_workspace_edit(&files, &edit).unwrap();
    let after = after_files.get(&file).unwrap();

    assert!(after.contains("class Renamed"));
    assert!(after.contains("Renamed.this.toString()"));
    assert!(!after.contains("Outer.this"));
}

#[test]
fn rename_type_updates_qualified_super_expression() {
    let file = FileId::new("Test.java");
    let src = r#"class Base {}
class Outer extends Base {
  class Inner {
    void m() { Outer.super.toString(); }
  }
}
"#;

    let mut files = BTreeMap::new();
    files.insert(file.clone(), src.to_string());

    let offset = src.find("class Outer").unwrap() + "class ".len() + 1;
    let edit = rename_type(
        &files,
        RenameTypeParams {
            file: file.clone(),
            offset,
            new_name: "Renamed".into(),
        },
    )
    .unwrap();

    let after_files = apply_workspace_edit(&files, &edit).unwrap();
    let after = after_files.get(&file).unwrap();

    assert!(after.contains("class Renamed extends Base"));
    assert!(after.contains("Renamed.super.toString()"));
    assert!(!after.contains("Outer.super"));
}

#[test]
fn rename_type_can_be_invoked_from_qualified_this_qualifier() {
    let file = FileId::new("Test.java");
    let src = r#"class Outer { class Inner { void m(){ Outer.this.toString(); } } }"#;

    let mut files = BTreeMap::new();
    files.insert(file.clone(), src.to_string());

    let offset = src.find("Outer.this").unwrap() + 1;
    let edit = rename_type(
        &files,
        RenameTypeParams {
            file: file.clone(),
            offset,
            new_name: "Renamed".into(),
        },
    )
    .unwrap();

    let after_files = apply_workspace_edit(&files, &edit).unwrap();
    let after = after_files.get(&file).unwrap();

    assert!(after.contains("class Renamed"));
    assert!(after.contains("Renamed.this.toString()"));
}

#[test]
fn rename_type_can_be_invoked_from_qualified_super_qualifier() {
    let file = FileId::new("Test.java");
    let src = r#"class Base {}
class Outer extends Base {
  class Inner {
    void m() { Outer.super.toString(); }
  }
}
"#;

    let mut files = BTreeMap::new();
    files.insert(file.clone(), src.to_string());

    let offset = src.find("Outer.super").unwrap() + 1;
    let edit = rename_type(
        &files,
        RenameTypeParams {
            file: file.clone(),
            offset,
            new_name: "Renamed".into(),
        },
    )
    .unwrap();

    let after_files = apply_workspace_edit(&files, &edit).unwrap();
    let after = after_files.get(&file).unwrap();

    assert!(after.contains("class Renamed extends Base"));
    assert!(after.contains("Renamed.super.toString()"));
}

#[test]
fn rename_type_updates_nested_qualified_this_outer_segment() {
    let file = FileId::new("Test.java");
    let src = r#"class Outer {
  class Inner {
    class Deep {
      void m(){ Outer.Inner.this.toString(); }
    }
  }
}
"#;

    let mut files = BTreeMap::new();
    files.insert(file.clone(), src.to_string());

    let offset = src.find("class Outer").unwrap() + "class ".len() + 1;
    let edit = rename_type(
        &files,
        RenameTypeParams {
            file: file.clone(),
            offset,
            new_name: "Renamed".into(),
        },
    )
    .unwrap();

    let after_files = apply_workspace_edit(&files, &edit).unwrap();
    let after = after_files.get(&file).unwrap();

    assert!(after.contains("class Renamed"));
    assert!(after.contains("Renamed.Inner.this.toString()"));
    assert!(!after.contains("Outer.Inner.this"));
}

#[test]
fn rename_type_can_be_invoked_from_nested_qualified_this_outer_segment() {
    let file = FileId::new("Test.java");
    let src = r#"class Outer {
  class Inner {
    class Deep {
      void m(){ Outer.Inner.this.toString(); }
    }
  }
}
"#;

    let mut files = BTreeMap::new();
    files.insert(file.clone(), src.to_string());

    let offset = src.find("Outer.Inner.this").unwrap() + 1; // on `Outer`
    let edit = rename_type(
        &files,
        RenameTypeParams {
            file: file.clone(),
            offset,
            new_name: "Renamed".into(),
        },
    )
    .unwrap();

    let after_files = apply_workspace_edit(&files, &edit).unwrap();
    let after = after_files.get(&file).unwrap();

    assert!(after.contains("class Renamed"));
    assert!(after.contains("Renamed.Inner.this.toString()"));
}

#[test]
fn rename_type_updates_nested_qualified_super_outer_segment() {
    let file = FileId::new("Test.java");
    let src = r#"class BaseInner {}
class Outer {
  class Inner extends BaseInner {
    class Deep {
      void m(){ Outer.Inner.super.toString(); }
    }
  }
}
"#;

    let mut files = BTreeMap::new();
    files.insert(file.clone(), src.to_string());

    let offset = src.find("class Outer").unwrap() + "class ".len() + 1;
    let edit = rename_type(
        &files,
        RenameTypeParams {
            file: file.clone(),
            offset,
            new_name: "Renamed".into(),
        },
    )
    .unwrap();

    let after_files = apply_workspace_edit(&files, &edit).unwrap();
    let after = after_files.get(&file).unwrap();

    assert!(after.contains("class Renamed"));
    assert!(after.contains("Renamed.Inner.super.toString()"));
    assert!(!after.contains("Outer.Inner.super"));
}

#[test]
fn rename_type_can_be_invoked_from_nested_qualified_super_outer_segment() {
    let file = FileId::new("Test.java");
    let src = r#"class BaseInner {}
class Outer {
  class Inner extends BaseInner {
    class Deep {
      void m(){ Outer.Inner.super.toString(); }
    }
  }
}
"#;

    let mut files = BTreeMap::new();
    files.insert(file.clone(), src.to_string());

    let offset = src.find("Outer.Inner.super").unwrap() + 1; // on `Outer`
    let edit = rename_type(
        &files,
        RenameTypeParams {
            file: file.clone(),
            offset,
            new_name: "Renamed".into(),
        },
    )
    .unwrap();

    let after_files = apply_workspace_edit(&files, &edit).unwrap();
    let after = after_files.get(&file).unwrap();

    assert!(after.contains("class Renamed"));
    assert!(after.contains("Renamed.Inner.super.toString()"));
}

#[test]
fn rename_type_updates_nested_qualified_this_inner_segment() {
    let file = FileId::new("Test.java");
    let src = r#"class Outer {
  class Inner {
    class Deep {
      void m(){ Outer.Inner.this.toString(); }
    }
  }
}
"#;

    let mut files = BTreeMap::new();
    files.insert(file.clone(), src.to_string());

    let offset = src.find("class Inner").unwrap() + "class ".len() + 1;
    let edit = rename_type(
        &files,
        RenameTypeParams {
            file: file.clone(),
            offset,
            new_name: "RenamedInner".into(),
        },
    )
    .unwrap();

    let after_files = apply_workspace_edit(&files, &edit).unwrap();
    let after = after_files.get(&file).unwrap();

    assert!(after.contains("class Outer"));
    assert!(after.contains("class RenamedInner"));
    assert!(after.contains("Outer.RenamedInner.this.toString()"));
    assert!(!after.contains("Outer.Inner.this"));
}

#[test]
fn rename_type_can_be_invoked_from_nested_qualified_this_inner_segment() {
    let file = FileId::new("Test.java");
    let src = r#"class Outer {
  class Inner {
    class Deep {
      void m(){ Outer.Inner.this.toString(); }
    }
  }
}
"#;

    let mut files = BTreeMap::new();
    files.insert(file.clone(), src.to_string());

    let start = src.find("Outer.Inner.this").unwrap();
    let offset = start + "Outer.".len() + 1; // on `Inner`
    let edit = rename_type(
        &files,
        RenameTypeParams {
            file: file.clone(),
            offset,
            new_name: "RenamedInner".into(),
        },
    )
    .unwrap();

    let after_files = apply_workspace_edit(&files, &edit).unwrap();
    let after = after_files.get(&file).unwrap();

    assert!(after.contains("class Outer"));
    assert!(after.contains("class RenamedInner"));
    assert!(after.contains("Outer.RenamedInner.this.toString()"));
}

#[test]
fn rename_type_updates_nested_qualified_super_inner_segment() {
    let file = FileId::new("Test.java");
    let src = r#"class BaseInner {}
class Outer {
  class Inner extends BaseInner {
    class Deep {
      void m(){ Outer.Inner.super.toString(); }
    }
  }
}
"#;

    let mut files = BTreeMap::new();
    files.insert(file.clone(), src.to_string());

    let offset = src.find("class Inner").unwrap() + "class ".len() + 1;
    let edit = rename_type(
        &files,
        RenameTypeParams {
            file: file.clone(),
            offset,
            new_name: "RenamedInner".into(),
        },
    )
    .unwrap();

    let after_files = apply_workspace_edit(&files, &edit).unwrap();
    let after = after_files.get(&file).unwrap();

    assert!(after.contains("class Outer"));
    assert!(after.contains("class RenamedInner extends BaseInner"));
    assert!(after.contains("Outer.RenamedInner.super.toString()"));
    assert!(!after.contains("Outer.Inner.super"));
}

#[test]
fn rename_type_can_be_invoked_from_nested_qualified_super_inner_segment() {
    let file = FileId::new("Test.java");
    let src = r#"class BaseInner {}
class Outer {
  class Inner extends BaseInner {
    class Deep {
      void m(){ Outer.Inner.super.toString(); }
    }
  }
}
"#;

    let mut files = BTreeMap::new();
    files.insert(file.clone(), src.to_string());

    let start = src.find("Outer.Inner.super").unwrap();
    let offset = start + "Outer.".len() + 1; // on `Inner`
    let edit = rename_type(
        &files,
        RenameTypeParams {
            file: file.clone(),
            offset,
            new_name: "RenamedInner".into(),
        },
    )
    .unwrap();

    let after_files = apply_workspace_edit(&files, &edit).unwrap();
    let after = after_files.get(&file).unwrap();

    assert!(after.contains("class Outer"));
    assert!(after.contains("class RenamedInner extends BaseInner"));
    assert!(after.contains("Outer.RenamedInner.super.toString()"));
}

#[test]
fn rename_type_updates_triple_nested_qualified_this_middle_segment() {
    let file = FileId::new("Test.java");
    let src = r#"class Outer {
  class Middle {
    class Inner {
      class Deep {
        void m(){ Outer.Middle.Inner.this.toString(); }
      }
    }
  }
}
"#;

    let mut files = BTreeMap::new();
    files.insert(file.clone(), src.to_string());

    let offset = src.find("class Middle").unwrap() + "class ".len() + 1;
    let edit = rename_type(
        &files,
        RenameTypeParams {
            file: file.clone(),
            offset,
            new_name: "RenamedMiddle".into(),
        },
    )
    .unwrap();

    let after_files = apply_workspace_edit(&files, &edit).unwrap();
    let after = after_files.get(&file).unwrap();

    assert!(after.contains("class RenamedMiddle"));
    assert!(after.contains("Outer.RenamedMiddle.Inner.this.toString()"));
    assert!(!after.contains("Outer.Middle.Inner.this"));
}

#[test]
fn rename_type_can_be_invoked_from_triple_nested_qualified_this_middle_segment() {
    let file = FileId::new("Test.java");
    let src = r#"class Outer {
  class Middle {
    class Inner {
      class Deep {
        void m(){ Outer.Middle.Inner.this.toString(); }
      }
    }
  }
}
"#;

    let mut files = BTreeMap::new();
    files.insert(file.clone(), src.to_string());

    let start = src.find("Outer.Middle.Inner.this").unwrap();
    let offset = start + "Outer.".len() + 1; // on `Middle`
    let edit = rename_type(
        &files,
        RenameTypeParams {
            file: file.clone(),
            offset,
            new_name: "RenamedMiddle".into(),
        },
    )
    .unwrap();

    let after_files = apply_workspace_edit(&files, &edit).unwrap();
    let after = after_files.get(&file).unwrap();

    assert!(after.contains("class RenamedMiddle"));
    assert!(after.contains("Outer.RenamedMiddle.Inner.this.toString()"));
}

#[test]
fn rename_type_updates_triple_nested_qualified_super_middle_segment() {
    let file = FileId::new("Test.java");
    let src = r#"class BaseInner {}
class Outer {
  class Middle {
    class Inner extends BaseInner {
      class Deep {
        void m(){ Outer.Middle.Inner.super.toString(); }
      }
    }
  }
}
"#;

    let mut files = BTreeMap::new();
    files.insert(file.clone(), src.to_string());

    let offset = src.find("class Middle").unwrap() + "class ".len() + 1;
    let edit = rename_type(
        &files,
        RenameTypeParams {
            file: file.clone(),
            offset,
            new_name: "RenamedMiddle".into(),
        },
    )
    .unwrap();

    let after_files = apply_workspace_edit(&files, &edit).unwrap();
    let after = after_files.get(&file).unwrap();

    assert!(after.contains("class RenamedMiddle"));
    assert!(after.contains("Outer.RenamedMiddle.Inner.super.toString()"));
    assert!(!after.contains("Outer.Middle.Inner.super"));
}

#[test]
fn rename_type_can_be_invoked_from_triple_nested_qualified_super_middle_segment() {
    let file = FileId::new("Test.java");
    let src = r#"class BaseInner {}
class Outer {
  class Middle {
    class Inner extends BaseInner {
      class Deep {
        void m(){ Outer.Middle.Inner.super.toString(); }
      }
    }
  }
}
"#;

    let mut files = BTreeMap::new();
    files.insert(file.clone(), src.to_string());

    let start = src.find("Outer.Middle.Inner.super").unwrap();
    let offset = start + "Outer.".len() + 1; // on `Middle`
    let edit = rename_type(
        &files,
        RenameTypeParams {
            file: file.clone(),
            offset,
            new_name: "RenamedMiddle".into(),
        },
    )
    .unwrap();

    let after_files = apply_workspace_edit(&files, &edit).unwrap();
    let after = after_files.get(&file).unwrap();

    assert!(after.contains("class RenamedMiddle"));
    assert!(after.contains("Outer.RenamedMiddle.Inner.super.toString()"));
}

#[test]
fn rename_type_updates_triple_nested_qualified_this_inner_segment() {
    let file = FileId::new("Test.java");
    let src = r#"class Outer {
  class Middle {
    class Inner {
      class Deep {
        void m(){ Outer.Middle.Inner.this.toString(); }
      }
    }
  }
}
"#;

    let mut files = BTreeMap::new();
    files.insert(file.clone(), src.to_string());

    let offset = src.find("class Inner").unwrap() + "class ".len() + 1;
    let edit = rename_type(
        &files,
        RenameTypeParams {
            file: file.clone(),
            offset,
            new_name: "RenamedInner".into(),
        },
    )
    .unwrap();

    let after_files = apply_workspace_edit(&files, &edit).unwrap();
    let after = after_files.get(&file).unwrap();

    assert!(after.contains("class RenamedInner"));
    assert!(after.contains("Outer.Middle.RenamedInner.this.toString()"));
    assert!(!after.contains("Outer.Middle.Inner.this"));
}

#[test]
fn rename_type_can_be_invoked_from_triple_nested_qualified_this_inner_segment() {
    let file = FileId::new("Test.java");
    let src = r#"class Outer {
  class Middle {
    class Inner {
      class Deep {
        void m(){ Outer.Middle.Inner.this.toString(); }
      }
    }
  }
}
"#;

    let mut files = BTreeMap::new();
    files.insert(file.clone(), src.to_string());

    let start = src.find("Outer.Middle.Inner.this").unwrap();
    let offset = start + "Outer.Middle.".len() + 1; // on `Inner`
    let edit = rename_type(
        &files,
        RenameTypeParams {
            file: file.clone(),
            offset,
            new_name: "RenamedInner".into(),
        },
    )
    .unwrap();

    let after_files = apply_workspace_edit(&files, &edit).unwrap();
    let after = after_files.get(&file).unwrap();

    assert!(after.contains("class RenamedInner"));
    assert!(after.contains("Outer.Middle.RenamedInner.this.toString()"));
}

#[test]
fn rename_type_updates_triple_nested_qualified_super_inner_segment() {
    let file = FileId::new("Test.java");
    let src = r#"class BaseInner {}
class Outer {
  class Middle {
    class Inner extends BaseInner {
      class Deep {
        void m(){ Outer.Middle.Inner.super.toString(); }
      }
    }
  }
}
"#;

    let mut files = BTreeMap::new();
    files.insert(file.clone(), src.to_string());

    let offset = src.find("class Inner").unwrap() + "class ".len() + 1;
    let edit = rename_type(
        &files,
        RenameTypeParams {
            file: file.clone(),
            offset,
            new_name: "RenamedInner".into(),
        },
    )
    .unwrap();

    let after_files = apply_workspace_edit(&files, &edit).unwrap();
    let after = after_files.get(&file).unwrap();

    assert!(after.contains("class RenamedInner extends BaseInner"));
    assert!(after.contains("Outer.Middle.RenamedInner.super.toString()"));
    assert!(!after.contains("Outer.Middle.Inner.super"));
}

#[test]
fn rename_type_can_be_invoked_from_triple_nested_qualified_super_inner_segment() {
    let file = FileId::new("Test.java");
    let src = r#"class BaseInner {}
class Outer {
  class Middle {
    class Inner extends BaseInner {
      class Deep {
        void m(){ Outer.Middle.Inner.super.toString(); }
      }
    }
  }
}
"#;

    let mut files = BTreeMap::new();
    files.insert(file.clone(), src.to_string());

    let start = src.find("Outer.Middle.Inner.super").unwrap();
    let offset = start + "Outer.Middle.".len() + 1; // on `Inner`
    let edit = rename_type(
        &files,
        RenameTypeParams {
            file: file.clone(),
            offset,
            new_name: "RenamedInner".into(),
        },
    )
    .unwrap();

    let after_files = apply_workspace_edit(&files, &edit).unwrap();
    let after = after_files.get(&file).unwrap();

    assert!(after.contains("class RenamedInner extends BaseInner"));
    assert!(after.contains("Outer.Middle.RenamedInner.super.toString()"));
}
