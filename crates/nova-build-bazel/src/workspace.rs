use crate::{
    aquery::{
        compile_info_by_owner, extract_java_compile_info, parse_aquery_textproto, JavaCompileInfo,
    },
    cache::{digest_file, BazelCache, CacheEntry},
    command::CommandRunner,
};
use anyhow::{Context, Result};
use blake3::Hash;
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BazelWorkspaceDiscovery {
    pub root: PathBuf,
}

impl BazelWorkspaceDiscovery {
    pub fn discover(start: impl AsRef<Path>) -> Option<Self> {
        nova_project::bazel_workspace_root(start).map(|root| Self { root })
    }
}

#[derive(Debug)]
pub struct BazelWorkspace<R: CommandRunner> {
    root: PathBuf,
    runner: R,
    cache_path: Option<PathBuf>,
    cache: BazelCache,
    last_query_hash: Option<Hash>,
}

impl<R: CommandRunner> BazelWorkspace<R> {
    pub fn new(root: PathBuf, runner: R) -> Result<Self> {
        Ok(Self {
            root,
            runner,
            cache_path: None,
            cache: BazelCache::default(),
            last_query_hash: None,
        })
    }

    pub fn with_cache_path(mut self, path: PathBuf) -> Result<Self> {
        self.cache = BazelCache::load(&path)?;
        self.cache_path = Some(path);
        Ok(self)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn java_targets(&mut self) -> Result<Vec<String>> {
        let output = self.runner.run(
            &self.root,
            "bazel",
            &["query", r#"kind("java_.* rule", //...)"#],
        )?;
        let query_hash = blake3::hash(output.stdout.as_bytes());
        self.last_query_hash = Some(query_hash);
        Ok(output
            .stdout
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(|l| l.to_string())
            .collect())
    }

    /// Resolve Java compilation information for a Bazel target.
    pub fn target_compile_info(&mut self, target: &str) -> Result<JavaCompileInfo> {
        let query_hash = self.ensure_query_hash()?;

        let build_file_digests = self.build_file_digests_for_target(target)?;

        if let Some(entry) = self.cache.get(target, query_hash, &build_file_digests) {
            return Ok(entry.info.clone());
        }

        let aquery = self.runner.run(
            &self.root,
            "bazel",
            &[
                "aquery",
                "--output=textproto",
                &format!(r#"mnemonic("Javac", deps({target}))"#),
            ],
        )?;

        // `deps(target)` returns `Javac` actions for the target _and_ its dependencies. Prefer the
        // action owned by `target` if present; otherwise fall back to the first available action.
        let by_owner = compile_info_by_owner(&aquery.stdout);
        let info = if let Some(info) = by_owner.get(target) {
            info.clone()
        } else if let Some((_, info)) = by_owner.into_iter().next() {
            info
        } else {
            let action = parse_aquery_textproto(&aquery.stdout)
                .into_iter()
                .next()
                .with_context(|| format!("no Javac actions found for {target}"))?;
            extract_java_compile_info(&action)
        };

        self.cache.insert(CacheEntry {
            target: target.to_string(),
            query_hash_hex: query_hash.to_hex().to_string(),
            build_files: build_file_digests.clone(),
            info: info.clone(),
        });

        self.persist_cache()?;

        Ok(info)
    }

    pub fn invalidate_changed_build_files(&mut self, changed: &[PathBuf]) -> Result<()> {
        self.cache.invalidate_changed_build_files(changed);
        self.persist_cache()
    }

    fn persist_cache(&self) -> Result<()> {
        if let Some(path) = &self.cache_path {
            self.cache.save(path)?;
        }
        Ok(())
    }

    fn build_file_digests_for_target(
        &self,
        target: &str,
    ) -> Result<Vec<crate::cache::BuildFileDigest>> {
        let Some(build_file) = build_file_for_label(&self.root, target)? else {
            return Ok(Vec::new());
        };
        Ok(vec![digest_file(&build_file)?])
    }

    fn ensure_query_hash(&mut self) -> Result<Hash> {
        if let Some(hash) = self.last_query_hash {
            return Ok(hash);
        }

        let output = self.runner.run(
            &self.root,
            "bazel",
            &["query", r#"kind("java_.* rule", //...)"#],
        )?;
        let hash = blake3::hash(output.stdout.as_bytes());
        self.last_query_hash = Some(hash);
        Ok(hash)
    }
}

fn build_file_for_label(workspace_root: &Path, label: &str) -> Result<Option<PathBuf>> {
    let Some(rest) = label.strip_prefix("//") else {
        return Ok(None);
    };
    let package = rest.split(':').next().unwrap_or(rest);
    let package_path = if package.is_empty() {
        workspace_root.to_path_buf()
    } else {
        workspace_root.join(package)
    };

    // Bazel allows either BUILD or BUILD.bazel.
    for name in ["BUILD.bazel", "BUILD"] {
        let candidate = package_path.join(name);
        if candidate.is_file() {
            return Ok(Some(candidate));
        }
    }

    // Some repositories use symlinks or generated BUILD files; avoid failing hard.
    if package_path.exists() {
        if let Ok(read_dir) = fs::read_dir(&package_path) {
            for entry in read_dir.flatten() {
                let file_name = entry.file_name();
                let file_name = file_name.to_string_lossy();
                if file_name == "BUILD" || file_name == "BUILD.bazel" {
                    return Ok(Some(entry.path()));
                }
            }
        }
    }

    Ok(None)
}
