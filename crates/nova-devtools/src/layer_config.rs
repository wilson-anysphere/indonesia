use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{anyhow, Context as _};
use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct LayerMapConfig {
    #[serde(default)]
    pub version: Option<u32>,

    pub layers: BTreeMap<String, i32>,
    pub crates: BTreeMap<String, String>,

    #[serde(default)]
    pub policy: PolicyConfig,
}

#[derive(Debug, Default, Deserialize, Clone)]
pub struct PolicyConfig {
    #[serde(default = "default_allow_same_layer")]
    pub allow_same_layer: bool,

    #[serde(default)]
    pub dev: DevPolicyConfig,
}

fn default_allow_same_layer() -> bool {
    true
}

#[derive(Debug, Default, Deserialize, Clone)]
pub struct DevPolicyConfig {
    /// Whether dev-dependencies are allowed to point "up" the layer stack (lower → higher).
    ///
    /// This is convenient for integration-style tests living in lower-layer crates.
    #[serde(default)]
    pub allow_upward: bool,

    /// Layer names that are forbidden targets for upward dev-dependencies, unless allowlisted.
    ///
    /// The default policy in this repo is to avoid dragging protocol/server crates into lower
    /// layers even in tests.
    #[serde(default)]
    pub forbid_upward_to: Vec<String>,

    #[serde(default)]
    pub allowlist: Vec<AllowlistedDevEdge>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AllowlistedDevEdge {
    pub from: String,
    pub to: String,
}

pub fn load_config(path: &Path) -> anyhow::Result<LayerMapConfig> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    parse_config(&raw).with_context(|| format!("failed to parse {}", path.display()))
}

pub fn parse_config(raw: &str) -> anyhow::Result<LayerMapConfig> {
    let config: LayerMapConfig = toml::from_str(raw)
        .map_err(|err| anyhow::Error::msg(sanitize_toml_error_message(err.message())))?;

    if let Some(version) = config.version {
        if version != 1 {
            return Err(anyhow!(
                "unsupported crate-layers.toml version {version}; expected 1"
            ));
        }
    }

    // Validate layer names referenced by crates.
    for (krate, layer) in &config.crates {
        if !config.layers.contains_key(layer) {
            return Err(anyhow!("crate {krate} references unknown layer {layer}"));
        }
    }

    for layer in &config.policy.dev.forbid_upward_to {
        if !config.layers.contains_key(layer) {
            return Err(anyhow!(
                "policy.dev.forbid_upward_to references unknown layer {layer}"
            ));
        }
    }

    for allow in &config.policy.dev.allowlist {
        if !config.crates.contains_key(&allow.from) {
            return Err(anyhow!(
                "policy.dev.allowlist refers to unknown crate {} (from)",
                allow.from
            ));
        }
        if !config.crates.contains_key(&allow.to) {
            return Err(anyhow!(
                "policy.dev.allowlist refers to unknown crate {} (to)",
                allow.to
            ));
        }
    }

    Ok(config)
}

fn sanitize_toml_error_message(message: &str) -> String {
    fn redact_quoted(message: &str, quote: char) -> String {
        const REDACTED: &str = "<redacted>";
        let mut out = String::with_capacity(message.len());
        let mut rest = message;
        while let Some(start) = rest.find(quote) {
            out.push_str(&rest[..start]);
            let quote_len = quote.len_utf8();
            out.push(quote);
            let Some(after_open) = rest.get(start + quote_len..) else {
                // Unterminated quote: redact the remainder and stop.
                out.push_str(REDACTED);
                return out;
            };

            let quote_byte = quote as u8;
            let bytes = after_open.as_bytes();
            let mut end = None;
            for (idx, &b) in bytes.iter().enumerate() {
                if b != quote_byte {
                    continue;
                }

                // Treat quotes preceded by an odd number of backslashes as escaped.
                let mut backslashes = 0usize;
                let mut k = idx;
                while k > 0 && bytes[k - 1] == b'\\' {
                    backslashes += 1;
                    k -= 1;
                }
                if backslashes % 2 == 0 {
                    end = Some(idx);
                    break;
                }
            }

            let Some(end) = end else {
                // Unterminated quote: redact the remainder and stop.
                out.push_str(REDACTED);
                return out;
            };

            out.push_str(REDACTED);
            out.push(quote);
            let Some(after_close) = after_open.get(end + quote_len..) else {
                return out;
            };
            rest = after_close;
        }
        out.push_str(rest);
        out
    }

    // `toml::de::Error::message()` can still include user-provided scalar values in quotes, for
    // example `invalid type: string "secret", expected u32`. `crate-layers.toml` can be
    // repository-local or user-authored; avoid echoing arbitrary config contents in error
    // messages.
    let mut out = redact_quoted(message, '"');
    out = redact_quoted(&out, '\'');

    // `serde` uses backticks in a few different diagnostics:
    //
    // - `unknown field `secret`, expected ...` (user-controlled key → redact)
    // - `unknown variant `secret`, expected ...` (user-controlled variant → redact)
    // - `invalid type: integer `123`, expected ...` (user-controlled scalar → redact)
    // - `missing field `foo`` (schema field name → keep)
    //
    // Redact only when the backticked segment is known to contain user-controlled content.
    let mut start = ["unknown field `", "unknown variant `"]
        .iter()
        .filter_map(|pattern| {
            out.find(pattern)
                .map(|pos| pos + pattern.len().saturating_sub(1))
        })
        .min();
    if start.is_none() && (out.contains("invalid type:") || out.contains("invalid value:")) {
        // `invalid type/value` errors include the unexpected scalar value before `, expected ...`.
        // Redact only backticked values in that prefix so we don't hide schema names in the
        // expected portion.
        let boundary = out.find(", expected").unwrap_or(out.len());
        start = out[..boundary].find('`');
        if start.is_none() && boundary == out.len() {
            // Some serde errors omit the `, expected ...` suffix. Fall back to the first backtick.
            start = out.find('`');
        }
    }
    if let Some(start) = start {
        let after_start = &out[start.saturating_add(1)..];
        let end = if let Some(end_rel) = after_start.rfind("`, expected") {
            Some(start.saturating_add(1).saturating_add(end_rel))
        } else if let Some(end_rel) = after_start.rfind('`') {
            Some(start.saturating_add(1).saturating_add(end_rel))
        } else {
            None
        };
        if let Some(end) = end {
            if start + 1 <= end && end <= out.len() {
                out.replace_range(start + 1..end, "<redacted>");
            }
        }
    }

    out
}

impl LayerMapConfig {
    pub fn layer_rank(&self, layer: &str) -> Option<i32> {
        self.layers.get(layer).copied()
    }

    pub fn layer_for_rank(&self, rank: i32) -> Option<&str> {
        self.layers
            .iter()
            .find_map(|(name, value)| (*value == rank).then_some(name.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_layer_references() {
        let err = parse_config(
            r#"
version = 1
[layers]
core = 0
[crates]
a = "missing"
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("references unknown layer"));
    }

    #[test]
    fn rejects_unknown_allowlist_crates() {
        let err = parse_config(
            r#"
version = 1
[layers]
core = 0
protocol = 1
[crates]
a = "core"

[policy.dev]
allow_upward = true
forbid_upward_to = ["protocol"]

[[policy.dev.allowlist]]
from = "a"
to = "missing"
"#,
        )
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("policy.dev.allowlist refers to unknown crate"));
    }

    #[test]
    fn rejects_unsupported_versions() {
        let err = parse_config(
            r#"
version = 2
[layers]
core = 0
[crates]
a = "core"
"#,
        )
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("unsupported crate-layers.toml version"));
    }

    #[test]
    fn toml_parse_errors_do_not_echo_string_values() {
        let secret_suffix = "nova-devtools-layer-config-secret";
        let secret = format!("prefix\\\"{secret_suffix}");
        let text = format!(
            r#"
version = "{secret}"
[layers]
core = 0
[crates]
a = "core"
"#
        );

        let raw_err =
            toml::from_str::<LayerMapConfig>(&text).expect_err("expected invalid type error");
        let raw_message = raw_err.message();
        assert!(
            raw_message.contains(secret_suffix),
            "expected raw toml error message to include the string value so this test catches leaks: {raw_message}"
        );

        let err = parse_config(&text).expect_err("expected parse_config error");
        let message = err.to_string();
        assert!(
            !message.contains(secret_suffix),
            "expected sanitized toml error message to omit string values: {message}"
        );
        assert!(
            message.contains("<redacted>"),
            "expected sanitized toml error message to include redaction marker: {message}"
        );
    }

    #[test]
    fn sanitize_toml_error_message_redacts_backticked_numeric_values() {
        #[derive(Debug, Deserialize)]
        struct Dummy {
            #[allow(dead_code)]
            flag: bool,
        }

        let raw_err =
            toml::from_str::<Dummy>("flag = 123").expect_err("expected invalid type error");
        let raw_message = raw_err.message();
        assert!(
            raw_message.contains("123"),
            "expected raw toml error message to include the numeric value so this test catches leaks: {raw_message}"
        );

        let sanitized = sanitize_toml_error_message(raw_message);
        assert!(
            !sanitized.contains("123"),
            "expected sanitized toml error message to omit numeric values: {sanitized}"
        );
        assert!(
            sanitized.contains("<redacted>"),
            "expected sanitized toml error message to include redaction marker: {sanitized}"
        );
    }
}
