use nova_build_bazel::{BazelWorkspace, CommandOutput, CommandRunner};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};
use tempfile::tempdir;

#[derive(Debug, Default, Clone)]
struct FakeRunner {
    calls: Arc<Mutex<Vec<Vec<String>>>>,
}

impl FakeRunner {
    fn count_subcommand(&self, subcommand: &str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|args| args.first().is_some_and(|arg| arg == subcommand))
            .count()
    }
}

impl CommandRunner for FakeRunner {
    fn run(&self, _cwd: &Path, program: &str, args: &[&str]) -> anyhow::Result<CommandOutput> {
        assert_eq!(program, "bazel");
        self.calls
            .lock()
            .unwrap()
            .push(args.iter().map(|s| s.to_string()).collect());

        let stdout = match args {
            ["query", _] => "//:hello\n".to_string(),
            ["aquery", "--output=textproto", _] => r#"
action {
  mnemonic: "Javac"
  owner: "//:hello"
  arguments: "-classpath"
  arguments: "a.jar"
}
"#
            .to_string(),
            _ => panic!("unexpected bazel invocation: {args:?}"),
        };

        Ok(CommandOutput {
            stdout,
            stderr: String::new(),
        })
    }
}

#[test]
fn bazelrc_digest_invalidation_triggers_aquery() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    std::fs::write(root.join("BUILD"), r#"java_library(name = "hello")"#).unwrap();
    std::fs::write(root.join(".bazelrc"), "build --javacopt=-Xlint").unwrap();

    let runner = FakeRunner::default();
    let mut workspace = BazelWorkspace::new(root.to_path_buf(), runner.clone()).unwrap();

    let info = workspace.target_compile_info("//:hello").unwrap();
    assert_eq!(info.classpath, vec!["a.jar".to_string()]);
    assert_eq!(runner.count_subcommand("aquery"), 1);

    // Cache hit: no additional aquery calls.
    let _ = workspace.target_compile_info("//:hello").unwrap();
    assert_eq!(runner.count_subcommand("aquery"), 1);

    // Editing `.bazelrc` should invalidate the compile-info cache key.
    std::fs::write(root.join(".bazelrc"), "build --javacopt=-Xlint:unchecked").unwrap();
    let _ = workspace.target_compile_info("//:hello").unwrap();
    assert_eq!(runner.count_subcommand("aquery"), 2);
}

