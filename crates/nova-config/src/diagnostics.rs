use serde::de::DeserializeOwned;
use std::path::PathBuf;

/// Combined diagnostics produced while loading and validating a Nova config.
///
/// Loading diagnostics are "best effort": callers always get a `NovaConfig` when deserialization
/// succeeds, plus a set of diagnostics describing issues that may impact runtime behavior.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigDiagnostics {
    /// Keys present in the input TOML that were not recognized by the current `NovaConfig` schema.
    ///
    /// These are collected via `serde_ignored` so nested tables use the full path (for example
    /// `ai.provider.kindd`).
    pub unknown_keys: Vec<String>,
    /// Non-fatal issues (deprecated keys, invalid-but-recoverable values, missing optional
    /// filesystem paths, etc).
    pub warnings: Vec<ConfigWarning>,
    /// Fatal semantic validation failures (config is internally inconsistent and would likely fail
    /// at runtime).
    pub errors: Vec<ConfigValidationError>,
}

impl ConfigDiagnostics {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.unknown_keys.is_empty() && self.warnings.is_empty() && self.errors.is_empty()
    }

    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }

    pub(crate) fn extend_validation(&mut self, validation: ValidationDiagnostics) {
        self.warnings.extend(validation.warnings);
        self.errors.extend(validation.errors);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ValidationDiagnostics {
    pub warnings: Vec<ConfigWarning>,
    pub errors: Vec<ConfigValidationError>,
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigWarning {
    DeprecatedKey {
        path: String,
        message: String,
    },
    InvalidValue {
        toml_path: String,
        message: String,
    },
    AiCompletionRankingTimeoutLikelyToTimeout {
        toml_path: String,
        provider: crate::AiProviderKind,
        timeout_ms: u64,
        message: String,
    },
    AiCompletionRankingCacheDisabled {
        toml_path: String,
        provider: crate::AiProviderKind,
        message: String,
    },
    ExtensionsWasmPathMissing {
        toml_path: String,
        resolved: PathBuf,
    },
    ExtensionsWasmPathNotDirectory {
        toml_path: String,
        resolved: PathBuf,
    },
    GeneratedSourcesOverrideRootsEmpty,
    LoggingLevelInvalid {
        value: String,
        normalized: String,
    },
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigValidationError {
    AiMissingApiKey {
        provider: crate::AiProviderKind,
    },
    InvalidValue {
        toml_path: String,
        message: String,
    },
    AiMissingAzureDeployment,
    AiMissingInProcessConfig,
    AiLocalOnlyForbidsCloudProvider {
        provider: crate::AiProviderKind,
    },
    AiLocalOnlyUrlNotLocal {
        provider: crate::AiProviderKind,
        url: String,
    },
    AiConcurrencyZero,
    AiCacheMaxEntriesZero,
    AiCacheTtlZero,
}

pub(crate) fn deserialize_toml_with_unknown_keys<T: DeserializeOwned>(
    text: &str,
) -> Result<(T, Vec<String>), toml::de::Error> {
    let mut unknown = Vec::<String>::new();
    let deserializer = toml::de::Deserializer::new(text);
    let value = serde_ignored::deserialize(deserializer, |path| {
        unknown.push(normalize_serde_ignored_path(path));
    })?;
    unknown.sort();
    unknown.dedup();
    Ok((value, unknown))
}

fn normalize_serde_ignored_path(path: serde_ignored::Path) -> String {
    // `serde_ignored::Path` renders with a leading `.` for root paths; `nova.toml` users expect
    // `a.b.c` style paths.
    let raw = path.to_string();
    let raw = raw.trim_start_matches('.');
    // `serde_ignored` renders sequence indices as `.0` segments. TOML users expect `a[0].b`.
    raw.split('.')
        .enumerate()
        .fold(String::new(), |mut out, (idx, segment)| {
            let is_index =
                idx > 0 && !segment.is_empty() && segment.bytes().all(|b| b.is_ascii_digit());
            if is_index {
                out.push('[');
                out.push_str(segment);
                out.push(']');
                return out;
            }

            if !out.is_empty() {
                out.push('.');
            }
            out.push_str(segment);
            out
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[test]
    fn unknown_key_paths_include_array_indexes() {
        #[derive(Debug, Deserialize)]
        struct Root {
            #[allow(dead_code)]
            items: Vec<Item>,
        }

        #[derive(Debug, Deserialize)]
        struct Item {
            #[allow(dead_code)]
            known: String,
        }

        let text = r#"
[[items]]
known = "ok"
typo = 1
"#;

        let (_value, unknown) = deserialize_toml_with_unknown_keys::<Root>(text).expect("parse");
        assert_eq!(unknown, vec!["items[0].typo"]);
    }
}
