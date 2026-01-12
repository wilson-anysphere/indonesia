use lsp_types::{
    ReferenceContext, ReferenceParams, TextDocumentIdentifier, TextDocumentPositionParams,
};

use nova_test_utils::Fixture;

#[test]
fn references_returns_locations_for_identifier() {
    let fixture = Fixture::parse(
        r#"
//- /Main.java
class Main {
    void test() {
        int $1foo = 1;
        $0foo++;
    }
}
"#,
    );

    let params = ReferenceParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: fixture.marker_uri(0),
            },
            position: fixture.marker_position(0),
        },
        context: ReferenceContext {
            include_declaration: true,
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };

    let locations = nova_lsp::handlers::references(&fixture.db, params).expect("some references");

    assert_eq!(locations.len(), 2);
    assert!(locations.iter().any(|loc| {
        loc.uri == fixture.marker_uri(0) && loc.range.start == fixture.marker_position(0)
    }));
    assert!(locations.iter().any(|loc| {
        loc.uri == fixture.marker_uri(1) && loc.range.start == fixture.marker_position(1)
    }));
}
