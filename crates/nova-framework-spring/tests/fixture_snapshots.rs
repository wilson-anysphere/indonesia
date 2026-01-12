use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use insta::assert_snapshot;

use nova_config_metadata::MetadataIndex;
use nova_framework_spring::{
    analyze_java_sources, completions_for_properties_file, completions_for_value_placeholder,
    diagnostics_for_config_file, SpringWorkspaceIndex,
};

mod suite;

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/basic")
}

fn collect_files(root: &Path) -> Vec<PathBuf> {
    fn visit(dir: &Path, out: &mut Vec<PathBuf>) {
        let mut entries: Vec<_> = fs::read_dir(dir)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", dir.display()))
            .collect();
        entries.sort_by_key(|e| e.as_ref().ok().map(|e| e.file_name()));

        for entry in entries {
            let entry = entry.expect("read_dir entry");
            let path = entry.path();
            if path.is_dir() {
                visit(&path, out);
            } else {
                out.push(path);
            }
        }
    }

    let mut out = Vec::new();
    visit(root, &mut out);
    out.sort();
    out
}

fn load_java_sources(root: &Path) -> (Vec<PathBuf>, Vec<String>) {
    let mut paths = Vec::new();
    let mut sources = Vec::new();

    for path in collect_files(root) {
        if path.extension().and_then(|e| e.to_str()) != Some("java") {
            continue;
        }
        let rel = path.strip_prefix(root).unwrap().to_path_buf();
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
        paths.push(rel);
        sources.push(text);
    }

    (paths, sources)
}

fn load_file(root: &Path, rel: &str) -> String {
    let path = root.join(rel);
    fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()))
}

#[test]
fn spring_fixture_di_snapshot() {
    let root = fixture_root();
    let (paths, sources) = load_java_sources(&root);
    let source_refs: Vec<&str> = sources.iter().map(String::as_str).collect();
    let analysis = analyze_java_sources(&source_refs);

    let mut out = String::new();

    out.push_str("Beans:\n");
    let mut bean_indices: Vec<usize> = (0..analysis.model.beans.len()).collect();
    bean_indices.sort_by_key(|&idx| {
        let bean = &analysis.model.beans[idx];
        (
            paths
                .get(bean.location.source)
                .map(PathBuf::as_path)
                .unwrap_or_else(|| Path::new("<unknown>"))
                .to_path_buf(),
            bean.location.span.start,
            bean.name.clone(),
        )
    });
    for idx in bean_indices {
        let bean = &analysis.model.beans[idx];
        let file = paths
            .get(bean.location.source)
            .map(PathBuf::as_path)
            .unwrap_or_else(|| Path::new("<unknown>"));
        out.push_str(&format!("- {}: {} ({:?})", bean.name, bean.ty, bean.kind));
        if bean.primary {
            out.push_str(" primary");
        }
        if !bean.qualifiers.is_empty() {
            out.push_str(&format!(" qualifiers={:?}", bean.qualifiers));
        }
        if !bean.profiles.is_empty() {
            out.push_str(&format!(" profiles={:?}", bean.profiles));
        }
        if bean.conditional {
            out.push_str(" conditional");
        }
        out.push_str(&format!(" @ {}\n", file.display()));
    }

    out.push_str("\nInjections:\n");
    let mut injection_indices: Vec<usize> = (0..analysis.model.injections.len()).collect();
    injection_indices.sort_by_key(|&idx| {
        let inj = &analysis.model.injections[idx];
        (
            paths
                .get(inj.location.source)
                .map(PathBuf::as_path)
                .unwrap_or_else(|| Path::new("<unknown>"))
                .to_path_buf(),
            inj.location.span.start,
            inj.owner_class.clone(),
            inj.name.clone(),
        )
    });
    for inj_idx in injection_indices {
        let inj = &analysis.model.injections[inj_idx];
        let file = paths
            .get(inj.location.source)
            .map(PathBuf::as_path)
            .unwrap_or_else(|| Path::new("<unknown>"));
        let candidates = analysis.model.injection_candidates[inj_idx]
            .iter()
            .filter_map(|&bean_idx| analysis.model.beans.get(bean_idx).map(|b| b.name.clone()))
            .collect::<Vec<_>>();
        out.push_str(&format!(
            "- {}.{}: {} qualifier={:?} -> {:?} @ {}\n",
            inj.owner_class,
            inj.name,
            inj.ty,
            inj.qualifier,
            candidates,
            file.display()
        ));
    }

    out.push_str("\nDiagnostics:\n");
    let mut diags = analysis.diagnostics.clone();
    diags.sort_by_key(|d| {
        (
            paths
                .get(d.source)
                .map(PathBuf::as_path)
                .unwrap_or_else(|| Path::new("<unknown>"))
                .to_path_buf(),
            d.diagnostic.span.map(|s| s.start).unwrap_or(0),
            d.diagnostic.code.to_string(),
        )
    });
    for diag in diags {
        let file = paths
            .get(diag.source)
            .map(PathBuf::as_path)
            .unwrap_or_else(|| Path::new("<unknown>"));
        out.push_str(&format!(
            "- {}: {} {:?} {}\n",
            file.display(),
            diag.diagnostic.code,
            diag.diagnostic.severity,
            diag.diagnostic.message
        ));
    }

    assert_snapshot!(
        out,
        @r###"
Beans:
- application: Application (Component) @ src/main/java/com/example/app/Application.java
- consumer: Consumer (Component) @ src/main/java/com/example/app/consumer/Consumer.java
- fooImpl1: FooImpl1 (Component) @ src/main/java/com/example/app/foo/FooImpl1.java
- fooImpl2: FooImpl2 (Component) @ src/main/java/com/example/app/foo/FooImpl2.java
- englishGreetingService: EnglishGreetingService (Component) primary @ src/main/java/com/example/app/service/EnglishGreetingService.java
- spanishGreetingService: SpanishGreetingService (Component) qualifiers=["spanish"] @ src/main/java/com/example/app/service/SpanishGreetingService.java

Injections:
- Consumer.foo: Foo qualifier=None -> ["fooImpl1", "fooImpl2"] @ src/main/java/com/example/app/consumer/Consumer.java
- Consumer.otherService: OtherService qualifier=None -> [] @ src/main/java/com/example/app/consumer/Consumer.java
- Consumer.greetingService: GreetingService qualifier=None -> ["englishGreetingService"] @ src/main/java/com/example/app/consumer/Consumer.java
- Consumer.spanish: GreetingService qualifier=Some("spanish") -> ["spanishGreetingService"] @ src/main/java/com/example/app/consumer/Consumer.java

Diagnostics:
- src/main/java/com/example/app/consumer/Consumer.java: SPRING_AMBIGUOUS_BEAN Error Multiple Spring beans of type `Foo` found (fooImpl1, fooImpl2); mark one @Primary or use @Qualifier to disambiguate
- src/main/java/com/example/app/consumer/Consumer.java: SPRING_NO_BEAN Error No Spring bean of type `OtherService` found for injection
"###
    );
}

#[test]
fn spring_fixture_config_completions_and_diagnostics_snapshot() {
    let root = fixture_root();
    let metadata_json = load_file(&root, "spring-configuration-metadata.json");
    let mut metadata = MetadataIndex::new();
    metadata
        .ingest_json_bytes(metadata_json.as_bytes())
        .expect("metadata ingest");

    let config = load_file(&root, "src/main/resources/application.properties");
    let consumer = load_file(
        &root,
        "src/main/java/com/example/app/consumer/Consumer.java",
    );

    let mut workspace = SpringWorkspaceIndex::new(Arc::new(metadata));
    workspace.add_config_file("src/main/resources/application.properties", &config);
    workspace.add_java_file(
        "src/main/java/com/example/app/consumer/Consumer.java",
        &consumer,
    );

    let diags = diagnostics_for_config_file(
        Path::new("src/main/resources/application.properties"),
        &config,
        workspace.metadata(),
    );

    let mut diag_lines: Vec<String> = diags
        .iter()
        .map(|d| format!("{} {:?} {}", d.code, d.severity, d.message))
        .collect();
    diag_lines.sort();

    assert_snapshot!(
        diag_lines.join("\n"),
        @r###"
SPRING_DEPRECATED_CONFIG_KEY Warning Deprecated Spring configuration key 'spring.main.banner-mode'; use 'spring.main.banner-mode2'
SPRING_UNKNOWN_CONFIG_KEY Warning Unknown Spring configuration key 'unknown.key'
"###
    );

    let offset = consumer.find("${server.p}").expect("placeholder") + "${server.p".len();
    let mut items = completions_for_value_placeholder(&consumer, offset, &workspace);
    items.sort_by(|a, b| a.label.cmp(&b.label));

    let rendered = items
        .iter()
        .map(|i| {
            let detail = i.detail.as_deref().unwrap_or("");
            format!("{} | {}", i.label, detail)
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert_snapshot!(
        rendered,
        @r###"
server.port | java.lang.Integer (default: 8080) — Server HTTP port
"###
    );

    let text = "spr";
    let offset = text.len();
    let mut items = completions_for_properties_file(
        Path::new("src/main/resources/application.properties"),
        text,
        offset,
        &workspace,
    );
    items.sort_by(|a, b| a.label.cmp(&b.label));

    let rendered = items
        .iter()
        .map(|i| {
            let detail = i.detail.as_deref().unwrap_or("");
            format!("{} | {}", i.label, detail)
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert_snapshot!(
        rendered,
        @r###"
spring.main.banner-mode | java.lang.String [deprecated → spring.main.banner-mode2] — Banner mode
"###
    );
}
