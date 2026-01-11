use nova_build::{
    collect_gradle_build_files, collect_maven_build_files, parse_gradle_classpath_output,
    parse_gradle_annotation_processing_output, parse_gradle_projects_output, parse_javac_diagnostics,
    parse_maven_classpath_output, parse_maven_effective_pom_annotation_processing,
    parse_maven_evaluate_scalar_output, BuildFileFingerprint, GradleProjectInfo, JavaCompileConfig,
};
use nova_core::{DiagnosticSeverity, Position, Range};
use std::path::{Path, PathBuf};

#[test]
fn fingerprint_changes_on_pom_edit() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(&root).unwrap();

    let pom = root.join("pom.xml");
    std::fs::write(
        &pom,
        "<project><modelVersion>4.0.0</modelVersion></project>",
    )
    .unwrap();

    let fp1 = BuildFileFingerprint::from_files(&root, vec![pom.clone()]).unwrap();
    std::fs::write(
        &pom,
        "<project><modelVersion>4.0.0</modelVersion><!--x--></project>",
    )
    .unwrap();
    let fp2 = BuildFileFingerprint::from_files(&root, vec![pom]).unwrap();

    assert_ne!(fp1.digest, fp2.digest);
}

#[test]
fn fingerprint_changes_on_maven_wrapper_edit() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(&root).unwrap();

    std::fs::write(
        root.join("pom.xml"),
        "<project><modelVersion>4.0.0</modelVersion></project>",
    )
    .unwrap();

    let wrapper_dir = root.join(".mvn").join("wrapper");
    std::fs::create_dir_all(&wrapper_dir).unwrap();
    let wrapper_props = wrapper_dir.join("maven-wrapper.properties");
    std::fs::write(
        &wrapper_props,
        "distributionUrl=https://example.invalid/a.zip\n",
    )
    .unwrap();

    let fp1 =
        BuildFileFingerprint::from_files(&root, collect_maven_build_files(&root).unwrap()).unwrap();
    std::fs::write(
        &wrapper_props,
        "distributionUrl=https://example.invalid/b.zip\n",
    )
    .unwrap();
    let fp2 =
        BuildFileFingerprint::from_files(&root, collect_maven_build_files(&root).unwrap()).unwrap();

    assert_ne!(fp1.digest, fp2.digest);
}

#[test]
fn fingerprint_changes_on_maven_maven_config_edit() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(&root).unwrap();

    std::fs::write(
        root.join("pom.xml"),
        "<project><modelVersion>4.0.0</modelVersion></project>",
    )
    .unwrap();

    let mvn_dir = root.join(".mvn");
    std::fs::create_dir_all(&mvn_dir).unwrap();
    let config = mvn_dir.join("maven.config");
    std::fs::write(&config, "-DskipTests\n").unwrap();

    let fp1 =
        BuildFileFingerprint::from_files(&root, collect_maven_build_files(&root).unwrap()).unwrap();
    std::fs::write(&config, "-DskipTests -Dstyle.color=always\n").unwrap();
    let fp2 =
        BuildFileFingerprint::from_files(&root, collect_maven_build_files(&root).unwrap()).unwrap();

    assert_ne!(fp1.digest, fp2.digest);
}

#[test]
fn fingerprint_changes_on_maven_extensions_xml_edit() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(&root).unwrap();

    std::fs::write(
        root.join("pom.xml"),
        "<project><modelVersion>4.0.0</modelVersion></project>",
    )
    .unwrap();

    let mvn_dir = root.join(".mvn");
    std::fs::create_dir_all(&mvn_dir).unwrap();
    let extensions = mvn_dir.join("extensions.xml");
    std::fs::write(&extensions, "<extensions></extensions>\n").unwrap();

    let fp1 =
        BuildFileFingerprint::from_files(&root, collect_maven_build_files(&root).unwrap()).unwrap();
    std::fs::write(&extensions, "<extensions><!--changed--></extensions>\n").unwrap();
    let fp2 =
        BuildFileFingerprint::from_files(&root, collect_maven_build_files(&root).unwrap()).unwrap();

    assert_ne!(fp1.digest, fp2.digest);
}

#[test]
fn fingerprint_changes_on_maven_wrapper_script_edit() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(&root).unwrap();

    std::fs::write(
        root.join("pom.xml"),
        "<project><modelVersion>4.0.0</modelVersion></project>",
    )
    .unwrap();

    let mvnw = root.join("mvnw");
    std::fs::write(&mvnw, "#!/bin/sh\necho mvnw\n").unwrap();

    let fp1 =
        BuildFileFingerprint::from_files(&root, collect_maven_build_files(&root).unwrap()).unwrap();
    std::fs::write(&mvnw, "#!/bin/sh\necho mvnw changed\n").unwrap();
    let fp2 =
        BuildFileFingerprint::from_files(&root, collect_maven_build_files(&root).unwrap()).unwrap();

    assert_ne!(fp1.digest, fp2.digest);
}

#[test]
fn fingerprint_ignores_misplaced_maven_wrapper_properties() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(&root).unwrap();

    std::fs::write(
        root.join("pom.xml"),
        "<project><modelVersion>4.0.0</modelVersion></project>",
    )
    .unwrap();

    // Only `.mvn/wrapper/maven-wrapper.properties` should affect the fingerprint.
    let misplaced = root.join("maven-wrapper.properties");
    std::fs::write(
        &misplaced,
        "distributionUrl=https://example.invalid/a.zip\n",
    )
    .unwrap();

    let fp1 =
        BuildFileFingerprint::from_files(&root, collect_maven_build_files(&root).unwrap()).unwrap();
    std::fs::write(
        &misplaced,
        "distributionUrl=https://example.invalid/b.zip\n",
    )
    .unwrap();
    let fp2 =
        BuildFileFingerprint::from_files(&root, collect_maven_build_files(&root).unwrap()).unwrap();

    assert_eq!(fp1.digest, fp2.digest);
}

#[test]
fn fingerprint_ignores_misplaced_maven_config_and_extensions_files() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(&root).unwrap();

    std::fs::write(
        root.join("pom.xml"),
        "<project><modelVersion>4.0.0</modelVersion></project>",
    )
    .unwrap();

    let misplaced_config = root.join("maven.config");
    std::fs::write(&misplaced_config, "-DskipTests\n").unwrap();
    let misplaced_extensions = root.join("extensions.xml");
    std::fs::write(&misplaced_extensions, "<extensions></extensions>\n").unwrap();

    let fp1 =
        BuildFileFingerprint::from_files(&root, collect_maven_build_files(&root).unwrap()).unwrap();

    std::fs::write(&misplaced_config, "-DskipTests -Dfoo=bar\n").unwrap();
    std::fs::write(
        &misplaced_extensions,
        "<extensions><!--changed--></extensions>\n",
    )
    .unwrap();

    let fp2 =
        BuildFileFingerprint::from_files(&root, collect_maven_build_files(&root).unwrap()).unwrap();

    assert_eq!(fp1.digest, fp2.digest);
}

#[test]
fn fingerprint_changes_on_gradle_wrapper_edit() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(&root).unwrap();

    std::fs::write(root.join("build.gradle"), "plugins { id 'java' }\n").unwrap();

    let wrapper_dir = root.join("gradle").join("wrapper");
    std::fs::create_dir_all(&wrapper_dir).unwrap();
    let wrapper_props = wrapper_dir.join("gradle-wrapper.properties");
    std::fs::write(
        &wrapper_props,
        "distributionUrl=https\\://services.gradle.org/distributions/gradle-8.0-bin.zip\n",
    )
    .unwrap();

    let fp1 = BuildFileFingerprint::from_files(&root, collect_gradle_build_files(&root).unwrap())
        .unwrap();
    std::fs::write(
        &wrapper_props,
        "distributionUrl=https\\://services.gradle.org/distributions/gradle-8.1-bin.zip\n",
    )
    .unwrap();
    let fp2 = BuildFileFingerprint::from_files(&root, collect_gradle_build_files(&root).unwrap())
        .unwrap();

    assert_ne!(fp1.digest, fp2.digest);
}

#[test]
fn fingerprint_changes_on_gradle_wrapper_script_edit() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(&root).unwrap();

    std::fs::write(root.join("build.gradle"), "plugins { id 'java' }\n").unwrap();

    let gradlew = root.join("gradlew");
    std::fs::write(&gradlew, "#!/bin/sh\necho gradlew\n").unwrap();

    let fp1 = BuildFileFingerprint::from_files(&root, collect_gradle_build_files(&root).unwrap())
        .unwrap();
    std::fs::write(&gradlew, "#!/bin/sh\necho gradlew changed\n").unwrap();
    let fp2 = BuildFileFingerprint::from_files(&root, collect_gradle_build_files(&root).unwrap())
        .unwrap();

    assert_ne!(fp1.digest, fp2.digest);
}

#[test]
fn fingerprint_ignores_misplaced_gradle_wrapper_properties() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(&root).unwrap();

    std::fs::write(root.join("build.gradle"), "plugins { id 'java' }\n").unwrap();

    // Only `gradle/wrapper/gradle-wrapper.properties` should affect the fingerprint.
    let misplaced = root.join("gradle-wrapper.properties");
    std::fs::write(&misplaced, "distributionUrl=foo\n").unwrap();

    let fp1 = BuildFileFingerprint::from_files(&root, collect_gradle_build_files(&root).unwrap())
        .unwrap();
    std::fs::write(&misplaced, "distributionUrl=bar\n").unwrap();
    let fp2 = BuildFileFingerprint::from_files(&root, collect_gradle_build_files(&root).unwrap())
        .unwrap();

    assert_eq!(fp1.digest, fp2.digest);
}

#[test]
fn fingerprint_changes_on_build_gradle_prefixed_file_edit() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(&root).unwrap();

    std::fs::write(root.join("build.gradle"), "plugins { id 'java' }\n").unwrap();

    let extra = root.join("build.gradle.custom");
    std::fs::write(&extra, "ext.foo = 1\n").unwrap();

    let fp1 = BuildFileFingerprint::from_files(&root, collect_gradle_build_files(&root).unwrap())
        .unwrap();
    std::fs::write(&extra, "ext.foo = 2\n").unwrap();
    let fp2 = BuildFileFingerprint::from_files(&root, collect_gradle_build_files(&root).unwrap())
        .unwrap();

    assert_ne!(fp1.digest, fp2.digest);
}

#[test]
fn parses_maven_classpath_bracket_list() {
    let out = r#"[/a/b/c.jar, /d/e/f.jar]"#;
    let cp = parse_maven_classpath_output(out);
    assert_eq!(
        cp,
        vec![PathBuf::from("/a/b/c.jar"), PathBuf::from("/d/e/f.jar")]
    );
}

#[test]
fn parses_maven_classpath_path_separator_list() {
    let out = std::env::join_paths([PathBuf::from("/a/b/c.jar"), PathBuf::from("/d/e/f.jar")])
        .expect("join paths")
        .to_string_lossy()
        .to_string();
    let cp = parse_maven_classpath_output(&out);
    assert_eq!(
        cp,
        vec![PathBuf::from("/a/b/c.jar"), PathBuf::from("/d/e/f.jar")]
    );
}

#[test]
fn parses_maven_classpath_newline_list() {
    let out = r#"
/a/b/c.jar
/d/e/f.jar
"#;
    let cp = parse_maven_classpath_output(out);
    assert_eq!(
        cp,
        vec![PathBuf::from("/a/b/c.jar"), PathBuf::from("/d/e/f.jar")]
    );
}

#[test]
fn parses_maven_classpath_with_noise_and_bracket_list_line() {
    let out = r#"
[INFO] Scanning for projects...
[WARNING] Some warning
[/a/b/c.jar, /d/e/f.jar]
"#;
    let cp = parse_maven_classpath_output(out);
    assert_eq!(
        cp,
        vec![PathBuf::from("/a/b/c.jar"), PathBuf::from("/d/e/f.jar")]
    );
}

#[test]
fn parses_maven_classpath_multiline_bracket_list() {
    let out = r#"
[INFO] something
[
/a/b/c.jar,
/d/e/f.jar
]
"#;
    let cp = parse_maven_classpath_output(out);
    assert_eq!(
        cp,
        vec![PathBuf::from("/a/b/c.jar"), PathBuf::from("/d/e/f.jar")]
    );
}

#[test]
fn parses_maven_classpath_skips_download_lines() {
    let out = r#"
Downloading from central: https://repo1.maven.org/maven2/
Downloaded from central: https://repo1.maven.org/maven2/ (10 kB at 1.2 MB/s)
[/a/b/c.jar, /d/e/f.jar]
"#;
    let cp = parse_maven_classpath_output(out);
    assert_eq!(
        cp,
        vec![PathBuf::from("/a/b/c.jar"), PathBuf::from("/d/e/f.jar")]
    );
}

#[test]
fn parses_maven_evaluate_scalar_output_with_noise() {
    let out = r#"
[INFO] Scanning for projects...
[INFO] --- maven-help-plugin:evaluate (default-cli) @ demo ---
17
"#;
    assert_eq!(
        parse_maven_evaluate_scalar_output(out),
        Some("17".to_string())
    );
    assert_eq!(parse_maven_evaluate_scalar_output("null\n"), None);
}

#[test]
fn unions_java_compile_configs_for_multi_module_roots() {
    let cfg_a = JavaCompileConfig {
        compile_classpath: vec![PathBuf::from("/a.jar"), PathBuf::from("/shared.jar")],
        test_classpath: vec![PathBuf::from("/a-test.jar"), PathBuf::from("/shared.jar")],
        module_path: Vec::new(),
        main_source_roots: vec![PathBuf::from("/module-a/src/main/java")],
        test_source_roots: vec![PathBuf::from("/module-a/src/test/java")],
        main_output_dir: Some(PathBuf::from("/module-a/target/classes")),
        test_output_dir: Some(PathBuf::from("/module-a/target/test-classes")),
        source: Some("17".to_string()),
        target: Some("17".to_string()),
        release: None,
        enable_preview: false,
    };

    let cfg_b = JavaCompileConfig {
        compile_classpath: vec![PathBuf::from("/shared.jar"), PathBuf::from("/b.jar")],
        test_classpath: vec![PathBuf::from("/shared.jar"), PathBuf::from("/b-test.jar")],
        module_path: Vec::new(),
        main_source_roots: vec![PathBuf::from("/module-b/src/main/java")],
        test_source_roots: vec![PathBuf::from("/module-b/src/test/java")],
        main_output_dir: Some(PathBuf::from("/module-b/target/classes")),
        test_output_dir: Some(PathBuf::from("/module-b/target/test-classes")),
        source: Some("17".to_string()),
        target: Some("17".to_string()),
        release: None,
        enable_preview: true,
    };

    let merged = JavaCompileConfig::union([cfg_a, cfg_b]);
    assert_eq!(
        merged.compile_classpath,
        vec![
            PathBuf::from("/a.jar"),
            PathBuf::from("/shared.jar"),
            PathBuf::from("/b.jar")
        ]
    );
    assert_eq!(
        merged.test_classpath,
        vec![
            PathBuf::from("/a-test.jar"),
            PathBuf::from("/shared.jar"),
            PathBuf::from("/b-test.jar")
        ]
    );
    assert_eq!(
        merged.main_source_roots,
        vec![
            PathBuf::from("/module-a/src/main/java"),
            PathBuf::from("/module-b/src/main/java")
        ]
    );
    assert_eq!(
        merged.test_source_roots,
        vec![
            PathBuf::from("/module-a/src/test/java"),
            PathBuf::from("/module-b/src/test/java")
        ]
    );

    // Output dirs are module-specific; the union model drops them.
    assert_eq!(merged.main_output_dir, None);
    assert_eq!(merged.test_output_dir, None);

    // Language level and preview flags are best-effort.
    assert_eq!(merged.source.as_deref(), Some("17"));
    assert_eq!(merged.target.as_deref(), Some("17"));
    assert!(merged.enable_preview);
}

#[test]
fn parses_maven_javac_diagnostics_with_continuation_lines() {
    let out = r#"
[ERROR] /workspace/src/main/java/com/example/Foo.java:[10,5] cannot find symbol
[ERROR]   symbol:   variable x
[ERROR]   location: class com.example.Foo
"#;
    let diags = parse_javac_diagnostics(out, "maven");
    assert_eq!(diags.len(), 1);
    let d = &diags[0];
    assert_eq!(
        d.file,
        PathBuf::from("/workspace/src/main/java/com/example/Foo.java")
    );
    assert_eq!(d.severity, DiagnosticSeverity::Error);
    assert_eq!(d.range, Range::point(Position::new(9, 4)));
    assert!(d.message.contains("cannot find symbol"));
    assert!(d.message.contains("symbol:"));
    assert!(d.message.contains("location:"));
}

#[test]
fn parses_standard_javac_diagnostics_with_caret_column() {
    let out = r#"
/workspace/src/main/java/com/example/Foo.java:10: error: cannot find symbol
        foo.bar();
            ^
  symbol:   method bar()
  location: variable foo of type Foo
"#;
    let diags = parse_javac_diagnostics(out, "gradle");
    assert_eq!(diags.len(), 1);
    let d = &diags[0];
    assert_eq!(
        d.file,
        PathBuf::from("/workspace/src/main/java/com/example/Foo.java")
    );
    assert_eq!(d.severity, DiagnosticSeverity::Error);
    // caret in the sample line points at the 13th character (1-based).
    assert_eq!(d.range, Range::point(Position::new(9, 12)));
    assert!(d.message.contains("cannot find symbol"));
    assert!(d.message.contains("symbol:"));
}

#[test]
fn parses_gradle_classpath_newline_list() {
    let out = r#"
/a/b/c.jar
/d/e/f.jar
"#;
    let cp = parse_gradle_classpath_output(out);
    assert_eq!(
        cp,
        vec![PathBuf::from("/a/b/c.jar"), PathBuf::from("/d/e/f.jar")]
    );
}

#[test]
fn parses_gradle_projects_json_block_from_noisy_output() {
    let out = r#"
> Task :printNovaProjects
Some random warning
NOVA_PROJECTS_BEGIN
{"projects":[{"path":":","projectDir":"/workspace"},{"path":":app","projectDir":"/workspace/app"}]}
NOVA_PROJECTS_END
BUILD SUCCESSFUL
"#;
    let projects = parse_gradle_projects_output(out).unwrap();
    assert_eq!(
        projects,
        vec![
            GradleProjectInfo {
                path: ":".into(),
                dir: PathBuf::from("/workspace"),
            },
            GradleProjectInfo {
                path: ":app".into(),
                dir: PathBuf::from("/workspace/app"),
            }
        ]
    );
}

#[test]
fn parses_gradle_annotation_processing_json() {
    let out = r#"
> Task :printNovaAnnotationProcessing
Some random warning
NOVA_APT_BEGIN
{"main":{"annotationProcessorPath":["/ap/lombok.jar"],"compilerArgs":["-Afoo=bar","-processor","com.example.Proc"],"generatedSourcesDir":"/workspace/build/generated/sources/annotationProcessor/java/main"},"test":{"annotationProcessorPath":[],"compilerArgs":["-proc:none"],"generatedSourcesDir":"/workspace/build/generated/sources/annotationProcessor/java/test"}}
NOVA_APT_END
BUILD SUCCESSFUL
"#;

    let ap = parse_gradle_annotation_processing_output(out).unwrap();
    let main = ap.main.unwrap();
    assert!(main.enabled);
    assert_eq!(
        main.generated_sources_dir,
        Some(PathBuf::from(
            "/workspace/build/generated/sources/annotationProcessor/java/main"
        ))
    );
    assert_eq!(main.processor_path, vec![PathBuf::from("/ap/lombok.jar")]);
    assert_eq!(main.processors, vec!["com.example.Proc".to_string()]);
    assert_eq!(main.options.get("foo").map(String::as_str), Some("bar"));

    let test = ap.test.unwrap();
    assert!(!test.enabled);
}

#[test]
fn parses_maven_effective_pom_annotation_processing() {
    let xml = r#"
<project>
  <build>
    <plugins>
      <plugin>
        <groupId>org.apache.maven.plugins</groupId>
        <artifactId>maven-compiler-plugin</artifactId>
        <configuration>
          <proc>none</proc>
          <generatedSourcesDirectory>generated</generatedSourcesDirectory>
          <generatedTestSourcesDirectory>generated-test</generatedTestSourcesDirectory>
          <compilerArgs>
            <arg>-Afoo=bar</arg>
            <arg>-processor</arg>
            <arg>com.example.Proc</arg>
          </compilerArgs>
        </configuration>
      </plugin>
    </plugins>
  </build>
</project>
"#;

    let ap = parse_maven_effective_pom_annotation_processing(xml, Path::new("/workspace/app"))
        .unwrap();
    let main = ap.main.unwrap();
    assert!(!main.enabled);
    assert_eq!(
        main.generated_sources_dir,
        Some(PathBuf::from("/workspace/app/generated"))
    );
    assert_eq!(main.processors, vec!["com.example.Proc".to_string()]);
    assert_eq!(main.options.get("foo").map(String::as_str), Some("bar"));

    let test = ap.test.unwrap();
    assert!(!test.enabled);
    assert_eq!(
        test.generated_sources_dir,
        Some(PathBuf::from("/workspace/app/generated-test"))
    );
}
