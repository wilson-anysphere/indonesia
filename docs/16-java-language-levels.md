# 16 - Java Language Levels and Feature Gating

[← Back to Main Document](../AGENTS.md) | [Previous: Work Breakdown](15-work-breakdown.md)

## Overview

Nova must behave like `javac` and IDEs: the *same source text* can be legal or illegal depending on the project's configured **Java language level** and whether **preview features** are enabled. This is a cross-cutting concern that must be represented *once* (canonical model) and threaded through:

- project/build detection (per module)
- parsing + syntax diagnostics (feature gates)
- semantic analysis + type checking (versioned rules)
- LSP configuration and dynamic updates
- tests (version matrix + preview toggles)

This document defines a single language-level model and how it integrates end-to-end.

---

## 1) Canonical language level model

### The representation

Nova should represent the effective Java language mode with a single type (name bikesheddable; `JavaLanguageLevel` used here):

```rust
/// The effective Java language mode for a module/file.
/// 
/// - `major`: the Java feature release number (8, 11, 17, 21, 22, …)
/// - `preview`: whether `--enable-preview` is in effect for this major version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JavaLanguageLevel {
  pub major: u16,
  pub preview: bool,
}

impl JavaLanguageLevel {
  pub const JAVA_8: Self  = Self { major: 8,  preview: false };
  pub const JAVA_11: Self = Self { major: 11, preview: false };
  pub const JAVA_17: Self = Self { major: 17, preview: false };
  pub const JAVA_21: Self = Self { major: 21, preview: false };

  pub fn with_preview(self, preview: bool) -> Self {
      Self { preview, ..self }
  }
}
```

Notes:
- A `major >= 22` value must be accepted (future-proofing). Feature gates should typically use `>=` comparisons, and only use exact version ranges when a feature is preview in some versions and final in later ones.
- The language level is *per module*, but most queries operate per file; treat the per-file level as a derived query from the file’s owning module.

### Feature support API

Many components need to ask “is feature X enabled?”. Avoid sprinkling version numbers everywhere—encode them once.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JavaFeature {
  Modules,                 // Java 9+
  VarLocalInference,       // Java 10+
  SwitchExpressions,       // final Java 14 (preview earlier)
  TextBlocks,              // final Java 15
  Records,                 // final Java 16 (preview 14/15)
  SealedClasses,           // final Java 17 (preview 15/16)
  PatternMatchingSwitch,   // final Java 21 (preview 17-20)
  RecordPatterns,          // final Java 21 (preview earlier)
  UnnamedVariables,        // Java 22+ (preview in 21)
  StringTemplates,         // Java 21+ (preview)
  // Extend as needed.
}

/// Whether the *language* supports a feature in this major version,
/// independent of whether preview is enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeatureAvailability {
  Unavailable,
  Preview,
  Stable,
}

impl JavaLanguageLevel {
  pub fn availability(self, feature: JavaFeature) -> FeatureAvailability {
      use FeatureAvailability::*;
      use JavaFeature::*;
      match feature {
          Modules => if self.major >= 9 { Stable } else { Unavailable },

          VarLocalInference => if self.major >= 10 { Stable } else { Unavailable },

          SwitchExpressions => if self.major >= 14 { Stable } else { Unavailable },

          TextBlocks => if self.major >= 15 { Stable } else { Unavailable },

          Records => if self.major >= 16 { Stable }
              else if self.major == 14 || self.major == 15 { Preview }
              else { Unavailable },

          SealedClasses => if self.major >= 17 { Stable }
              else if self.major == 15 || self.major == 16 { Preview }
              else { Unavailable },

          PatternMatchingSwitch => if self.major >= 21 { Stable }
              else if (17..=20).contains(&self.major) { Preview }
              else { Unavailable },

          RecordPatterns => if self.major >= 21 { Stable }
              else if (19..=20).contains(&self.major) { Preview }
              else { Unavailable },

          UnnamedVariables => if self.major >= 22 { Stable }
              else if self.major == 21 { Preview }
              else { Unavailable },

          StringTemplates => if self.major >= 21 { Preview } else { Unavailable },
      }
  }

  /// Is the feature usable in this configuration? (applies `preview` flag)
  pub fn is_enabled(self, feature: JavaFeature) -> bool {
      match self.availability(feature) {
          FeatureAvailability::Stable => true,
          FeatureAvailability::Preview => self.preview,
          FeatureAvailability::Unavailable => false,
      }
  }

  // Convenience helpers (used heavily by parser/semantics).
  pub fn supports_var_local_inference(self) -> bool {
      self.is_enabled(JavaFeature::VarLocalInference)
  }
  pub fn supports_records(self) -> bool { self.is_enabled(JavaFeature::Records) }
  pub fn supports_sealed(self) -> bool { self.is_enabled(JavaFeature::SealedClasses) }
  pub fn supports_switch_expressions(self) -> bool { self.is_enabled(JavaFeature::SwitchExpressions) }
  pub fn supports_pattern_matching_switch(self) -> bool { self.is_enabled(JavaFeature::PatternMatchingSwitch) }
}
```

This structure also supports more informative diagnostics:
- `Unavailable` → “requires Java N”
- `Preview` with preview disabled → “requires --enable-preview”
- `Preview` with preview enabled → optional “preview feature” warning/hint

---

## 2) Determining language level per module

Language level is a property of a **module** (Maven module, Gradle subproject, Bazel target set), not of a file. Nova should compute an *effective* `JavaLanguageLevel` for every module via layered configuration:

**Priority (highest wins):**
1. `nova-config` explicit override (user intent)
2. build tool settings (Maven/Gradle/Bazel) (project intent)
3. fallback default (e.g., workspace default, or detected JDK used by build)

### Maven (pom.xml)

Infer from:
- `<properties>`
- `maven.compiler.release` (preferred)
- `maven.compiler.source` / `maven.compiler.target`
- `maven-compiler-plugin` configuration
- `<release>`, `<source>`, `<target>`

Parsing rules:
- Prefer `release` if set.
- Accept `"1.8"` as `8`.
- If only `source` is set, treat that as language level; `target` affects bytecode but also indicates compatibility expectations.

### Gradle (build.gradle / build.gradle.kts)

Best-effort inference from:
- `sourceCompatibility = "17"` or `JavaVersion.VERSION_17`
- toolchains:
```kotlin
java {
  toolchain {
    languageVersion.set(JavaLanguageVersion.of(21))
  }
}
```

For correctness at scale, Nova should prefer a real extracted model over regex parsing. Today Nova
starts with heuristics in `nova-project`, and can optionally refine language level/classpath data by
executing Gradle via `nova-build` and consuming the workspace-local snapshot
`.nova/queries/gradle.json` (see [`gradle-build-integration.md`](gradle-build-integration.md)).

### Bazel

Infer from extracted `javac` options (best effort):
- `--release`, `--source`, `--target`, `-source`, `-target`

This typically requires querying Bazel (`aquery`/`cquery`) to obtain the actual compilation actions and flags. If unavailable, fallback to workspace defaults.

### Explicit override via `nova-config`

`nova-config` should allow:
- a workspace default language level (applies when build tool unknown)
- per-module overrides (module path / Maven artifact / Gradle project path / Bazel label)
- preview enablement per module

Example (illustrative):

```json
{
"java": {
  "defaultLanguageLevel": { "major": 21, "preview": false },
  "modules": {
    ":legacy-module": { "major": 8, "preview": false },
    ":experiments":   { "major": 21, "preview": true }
  }
}
}
```

---

## 3) Parser + syntax diagnostics: feature gates

The parser should be able to build a tree for modern Java syntax even when the configured level is older, then emit *version-aware* diagnostics. This gives:
- better recovery (new syntax still forms coherent nodes)
- precise “feature not supported” diagnostics instead of cascaded parse errors

Recommended architecture:

1. **Parse a superset** of the grammar (Java 21 + selected newer previews).
2. Run a **feature-gate pass** over the produced syntax tree:
 - detect nodes/tokens that correspond to `JavaFeature`s
 - compare against `JavaLanguageLevel`
 - emit diagnostics with clear codes/messages

Diagnostics policy:
- `Unavailable` → error
- `Preview` and preview disabled → error (“requires --enable-preview”)
- `Preview` and preview enabled → optional warning/hint (“preview feature may change”)

Contextual / restricted identifiers:
- Some words are *restricted identifiers* in modern Java (`var`, `yield`, `record`, `sealed`, `permits`).
- These must be handled by the parser as context-sensitive tokens, not as always-reserved keywords.
- The feature-gate pass can attribute “feature usage” to a syntax node even when the token is still lexed as `Identifier` (by checking its text).

---

## 4) Semantic feature gating

Certain rules cannot be enforced at parse-time:
- module system behavior (requires classpath/module-path integration)
- record member synthesis (fields, canonical ctor)
- sealed type hierarchy checks
- pattern matching typing rules

Semantics should therefore:
- use the same `JavaLanguageLevel` per module/file
- apply versioned rules when lowering to HIR and during type checking
- emit diagnostics that match `javac` intent (“feature not supported in -source N” / “preview feature … is disabled”)

Example version gates in semantics:
- `module-info.java` present but `major < 9` → error “modules require Java 9+”
- record declaration with `major < 16` → error “records require Java 16+” (or “preview in 14/15” depending)
- pattern matching for switch with `major == 17` and preview off → error “preview feature requires --enable-preview”

---

## 5) LSP + workspace propagation

The workspace model should expose language level as first-class data:
- store `module → JavaLanguageLevel`
- store `file → module`
- derive `file → JavaLanguageLevel` for parse/diagnostics queries

LSP implications:
- `nova/projectConfiguration` should include language level per module (and ideally the *reason*: config override vs build-derived vs default)
- configuration changes (`workspace/didChangeConfiguration` or nova-specific config reload) must invalidate:
  - parse + syntax diagnostics for affected files
  - semantic queries (HIR/type checking) for affected files

---

## 6) Tests: version matrix fixtures

Nova needs tests that explicitly pin a language level and assert diagnostics.

A good pattern is “one source, many modes”:

- `JavaLanguageLevel::JAVA_11`: `record Point(int x, int y) {}` → error “records require Java 16+”
- `JavaLanguageLevel { major: 17, preview: false }`: pattern matching `switch` → error “requires --enable-preview”
- `JavaLanguageLevel { major: 17, preview: true }`: same file → no error (optionally a warning)
- `JavaLanguageLevel::JAVA_21`: same file → no error

Also include contextual keyword regression tests:
- `var` as a type name should behave differently at 8 vs 10+.
- `yield` should be legal as an identifier/label in modes without switch expressions.

---

[← Previous: Work Breakdown](15-work-breakdown.md) | [Back to Main Document →](../AGENTS.md)
