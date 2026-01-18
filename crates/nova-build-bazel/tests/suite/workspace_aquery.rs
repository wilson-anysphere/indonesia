use anyhow::Result;
use nova_build_bazel::{BazelWorkspace, CommandOutput, CommandRunner};
use std::{
    io::{BufRead, BufReader, Cursor, Read},
    path::Path,
    sync::{Arc, Mutex},
};
use tempfile::tempdir;

type AqueryFactory = Arc<dyn Fn(&str) -> Box<dyn Read + Send> + Send + Sync>;

#[derive(Clone)]
struct TestRunner {
    calls: Arc<Mutex<Vec<Vec<String>>>>,
    aquery_factory: AqueryFactory,
}

impl TestRunner {
    fn new(aquery_factory: impl Fn(&str) -> Box<dyn Read + Send> + Send + Sync + 'static) -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            aquery_factory: Arc::new(aquery_factory),
        }
    }

    fn calls(&self) -> Vec<Vec<String>> {
        self.calls
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    }
}

impl CommandRunner for TestRunner {
    fn run(&self, _cwd: &Path, program: &str, args: &[&str]) -> Result<CommandOutput> {
        assert_eq!(program, "bazel");
        self.calls
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .push(args.iter().map(|s| s.to_string()).collect());

        match args.first().copied() {
            Some("query") => Ok(CommandOutput {
                stdout: "//:dummy\n".to_string(),
                stderr: String::new(),
            }),
            other => panic!("unexpected CommandRunner::run invocation: {other:?}"),
        }
    }

    fn run_with_stdout<R>(
        &self,
        _cwd: &Path,
        program: &str,
        args: &[&str],
        f: impl FnOnce(&mut dyn BufRead) -> Result<R>,
    ) -> Result<R> {
        assert_eq!(program, "bazel");
        self.calls
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .push(args.iter().map(|s| s.to_string()).collect());

        match args.first().copied() {
            Some("query") => {
                let stdout = "//:dummy\n".as_bytes().to_vec();
                let mut reader = BufReader::new(Cursor::new(stdout));
                f(&mut reader)
            }
            Some("aquery") => {
                assert_eq!(args.get(1).copied(), Some("--output=textproto"));
                let expr = args.get(2).expect("missing aquery expression");

                let reader = (self.aquery_factory)(expr);
                let mut reader = BufReader::new(reader);
                f(&mut reader)
            }
            other => panic!("unexpected bazel invocation: {other:?}"),
        }
    }
}

#[test]
fn target_compile_info_uses_direct_aquery_when_available() {
    let target = "//:hello";
    let direct_expr = format!(r#"mnemonic("Javac", {target})"#);
    let direct_expr_for_closure = direct_expr.clone();

    let direct_output = format!(
        r#"
action {{
  mnemonic: "Javac"
  owner: "{target}"
  arguments: "javac"
  arguments: "-classpath"
  arguments: "a.jar:b.jar"
  arguments: "src/Hello.java"
}}
"#
    );

    let runner = TestRunner::new({
        let direct_output = direct_output.clone();
        move |expr| {
            if expr.contains("deps(") {
                panic!("deps(...) query should not be executed when direct query returns a Javac action");
            }
            assert_eq!(expr, direct_expr_for_closure);
            Box::new(Cursor::new(direct_output.clone().into_bytes()))
        }
    });

    let root = tempdir().unwrap();
    let mut workspace = BazelWorkspace::new(root.path().to_path_buf(), runner.clone()).unwrap();
    let info = workspace.target_compile_info(target).unwrap();

    assert_eq!(
        info.classpath,
        vec!["a.jar".to_string(), "b.jar".to_string()]
    );

    let aquery_exprs: Vec<String> = runner
        .calls()
        .into_iter()
        .filter(|args| args.first().map(String::as_str) == Some("aquery"))
        .map(|args| args[2].clone())
        .collect();
    assert_eq!(aquery_exprs, vec![direct_expr]);
}

#[test]
fn target_compile_info_falls_back_to_deps_aquery_and_stops_after_first_action() {
    struct HeadTailReader {
        head: Vec<u8>,
        tail: Vec<u8>,
        head_pos: usize,
        tail_pos: usize,
        bytes_read: usize,
        max_bytes: usize,
    }

    impl Read for HeadTailReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.bytes_read > self.max_bytes {
                panic!(
                    "workspace read too much aquery output: {} bytes (limit {})",
                    self.bytes_read, self.max_bytes
                );
            }

            if self.head_pos < self.head.len() {
                let remaining = &self.head[self.head_pos..];
                let n = remaining.len().min(buf.len());
                buf[..n].copy_from_slice(&remaining[..n]);
                self.head_pos += n;
                self.bytes_read += n;
                return Ok(n);
            }

            if self.tail.is_empty() {
                return Ok(0);
            }

            let remaining = &self.tail[self.tail_pos..];
            let n = remaining.len().min(buf.len());
            buf[..n].copy_from_slice(&remaining[..n]);
            self.tail_pos = (self.tail_pos + n) % self.tail.len();
            self.bytes_read += n;
            Ok(n)
        }
    }

    let target = "//:hello";
    let direct_expr = format!(r#"mnemonic("Javac", {target})"#);
    let deps_expr = format!(r#"mnemonic("Javac", deps({target}))"#);
    let direct_expr_for_closure = direct_expr.clone();
    let deps_expr_for_closure = deps_expr.clone();

    let direct_output = r#"
action {
  mnemonic: "Symlink"
  arguments: "ignored"
}
"#
    .to_string();

    let deps_head = r#"
action {
  mnemonic: "Javac"
  owner: "//:dep"
  arguments: "javac"
  arguments: "-classpath"
  arguments: "dep.jar"
  arguments: "src/Dep.java"
}
"#
    .to_string();

    let deps_tail = r#"
action {
  mnemonic: "Javac"
  owner: "//:another_dep"
  arguments: "javac"
  arguments: "-classpath"
  arguments: "another.jar"
  arguments: "src/Another.java"
}
"#
    .to_string();

    let runner = TestRunner::new({
        let direct_output = direct_output.clone();
        let deps_head = deps_head.clone();
        let deps_tail = deps_tail.clone();
        move |expr| {
            if expr == direct_expr_for_closure {
                return Box::new(Cursor::new(direct_output.clone().into_bytes()));
            }
            if expr == deps_expr_for_closure {
                return Box::new(HeadTailReader {
                    head: deps_head.clone().into_bytes(),
                    tail: deps_tail.clone().into_bytes(),
                    head_pos: 0,
                    tail_pos: 0,
                    bytes_read: 0,
                    // BufReader may prefetch; leave generous room for buffering and parsing.
                    max_bytes: 256 * 1024,
                });
            }
            panic!("unexpected aquery expression: {expr}");
        }
    });

    let root = tempdir().unwrap();
    let mut workspace = BazelWorkspace::new(root.path().to_path_buf(), runner.clone()).unwrap();
    let info = workspace.target_compile_info(target).unwrap();

    assert_eq!(info.classpath, vec!["dep.jar".to_string()]);

    let aquery_exprs: Vec<String> = runner
        .calls()
        .into_iter()
        .filter(|args| args.first().map(String::as_str) == Some("aquery"))
        .map(|args| args[2].clone())
        .collect();
    assert_eq!(aquery_exprs, vec![direct_expr, deps_expr]);
}

#[test]
fn target_compile_info_extracts_full_compile_info() {
    let target = "//java/com/example:hello";
    let direct_expr = format!(r#"mnemonic("Javac", {target})"#);
    let direct_expr_for_closure = direct_expr.clone();

    let output = format!(
        r#"
action {{
  mnemonic: "Javac"
  owner: "{target}"
  arguments: "javac"
  arguments: "-classpath"
  arguments: "a.jar:b.jar"
  arguments: "--module-path"
  arguments: "mods"
  arguments: "--release=21"
  arguments: "--enable-preview"
  arguments: "-d=out/dir"
  arguments: "--source"
  arguments: "17"
  arguments: "--target"
  arguments: "17"
  arguments: "java/com/example/Hello.java"
}}
"#
    );

    let runner = TestRunner::new({
        let output = output.clone();
        move |expr| {
            assert_eq!(expr, direct_expr_for_closure);
            Box::new(Cursor::new(output.clone().into_bytes()))
        }
    });

    let root = tempdir().unwrap();
    let mut workspace = BazelWorkspace::new(root.path().to_path_buf(), runner).unwrap();
    let info = workspace.target_compile_info(target).unwrap();

    assert_eq!(
        info.classpath,
        vec!["a.jar".to_string(), "b.jar".to_string()]
    );
    assert_eq!(info.module_path, vec!["mods".to_string()]);
    assert_eq!(info.release.as_deref(), Some("21"));
    assert!(info.preview);
    assert_eq!(info.output_dir.as_deref(), Some("out/dir"));
    // `--release` implies both language level and bytecode target; ignore any later explicit flags.
    assert_eq!(info.source.as_deref(), Some("21"));
    assert_eq!(info.target.as_deref(), Some("21"));
    assert_eq!(info.source_roots, vec!["java/com/example".to_string()]);
}

#[test]
fn target_compile_info_falls_back_when_direct_aquery_errors() {
    #[derive(Clone)]
    struct FailingDirectRunner {
        direct_expr: String,
        deps_expr: String,
    }

    impl CommandRunner for FailingDirectRunner {
        fn run(&self, _cwd: &Path, _program: &str, _args: &[&str]) -> Result<CommandOutput> {
            unreachable!("workspace uses run_with_stdout for queries")
        }

        fn run_with_stdout<R>(
            &self,
            _cwd: &Path,
            program: &str,
            args: &[&str],
            f: impl FnOnce(&mut dyn BufRead) -> Result<R>,
        ) -> Result<R> {
            assert_eq!(program, "bazel");
            assert_eq!(args.first().copied(), Some("query"));
            // Return an empty query result for buildfiles/loadfiles; those are best-effort inputs.
            let mut reader = BufReader::new(Cursor::new(Vec::new()));
            f(&mut reader)
        }

        fn run_with_stdout_controlled<R>(
            &self,
            _cwd: &Path,
            program: &str,
            args: &[&str],
            f: impl FnOnce(&mut dyn BufRead) -> Result<std::ops::ControlFlow<R, R>>,
        ) -> Result<R> {
            assert_eq!(program, "bazel");
            assert_eq!(args.first().copied(), Some("aquery"));
            let expr = args.get(2).expect("missing aquery expression");

            if *expr == self.direct_expr {
                return Err(anyhow::anyhow!("direct aquery expression unsupported"));
            }

            assert_eq!(*expr, self.deps_expr);
            let output = r#"
action {
  mnemonic: "Javac"
  owner: "//:dep"
  arguments: "-classpath"
  arguments: "a.jar"
  arguments: "src/Dep.java"
}
"#;
            let mut reader = BufReader::new(Cursor::new(output.as_bytes()));
            let result = f(&mut reader)?;
            Ok(match result {
                std::ops::ControlFlow::Continue(value) | std::ops::ControlFlow::Break(value) => {
                    value
                }
            })
        }
    }

    let target = "//:hello";
    let direct_expr = format!(r#"mnemonic("Javac", {target})"#);
    let deps_expr = format!(r#"mnemonic("Javac", deps({target}))"#);

    let root = tempdir().unwrap();
    let runner = FailingDirectRunner {
        direct_expr,
        deps_expr,
    };

    let mut workspace = BazelWorkspace::new(root.path().to_path_buf(), runner).unwrap();
    let info = workspace.target_compile_info(target).unwrap();
    assert_eq!(info.classpath, vec!["a.jar".to_string()]);
}
