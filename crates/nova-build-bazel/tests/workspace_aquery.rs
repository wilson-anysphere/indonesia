use anyhow::Result;
use nova_build_bazel::{BazelWorkspace, CommandOutput, CommandRunner};
use std::{
    io::{BufRead, BufReader, Cursor, Read},
    path::Path,
    sync::{Arc, Mutex},
};
use tempfile::tempdir;

#[derive(Clone)]
struct TestRunner {
    calls: Arc<Mutex<Vec<Vec<String>>>>,
    aquery_factory: Arc<dyn Fn(&str) -> Box<dyn Read + Send> + Send + Sync>,
}

impl TestRunner {
    fn new(aquery_factory: impl Fn(&str) -> Box<dyn Read + Send> + Send + Sync + 'static) -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            aquery_factory: Arc::new(aquery_factory),
        }
    }

    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }
}

impl CommandRunner for TestRunner {
    fn run(&self, _cwd: &Path, program: &str, args: &[&str]) -> Result<CommandOutput> {
        assert_eq!(program, "bazel");
        self.calls
            .lock()
            .unwrap()
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
            .unwrap()
            .push(args.iter().map(|s| s.to_string()).collect());

        assert_eq!(args.get(0).copied(), Some("aquery"));
        assert_eq!(args.get(1).copied(), Some("--output=textproto"));
        let expr = args.get(2).expect("missing aquery expression");

        let reader = (self.aquery_factory)(expr);
        let mut reader = BufReader::new(reader);
        f(&mut reader)
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

    assert_eq!(info.classpath, vec!["a.jar".to_string(), "b.jar".to_string()]);

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
