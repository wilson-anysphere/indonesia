use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BenchMetric {
    /// Median / p50 per-iteration time in nanoseconds.
    pub p50_ns: u64,
    /// p95 per-iteration time in nanoseconds.
    pub p95_ns: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BenchRun {
    pub benchmarks: BTreeMap<String, BenchMetric>,
}

impl BenchRun {
    pub fn write_json(&self, path: impl AsRef<Path>) -> Result<()> {
        let serialized = serde_json::to_string_pretty(self)?;
        fs::write(path.as_ref(), serialized).with_context(|| {
            format!(
                "failed to write benchmark run JSON to {}",
                path.as_ref().display()
            )
        })?;
        Ok(())
    }

    pub fn read_json(path: impl AsRef<Path>) -> Result<Self> {
        let bytes = fs::read(path.as_ref()).with_context(|| {
            format!(
                "failed to read benchmark run JSON from {}",
                path.as_ref().display()
            )
        })?;
        Ok(serde_json::from_slice(&bytes)?)
    }
}

#[derive(Debug, Deserialize)]
struct CriterionSample {
    iters: Vec<f64>,
    times: Vec<f64>,
}

/// Load `criterion` benchmarks from a `target/criterion` directory.
///
/// We only depend on `new/sample.json`, which exists regardless of HTML report generation.
pub fn load_criterion_directory(path: impl AsRef<Path>) -> Result<BenchRun> {
    let root = path.as_ref();
    if !root.exists() {
        return Err(anyhow!(
            "criterion directory does not exist: {}",
            root.display()
        ));
    }

    let mut run = BenchRun::default();

    for entry in WalkDir::new(root)
        .into_iter()
        .filter_map(|entry| entry.ok())
    {
        if entry.file_name() != "sample.json" {
            continue;
        }

        let sample_path = entry.path();
        if sample_path.parent().and_then(|parent| parent.file_name()) != Some(OsStr::new("new")) {
            continue;
        }

        let bench_dir = sample_path
            .parent()
            .and_then(|p| p.parent())
            .ok_or_else(|| anyhow!("unexpected criterion layout for {}", sample_path.display()))?;

        let bench_id = bench_dir
            .strip_prefix(root)
            .unwrap_or(bench_dir)
            .to_string_lossy()
            .replace('\\', "/");

        if bench_id.is_empty() {
            continue;
        }

        let bytes =
            fs::read(sample_path).with_context(|| format!("read {}", sample_path.display()))?;
        let sample: CriterionSample = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse {}", sample_path.display()))?;

        let metric = metric_from_sample(&sample).with_context(|| {
            format!(
                "failed to compute metric for benchmark {bench_id} ({})",
                sample_path.display()
            )
        })?;

        run.benchmarks.insert(bench_id, metric);
    }

    if run.benchmarks.is_empty() {
        return Err(anyhow!(
            "no benchmarks found under criterion directory {}",
            root.display()
        ));
    }

    Ok(run)
}

fn metric_from_sample(sample: &CriterionSample) -> Result<BenchMetric> {
    if sample.iters.len() != sample.times.len() {
        return Err(anyhow!(
            "sample iters/times length mismatch (iters={}, times={})",
            sample.iters.len(),
            sample.times.len()
        ));
    }

    let mut per_iter_ns = Vec::with_capacity(sample.times.len());
    for (iters, total_ns) in sample.iters.iter().zip(sample.times.iter()) {
        if *iters <= 0.0 {
            continue;
        }
        per_iter_ns.push(total_ns / *iters);
    }

    per_iter_ns.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    if per_iter_ns.is_empty() {
        return Err(anyhow!("no samples in criterion run"));
    }

    let p50_ns = percentile(&per_iter_ns, 0.50).round().max(0.0) as u64;
    let p95_ns = percentile(&per_iter_ns, 0.95).round().max(0.0) as u64;

    Ok(BenchMetric { p50_ns, p95_ns })
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }

    if sorted.len() == 1 {
        return sorted[0];
    }

    let p = p.clamp(0.0, 1.0);
    let idx = p * (sorted.len() - 1) as f64;
    let lower = idx.floor() as usize;
    let upper = idx.ceil() as usize;
    if lower == upper {
        return sorted[lower];
    }

    let weight = idx - lower as f64;
    sorted[lower] * (1.0 - weight) + sorted[upper] * weight
}

/// Regression thresholds for a benchmark comparison.
///
/// ## Serde JSON representation (stable)
///
/// Serialized as an object with the following keys:
///
/// - `p50_regression`
/// - `p95_regression`
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct Thresholds {
    #[serde(default = "default_p50_regression")]
    pub p50_regression: f64,
    #[serde(default = "default_p95_regression")]
    pub p95_regression: f64,
}

fn default_p50_regression() -> f64 {
    0.10
}

fn default_p95_regression() -> f64 {
    0.20
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds {
            p50_regression: default_p50_regression(),
            p95_regression: default_p95_regression(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ThresholdConfig {
    #[serde(default)]
    pub default: Thresholds,
    #[serde(default)]
    pub benchmarks: BTreeMap<String, Thresholds>,
    #[serde(default)]
    pub allow_regressions: Vec<String>,
}

impl ThresholdConfig {
    pub fn read_toml(path: impl AsRef<Path>) -> Result<Self> {
        let content = fs::read_to_string(path.as_ref()).with_context(|| {
            format!(
                "failed to read thresholds TOML from {}",
                path.as_ref().display()
            )
        })?;
        Ok(toml::from_str(&content)?)
    }

    pub fn thresholds_for(&self, bench_id: &str) -> Thresholds {
        self.benchmarks
            .get(bench_id)
            .copied()
            .unwrap_or(self.default)
    }
}

/// The outcome of comparing a benchmark between two runs.
///
/// ## Serde JSON representation (stable)
///
/// When serialized as JSON, this enum is represented as a string with one of the
/// following values:
///
/// - `"ok"`
/// - `"regression"`
/// - `"allowed_regression"`
/// - `"missing_in_current"`
/// - `"new_in_current"`
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum DiffStatus {
    #[serde(rename = "ok")]
    Ok,
    #[serde(rename = "regression")]
    Regression,
    #[serde(rename = "allowed_regression")]
    AllowedRegression,
    #[serde(rename = "missing_in_current")]
    MissingInCurrent,
    #[serde(rename = "new_in_current")]
    NewInCurrent,
}

/// The per-benchmark diff produced by [`compare_runs`].
///
/// ## Serde JSON representation (stable)
///
/// Serialized as an object with the following keys:
///
/// - `id`: benchmark identifier
/// - `baseline`: baseline metrics (or `null`)
/// - `current`: current metrics (or `null`)
/// - `p50_change`: relative p50 change (e.g. `0.12` for +12%) (or `null`)
/// - `p95_change`: relative p95 change (or `null`)
/// - `thresholds`: thresholds used for the comparison (or `null`)
/// - `status`: [`DiffStatus`] as a stable string
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BenchDiff {
    pub id: String,
    pub baseline: Option<BenchMetric>,
    pub current: Option<BenchMetric>,
    pub p50_change: Option<f64>,
    pub p95_change: Option<f64>,
    pub thresholds: Option<Thresholds>,
    pub status: DiffStatus,
}

/// The result of comparing two [`BenchRun`]s.
///
/// ## Serde JSON representation (stable)
///
/// Serialized as an object with the following keys:
///
/// - `diffs`: array of [`BenchDiff`]
/// - `has_failure`: `true` if the comparison contains any hard failures
///   (e.g. regressions not allowlisted, or benchmarks missing in the current run)
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Comparison {
    pub diffs: Vec<BenchDiff>,
    pub has_failure: bool,
}

impl Comparison {
    /// Write this comparison as pretty JSON to `path`.
    pub fn write_json(&self, path: impl AsRef<Path>) -> Result<()> {
        let serialized = serde_json::to_string_pretty(self)?;
        fs::write(path.as_ref(), serialized).with_context(|| {
            format!(
                "failed to write comparison JSON to {}",
                path.as_ref().display()
            )
        })?;
        Ok(())
    }

    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str("## Nova performance report\n\n");
        out.push_str("| Benchmark | p50 (base) | p50 (new) | Δp50 | p95 (base) | p95 (new) | Δp95 | Status |\n");
        out.push_str("|---|---:|---:|---:|---:|---:|---:|---|\n");

        for diff in &self.diffs {
            let (base_p50, base_p95) = diff
                .baseline
                .as_ref()
                .map(|m| (format_ns(m.p50_ns), format_ns(m.p95_ns)))
                .unwrap_or_else(|| ("—".to_string(), "—".to_string()));
            let (cur_p50, cur_p95) = diff
                .current
                .as_ref()
                .map(|m| (format_ns(m.p50_ns), format_ns(m.p95_ns)))
                .unwrap_or_else(|| ("—".to_string(), "—".to_string()));

            let p50_delta = diff
                .p50_change
                .map(format_pct)
                .unwrap_or_else(|| "—".to_string());
            let p95_delta = diff
                .p95_change
                .map(format_pct)
                .unwrap_or_else(|| "—".to_string());

            out.push_str(&format!(
                "| `{}` | {} | {} | {} | {} | {} | {} | {} |\n",
                diff.id, base_p50, cur_p50, p50_delta, base_p95, cur_p95, p95_delta, diff.status
            ));
        }

        let regressions: Vec<&BenchDiff> = self
            .diffs
            .iter()
            .filter(|d| d.status == DiffStatus::Regression)
            .collect();
        if !regressions.is_empty() {
            out.push_str("\n### Top regressions\n\n");
            let mut regressions = regressions;
            regressions.sort_by(|a, b| {
                b.p50_change
                    .unwrap_or(0.0)
                    .partial_cmp(&a.p50_change.unwrap_or(0.0))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            for diff in regressions.into_iter().take(5) {
                out.push_str(&format!(
                    "- `{}`: p50 {}, p95 {}\n",
                    diff.id,
                    diff.p50_change.map(format_pct).unwrap_or_default(),
                    diff.p95_change.map(format_pct).unwrap_or_default()
                ));
            }
        }

        out
    }
}

impl fmt::Display for DiffStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiffStatus::Ok => write!(f, "OK"),
            DiffStatus::Regression => write!(f, "REGRESSION"),
            DiffStatus::AllowedRegression => write!(f, "ALLOWED"),
            DiffStatus::MissingInCurrent => write!(f, "MISSING"),
            DiffStatus::NewInCurrent => write!(f, "NEW"),
        }
    }
}

pub fn compare_runs(
    baseline: &BenchRun,
    current: &BenchRun,
    config: &ThresholdConfig,
    extra_allowed: &[String],
) -> Comparison {
    let mut diffs = Vec::new();
    let mut has_failure = false;

    let mut allowed = config.allow_regressions.clone();
    allowed.extend(extra_allowed.iter().cloned());

    for (bench_id, base_metric) in &baseline.benchmarks {
        match current.benchmarks.get(bench_id) {
            Some(cur_metric) => {
                let thresholds = config.thresholds_for(bench_id);

                let p50_change =
                    change_ratio(base_metric.p50_ns, cur_metric.p50_ns).map(|r| r - 1.0);
                let p95_change =
                    change_ratio(base_metric.p95_ns, cur_metric.p95_ns).map(|r| r - 1.0);

                let regression = p50_change.unwrap_or(0.0) > thresholds.p50_regression
                    || p95_change.unwrap_or(0.0) > thresholds.p95_regression;

                let is_allowed = allowed.iter().any(|a| a == bench_id);
                let status = if regression && is_allowed {
                    DiffStatus::AllowedRegression
                } else if regression {
                    has_failure = true;
                    DiffStatus::Regression
                } else {
                    DiffStatus::Ok
                };

                diffs.push(BenchDiff {
                    id: bench_id.clone(),
                    baseline: Some(base_metric.clone()),
                    current: Some(cur_metric.clone()),
                    p50_change,
                    p95_change,
                    thresholds: Some(thresholds),
                    status,
                });
            }
            None => {
                has_failure = true;
                diffs.push(BenchDiff {
                    id: bench_id.clone(),
                    baseline: Some(base_metric.clone()),
                    current: None,
                    p50_change: None,
                    p95_change: None,
                    thresholds: None,
                    status: DiffStatus::MissingInCurrent,
                });
            }
        }
    }

    for (bench_id, metric) in &current.benchmarks {
        if baseline.benchmarks.contains_key(bench_id) {
            continue;
        }
        diffs.push(BenchDiff {
            id: bench_id.clone(),
            baseline: None,
            current: Some(metric.clone()),
            p50_change: None,
            p95_change: None,
            thresholds: Some(config.thresholds_for(bench_id)),
            status: DiffStatus::NewInCurrent,
        });
    }

    diffs.sort_by(|a, b| a.id.cmp(&b.id));

    Comparison { diffs, has_failure }
}

fn change_ratio(baseline: u64, current: u64) -> Option<f64> {
    if baseline == 0 {
        return None;
    }
    Some(current as f64 / baseline as f64)
}

fn format_pct(v: f64) -> String {
    format!("{:+.1}%", v * 100.0)
}

fn format_ns(ns: u64) -> String {
    let ns_f = ns as f64;
    if ns < 1_000 {
        return format!("{ns} ns");
    }
    if ns < 1_000_000 {
        return format!("{:.1} µs", ns_f / 1_000.0);
    }
    if ns < 1_000_000_000 {
        return format!("{:.1} ms", ns_f / 1_000_000.0);
    }
    format!("{:.2} s", ns_f / 1_000_000_000.0)
}

/// Convenience for CI/workflows: resolve a path relative to the repository root.
pub fn resolve_repo_relative_path(path: &str) -> PathBuf {
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_with_single_bench(p50: u64, p95: u64) -> BenchRun {
        BenchRun {
            benchmarks: BTreeMap::from([(
                "parsing/small".to_string(),
                BenchMetric {
                    p50_ns: p50,
                    p95_ns: p95,
                },
            )]),
        }
    }

    #[test]
    fn detects_regression_against_default_thresholds() {
        let baseline = run_with_single_bench(100, 200);
        let current = run_with_single_bench(120, 210);
        let config = ThresholdConfig::default();

        let comparison = compare_runs(&baseline, &current, &config, &[]);
        assert!(comparison.has_failure);
        assert_eq!(comparison.diffs[0].status, DiffStatus::Regression);
    }

    #[test]
    fn allows_regression_when_bench_is_allowlisted() {
        let baseline = run_with_single_bench(100, 200);
        let current = run_with_single_bench(120, 300);

        let mut config = ThresholdConfig::default();
        config.allow_regressions.push("parsing/small".to_string());

        let comparison = compare_runs(&baseline, &current, &config, &[]);
        assert!(!comparison.has_failure);
        assert_eq!(comparison.diffs[0].status, DiffStatus::AllowedRegression);
    }

    #[test]
    fn per_benchmark_thresholds_override_default() {
        let baseline = run_with_single_bench(100, 200);
        let current = run_with_single_bench(115, 230);

        let mut config = ThresholdConfig::default();
        config.benchmarks.insert(
            "parsing/small".to_string(),
            Thresholds {
                p50_regression: 0.20,
                p95_regression: 0.20,
            },
        );

        let comparison = compare_runs(&baseline, &current, &config, &[]);
        assert!(!comparison.has_failure);
        assert_eq!(comparison.diffs[0].status, DiffStatus::Ok);
    }

    #[test]
    fn missing_benchmark_is_a_failure() {
        let baseline = run_with_single_bench(100, 200);
        let current = BenchRun::default();
        let config = ThresholdConfig::default();

        let comparison = compare_runs(&baseline, &current, &config, &[]);
        assert!(comparison.has_failure);
        assert_eq!(comparison.diffs[0].status, DiffStatus::MissingInCurrent);
    }

    #[test]
    fn diff_status_serializes_as_stable_snake_case_strings() {
        assert_eq!(serde_json::to_string(&DiffStatus::Ok).unwrap(), "\"ok\"");
        assert_eq!(
            serde_json::to_string(&DiffStatus::Regression).unwrap(),
            "\"regression\""
        );
        assert_eq!(
            serde_json::to_string(&DiffStatus::AllowedRegression).unwrap(),
            "\"allowed_regression\""
        );
        assert_eq!(
            serde_json::to_string(&DiffStatus::MissingInCurrent).unwrap(),
            "\"missing_in_current\""
        );
        assert_eq!(
            serde_json::to_string(&DiffStatus::NewInCurrent).unwrap(),
            "\"new_in_current\""
        );
    }

    #[test]
    fn comparison_json_roundtrips() {
        let baseline = BenchRun {
            benchmarks: BTreeMap::from([
                (
                    "ok/bench".to_string(),
                    BenchMetric {
                        p50_ns: 100,
                        p95_ns: 200,
                    },
                ),
                (
                    "regression/bench".to_string(),
                    BenchMetric {
                        p50_ns: 100,
                        p95_ns: 200,
                    },
                ),
                (
                    "allowed/bench".to_string(),
                    BenchMetric {
                        p50_ns: 100,
                        p95_ns: 200,
                    },
                ),
                (
                    "missing/bench".to_string(),
                    BenchMetric {
                        p50_ns: 100,
                        p95_ns: 200,
                    },
                ),
            ]),
        };

        let current = BenchRun {
            benchmarks: BTreeMap::from([
                (
                    "ok/bench".to_string(),
                    BenchMetric {
                        p50_ns: 105,
                        p95_ns: 210,
                    },
                ),
                (
                    "regression/bench".to_string(),
                    BenchMetric {
                        p50_ns: 120,
                        p95_ns: 210,
                    },
                ),
                (
                    "allowed/bench".to_string(),
                    BenchMetric {
                        p50_ns: 130,
                        p95_ns: 260,
                    },
                ),
                (
                    "new/bench".to_string(),
                    BenchMetric {
                        p50_ns: 50,
                        p95_ns: 100,
                    },
                ),
            ]),
        };

        let mut config = ThresholdConfig::default();
        config.allow_regressions.push("allowed/bench".to_string());

        let comparison = compare_runs(&baseline, &current, &config, &[]);
        let json = serde_json::to_string_pretty(&comparison).unwrap();
        let decoded: Comparison = serde_json::from_str(&json).unwrap();

        // JSON floats are not guaranteed to preserve the exact underlying IEEE-754
        // representation. We assert "semantic" equality here, treating floats as
        // equivalent within a small epsilon.
        fn assert_close(label: &str, left: f64, right: f64) {
            assert!(
                (left - right).abs() < 1e-12,
                "{label} differs too much: left={left:?} right={right:?}"
            );
        }

        fn assert_opt_close(label: &str, left: Option<f64>, right: Option<f64>) {
            match (left, right) {
                (None, None) => {}
                (Some(left), Some(right)) => assert_close(label, left, right),
                (left, right) => panic!("{label} differs: left={left:?} right={right:?}"),
            }
        }

        fn assert_opt_thresholds_close(
            label: &str,
            left: Option<Thresholds>,
            right: Option<Thresholds>,
        ) {
            match (left, right) {
                (None, None) => {}
                (Some(left), Some(right)) => {
                    assert_close(
                        &format!("{label}.p50_regression"),
                        left.p50_regression,
                        right.p50_regression,
                    );
                    assert_close(
                        &format!("{label}.p95_regression"),
                        left.p95_regression,
                        right.p95_regression,
                    );
                }
                (left, right) => panic!("{label} differs: left={left:?} right={right:?}"),
            }
        }

        assert_eq!(comparison.has_failure, decoded.has_failure);
        assert_eq!(comparison.diffs.len(), decoded.diffs.len());

        for (idx, (left, right)) in comparison
            .diffs
            .iter()
            .zip(decoded.diffs.iter())
            .enumerate()
        {
            assert_eq!(left.id, right.id, "diffs[{idx}].id differs");
            assert_eq!(
                left.baseline, right.baseline,
                "diffs[{idx}].baseline differs"
            );
            assert_eq!(left.current, right.current, "diffs[{idx}].current differs");
            assert_opt_close(
                &format!("diffs[{idx}].p50_change"),
                left.p50_change,
                right.p50_change,
            );
            assert_opt_close(
                &format!("diffs[{idx}].p95_change"),
                left.p95_change,
                right.p95_change,
            );
            assert_opt_thresholds_close(
                &format!("diffs[{idx}].thresholds"),
                left.thresholds,
                right.thresholds,
            );
            assert_eq!(left.status, right.status, "diffs[{idx}].status differs");
        }
    }
}
