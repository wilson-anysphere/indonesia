use semver::Version;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer};
use std::collections::BTreeSet;
use std::path::PathBuf;
use toml::Value;

pub const MANIFEST_FILE_NAME: &str = "nova-ext.toml";
pub const SUPPORTED_ABI_VERSION: u32 = 1;

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ExtensionManifest {
    pub id: String,
    #[serde(deserialize_with = "deserialize_semver")]
    pub version: Version,
    pub entry: PathBuf,
    pub abi_version: u32,
    #[serde(deserialize_with = "deserialize_capabilities")]
    pub capabilities: Vec<ExtensionCapability>,

    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub authors: Vec<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub config_schema: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ExtensionCapability {
    Diagnostics,
    Completion,
    CodeAction,
    Navigation,
    InlayHint,
}

impl ExtensionCapability {
    pub fn as_str(self) -> &'static str {
        match self {
            ExtensionCapability::Diagnostics => "diagnostics",
            ExtensionCapability::Completion => "completion",
            ExtensionCapability::CodeAction => "code_action",
            ExtensionCapability::Navigation => "navigation",
            ExtensionCapability::InlayHint => "inlay_hint",
        }
    }
}

impl std::str::FromStr for ExtensionCapability {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let normalized = s.trim().to_ascii_lowercase().replace('-', "_");
        match normalized.as_str() {
            "diagnostic" | "diagnostics" => Ok(ExtensionCapability::Diagnostics),
            "completion" | "completions" => Ok(ExtensionCapability::Completion),
            "code_action" | "code_actions" | "codeaction" | "codeactions" => Ok(ExtensionCapability::CodeAction),
            "navigation" | "navigations" => Ok(ExtensionCapability::Navigation),
            "inlay_hint" | "inlay_hints" | "inlayhint" | "inlayhints" => Ok(ExtensionCapability::InlayHint),
            _ => Err(format!("unknown capability '{s}'")),
        }
    }
}

fn deserialize_semver<'de, D>(deserializer: D) -> Result<Version, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    Version::parse(&raw).map_err(|e| D::Error::custom(format!("invalid semver version '{raw}': {e}")))
}

fn deserialize_capabilities<'de, D>(deserializer: D) -> Result<Vec<ExtensionCapability>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum RawCapabilities {
        List(Vec<String>),
        Name(String),
    }

    let raw = RawCapabilities::deserialize(deserializer)?;
    let names: Vec<String> = match raw {
        RawCapabilities::List(list) => list,
        RawCapabilities::Name(name) => match name.trim().to_ascii_lowercase().as_str() {
            "all" => vec![
                "diagnostics".to_string(),
                "completion".to_string(),
                "code_action".to_string(),
                "navigation".to_string(),
                "inlay_hint".to_string(),
            ],
            other => vec![other.to_string()],
        },
    };

    let mut out = BTreeSet::<ExtensionCapability>::new();
    for name in names {
        let cap = name.parse::<ExtensionCapability>().map_err(D::Error::custom)?;
        out.insert(cap);
    }

    if out.is_empty() {
        return Err(D::Error::custom("capabilities must not be empty"));
    }

    Ok(out.into_iter().collect())
}
