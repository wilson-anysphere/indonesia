# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## Versioning policy

- The **single source of truth** for the Nova version is `Cargo.toml` (`[workspace.package].version`).
- Release tags are of the form `vMAJOR.MINOR.PATCH` and must match `Cargo.toml`.
- The VS Code extension version is kept in lockstep with the Nova version.

## [Unreleased]

- Initial release engineering scaffolding (cargo workspace, cargo-dist config, CI/workflows).
- Documentation: add an architecture-to-code crate map and a stable spec for Nova custom `nova/*`
  protocol extensions; update architecture reconciliation and surface the new docs from `README.md`.
- AI privacy: gate cloud code-editing (patch/apply) behind explicit opt-in flags and refuse edits
  when anonymization is enabled (patches cannot be applied reliably).
- AI completions: add a server startup override `NOVA_AI_COMPLETIONS_MAX_ITEMS` to cap (or disable
  with `0`) async multi-token completion items (used by the VS Code setting
  `nova.aiCompletions.maxItems`; restart required).
