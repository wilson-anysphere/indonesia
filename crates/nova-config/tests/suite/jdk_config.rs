use std::path::Path;

use nova_config::NovaConfig;

#[test]
fn parses_jdk_toolchains_from_toml() {
    let cfg: NovaConfig = toml::from_str(
        r#"
[jdk]
home = "jdks/default"

[[jdk.toolchains]]
release = 8
home = "jdks/jdk8"

[[jdk.toolchains]]
release = 17
home = "jdks/jdk17"
"#,
    )
    .expect("TOML should parse");

    let jdk_cfg = cfg.jdk_config();
    assert_eq!(jdk_cfg.home.as_deref(), Some(Path::new("jdks/default")));
    assert_eq!(jdk_cfg.toolchains.len(), 2);
    assert_eq!(
        jdk_cfg.toolchains.get(&8).map(|p| p.as_path()),
        Some(Path::new("jdks/jdk8"))
    );
    assert_eq!(
        jdk_cfg.toolchains.get(&17).map(|p| p.as_path()),
        Some(Path::new("jdks/jdk17"))
    );
}

#[test]
fn duplicate_jdk_toolchains_last_wins() {
    let cfg: NovaConfig = toml::from_str(
        r#"
[jdk]

[[jdk.toolchains]]
release = 17
home = "jdks/jdk17-first"

[[jdk.toolchains]]
release = 17
home = "jdks/jdk17-second"
"#,
    )
    .expect("TOML should parse");

    let jdk_cfg = cfg.jdk_config();
    assert_eq!(jdk_cfg.toolchains.len(), 1);
    assert_eq!(
        jdk_cfg.toolchains.get(&17).map(|p| p.as_path()),
        Some(Path::new("jdks/jdk17-second"))
    );
}

#[test]
fn jdk_home_alias_still_parses() {
    let cfg: NovaConfig = toml::from_str(
        r#"
[jdk]
jdk_home = "jdks/legacy"
"#,
    )
    .expect("legacy TOML should parse");

    assert_eq!(cfg.jdk.home.as_deref(), Some(Path::new("jdks/legacy")));

    let jdk_cfg = cfg.jdk_config();
    assert_eq!(jdk_cfg.home.as_deref(), Some(Path::new("jdks/legacy")));
    assert!(jdk_cfg.toolchains.is_empty());
}
