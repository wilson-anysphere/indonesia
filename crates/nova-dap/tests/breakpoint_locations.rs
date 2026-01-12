mod harness;

use harness::spawn_wire_server;
use serde_json::json;
use tempfile::tempdir;

#[tokio::test]
async fn breakpoint_locations_discovers_executable_statement_lines() {
    let (client, server_task) = spawn_wire_server();
    client.initialize_handshake().await;

    let dir = tempdir().expect("tempdir");
    let file_path = dir.path().join("Foo.java");
    std::fs::write(
        &file_path,
        r#"public class Foo {
  public static void main(String[] args) {
    // comment

    int x = 0;

    x++;
  }
}
"#,
    )
    .expect("write java file");

    let resp = client
        .request(
            "breakpointLocations",
            json!({
                "source": { "path": file_path.to_string_lossy() },
                "line": 1,
                "endLine": 50,
            }),
        )
        .await;

    assert_eq!(resp.get("success").and_then(|v| v.as_bool()), Some(true));

    let breakpoints = resp
        .pointer("/body/breakpoints")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("breakpointLocations response missing body.breakpoints: {resp}"));

    let lines: Vec<i64> = breakpoints
        .iter()
        .filter_map(|bp| bp.get("line").and_then(|v| v.as_i64()))
        .collect();
    assert_eq!(lines, vec![5, 7], "unexpected breakpoint locations: {resp}");

    client.disconnect().await;
    server_task.await.unwrap().unwrap();
}

