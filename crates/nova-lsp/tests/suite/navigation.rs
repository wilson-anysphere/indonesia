use lsp_types::request::{
    GotoDeclarationParams, GotoImplementationParams, GotoTypeDefinitionParams,
};
use lsp_types::{TextDocumentIdentifier, TextDocumentPositionParams};

use nova_test_utils::Fixture;

#[test]
fn go_to_implementation_on_interface_method_returns_implementing_method() {
    let fixture = Fixture::parse(
        r#"
//- /I.java
interface I {
    void $0foo();
}
//- /C.java
class C implements I {
    public void $1foo() {}
}
"#,
    );

    let params = GotoImplementationParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: fixture.marker_uri(0),
            },
            position: fixture.marker_position(0),
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };

    let resp = nova_lsp::handlers::implementation(&fixture.db, params).unwrap();
    let got = match resp {
        lsp_types::GotoDefinitionResponse::Array(arr) => arr,
        _ => panic!("expected array response"),
    };
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].uri, fixture.marker_uri(1));
    assert_eq!(got[0].range.start, fixture.marker_position(1));
}

#[test]
fn go_to_type_definition_on_variable_returns_class() {
    let fixture = Fixture::parse(
        r#"
//- /Foo.java
class $1Foo {}
//- /Main.java
class Main {
    void test() {
        Foo foo = new Foo();
        $0foo.toString();
    }
}
"#,
    );

    let params = GotoTypeDefinitionParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: fixture.marker_uri(0),
            },
            position: fixture.marker_position(0),
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };

    let resp = nova_lsp::handlers::type_definition(&fixture.db, params).unwrap();
    let got = match resp {
        lsp_types::GotoDefinitionResponse::Scalar(loc) => loc,
        _ => panic!("expected scalar response"),
    };
    assert_eq!(got.uri, fixture.marker_uri(1));
    assert_eq!(got.range.start, fixture.marker_position(1));
}

#[test]
fn go_to_declaration_on_override_returns_interface_declaration() {
    let fixture = Fixture::parse(
        r#"
//- /I.java
interface I {
    void $1foo();
}
//- /C.java
class C implements I {
    public void $0foo() {}
}
"#,
    );

    let params = GotoDeclarationParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: fixture.marker_uri(0),
            },
            position: fixture.marker_position(0),
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };

    let resp = nova_lsp::handlers::declaration(&fixture.db, params).unwrap();
    let got = match resp {
        lsp_types::GotoDefinitionResponse::Scalar(loc) => loc,
        _ => panic!("expected scalar response"),
    };
    assert_eq!(got.uri, fixture.marker_uri(1));
    assert_eq!(got.range.start, fixture.marker_position(1));
}
