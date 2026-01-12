//! Spring Boot configuration metadata indexing.
//!
//! Spring Boot publishes `META-INF/spring-configuration-metadata.json` (and
//! optionally `META-INF/additional-spring-configuration-metadata.json`) inside
//! dependency JARs. This crate ingests those JSON files and provides lookup APIs
//! used for completions and diagnostics.

use std::collections::BTreeMap;

use anyhow::Context;
use nova_archive::Archive;
use nova_classpath::ClasspathEntry;
use serde::Deserialize;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeprecationMeta {
    pub level: Option<String>,
    pub reason: Option<String>,
    pub replacement: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PropertyMeta {
    pub name: String,
    pub ty: Option<String>,
    pub description: Option<String>,
    pub default_value: Option<String>,
    pub deprecation: Option<DeprecationMeta>,
    pub allowed_values: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct MetadataIndex {
    properties: BTreeMap<String, PropertyMeta>,
}

impl MetadataIndex {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.properties.is_empty()
    }

    pub fn ingest_classpath(&mut self, classpath: &[ClasspathEntry]) -> anyhow::Result<()> {
        for entry in classpath {
            match entry {
                ClasspathEntry::Jar(path) | ClasspathEntry::ClassDir(path) => {
                    self.ingest_archive(&Archive::new(path))?;
                }
                ClasspathEntry::Jmod(_) => {}
            }
        }
        Ok(())
    }

    pub fn ingest_archive(&mut self, archive: &Archive) -> anyhow::Result<()> {
        const PATHS: [&str; 2] = [
            "META-INF/spring-configuration-metadata.json",
            "META-INF/additional-spring-configuration-metadata.json",
        ];

        for path in PATHS {
            let Some(bytes) = archive.read(path)? else {
                continue;
            };
            self.ingest_json_bytes(&bytes).with_context(|| {
                format!("while ingesting {} from {}", path, archive.path().display())
            })?;
        }

        Ok(())
    }

    pub fn ingest_json_bytes(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        let raw: RawMetadata = serde_json::from_slice(bytes)
            .with_context(|| "failed to parse Spring configuration metadata JSON")?;

        for prop in raw.properties.unwrap_or_default() {
            self.insert_property(prop);
        }

        // Hints provide allowed values.
        for hint in raw.hints.unwrap_or_default() {
            if let Some(values) = hint.values {
                let allowed_values: Vec<String> = values
                    .into_iter()
                    .map(|v| v.value.to_string().trim_matches('"').to_string())
                    .collect();
                self.properties
                    .entry(hint.name.clone())
                    .and_modify(|meta| merge_allowed_values(meta, &allowed_values))
                    .or_insert_with(|| PropertyMeta {
                        name: hint.name,
                        ty: None,
                        description: None,
                        default_value: None,
                        deprecation: None,
                        allowed_values,
                    });
            }
        }

        Ok(())
    }

    fn insert_property(&mut self, prop: RawProperty) {
        let default_value = prop
            .default_value
            .map(|v| v.to_string().trim_matches('"').to_string());
        let deprecation = prop.deprecation.map(|d| DeprecationMeta {
            level: d.level,
            reason: d.reason,
            replacement: d.replacement,
        });

        self.properties
            .entry(prop.name.clone())
            .and_modify(|existing| {
                if existing.ty.is_none() {
                    existing.ty = prop.ty.clone();
                }
                if existing.description.is_none() {
                    existing.description = prop.description.clone();
                }
                if existing.default_value.is_none() {
                    existing.default_value = default_value.clone();
                }
                if existing.deprecation.is_none() {
                    existing.deprecation = deprecation.clone();
                }
            })
            .or_insert_with(|| PropertyMeta {
                name: prop.name,
                ty: prop.ty,
                description: prop.description,
                default_value,
                deprecation,
                allowed_values: Vec::new(),
            });
    }

    #[must_use]
    pub fn property_meta(&self, key: &str) -> Option<&PropertyMeta> {
        self.properties.get(key)
    }

    /// Iterate over all known properties that begin with `prefix`.
    ///
    /// This uses an ordered map internally, so prefix queries are efficient and
    /// stable without allocating/cloning the entire metadata set.
    pub fn known_properties<'a>(
        &'a self,
        prefix: &'a str,
    ) -> impl Iterator<Item = &'a PropertyMeta> + 'a {
        let start = prefix.to_string();
        self.properties
            .range(start..)
            .take_while(move |(name, _)| name.starts_with(prefix))
            .map(|(_, meta)| meta)
    }
}

fn merge_allowed_values(target: &mut PropertyMeta, values: &[String]) {
    for value in values {
        if !target.allowed_values.contains(value) {
            target.allowed_values.push(value.clone());
        }
    }
}

#[derive(Deserialize)]
struct RawMetadata {
    properties: Option<Vec<RawProperty>>,
    hints: Option<Vec<RawHint>>,
}

#[derive(Deserialize)]
struct RawProperty {
    name: String,
    #[serde(rename = "type")]
    ty: Option<String>,
    description: Option<String>,
    #[serde(rename = "defaultValue")]
    default_value: Option<serde_json::Value>,
    deprecation: Option<RawDeprecation>,
}

#[derive(Deserialize)]
struct RawDeprecation {
    level: Option<String>,
    reason: Option<String>,
    replacement: Option<String>,
}

#[derive(Deserialize)]
struct RawHint {
    name: String,
    values: Option<Vec<RawHintValue>>,
}

#[derive(Deserialize)]
struct RawHintValue {
    value: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::io::Write;
    use tempfile::tempdir;
    use zip::write::FileOptions;

    #[test]
    fn ingests_metadata_from_jar() {
        let dir = tempdir().unwrap();
        let jar_path = dir.path().join("test.jar");

        let mut jar = zip::ZipWriter::new(std::fs::File::create(&jar_path).unwrap());
        let options = FileOptions::<()>::default();
        jar.start_file("META-INF/spring-configuration-metadata.json", options)
            .unwrap();
        write!(
            jar,
            r#"{{
              "properties": [
                {{
                  "name": "server.port",
                  "type": "java.lang.Integer",
                  "description": "Server HTTP port",
                  "defaultValue": 8080
                }},
                {{
                  "name": "spring.main.banner-mode",
                  "type": "java.lang.String",
                  "deprecation": {{ "level": "warning", "replacement": "spring.main.banner-mode2" }}
                }}
              ],
              "hints": [
                {{
                  "name": "spring.main.banner-mode",
                  "values": [ {{ "value": "off" }}, {{ "value": "console" }} ]
                }}
              ]
            }}"#
        )
        .unwrap();

        jar.start_file(
            "META-INF/additional-spring-configuration-metadata.json",
            options,
        )
        .unwrap();
        write!(
            jar,
            r#"{{
              "properties": [
                {{
                  "name": "acme.feature.enabled",
                  "type": "java.lang.Boolean",
                  "description": "Turns on the Acme feature",
                  "defaultValue": true
                }}
              ]
            }}"#
        )
        .unwrap();
        jar.finish().unwrap();

        let mut index = MetadataIndex::new();
        index
            .ingest_classpath(&[ClasspathEntry::Jar(jar_path)])
            .unwrap();

        let server_port = index.property_meta("server.port").unwrap();
        assert_eq!(server_port.ty.as_deref(), Some("java.lang.Integer"));
        assert_eq!(server_port.default_value.as_deref(), Some("8080"));

        let banner_mode = index.property_meta("spring.main.banner-mode").unwrap();
        assert_eq!(banner_mode.allowed_values, vec!["off", "console"]);
        assert!(banner_mode.deprecation.is_some());

        let acme_enabled = index.property_meta("acme.feature.enabled").unwrap();
        assert_eq!(acme_enabled.ty.as_deref(), Some("java.lang.Boolean"));
        assert_eq!(acme_enabled.default_value.as_deref(), Some("true"));
    }

    #[test]
    fn ingests_metadata_from_directory_classpath_entry() {
        let dir = tempdir().unwrap();
        let meta_dir = dir.path().join("META-INF");
        std::fs::create_dir_all(&meta_dir).unwrap();
        std::fs::write(
            meta_dir.join("spring-configuration-metadata.json"),
            br#"{
              "properties": [
                {
                  "name": "server.port",
                  "type": "java.lang.Integer",
                  "defaultValue": 8080
                }
              ]
            }"#,
        )
        .unwrap();

        let mut index = MetadataIndex::new();
        index
            .ingest_classpath(&[ClasspathEntry::ClassDir(dir.path().to_path_buf())])
            .unwrap();

        let server_port = index.property_meta("server.port").unwrap();
        assert_eq!(server_port.ty.as_deref(), Some("java.lang.Integer"));
        assert_eq!(server_port.default_value.as_deref(), Some("8080"));
    }

    #[test]
    fn ingest_classpath_ignores_missing_archives() {
        let dir = tempdir().unwrap();

        let jar_path = dir.path().join("metadata.jar");
        let mut jar = zip::ZipWriter::new(std::fs::File::create(&jar_path).unwrap());
        let options = FileOptions::<()>::default();
        jar.start_file("META-INF/spring-configuration-metadata.json", options)
            .unwrap();
        jar.write_all(
            br#"{
              "properties": [
                {
                  "name": "server.port",
                  "type": "java.lang.Integer",
                  "defaultValue": 8080
                }
              ]
            }"#,
        )
        .unwrap();
        jar.finish().unwrap();

        let missing = dir.path().join("does-not-exist.jar");

        let mut index = MetadataIndex::new();
        index
            .ingest_classpath(&[ClasspathEntry::Jar(missing), ClasspathEntry::Jar(jar_path)])
            .unwrap();

        assert!(
            index.property_meta("server.port").is_some(),
            "expected metadata ingestion to ignore missing archives and still ingest metadata from existing jars"
        );
    }
}
