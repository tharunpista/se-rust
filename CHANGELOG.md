# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
- Team runs (`se team run`, `se team resume`, `/team`): a task graph of
  specialists, each on any vendor's model, with per-vendor limits, a run
  journal, and resume that never re-runs work of unknown outcome.
- Ensembles with an evaluator that reports every disagreement with checked
  evidence and never votes.
- Per-task file claims, a token budget with a reserved review share, a
  per-task approval rate limit, and per-task usage attribution.
- Native Anthropic API support; automatic specialist routing.
- Workspace path confinement for every tool; a durable audit log.

### Security
- The permission gateway fails closed: a decision that cannot be audited
  is a denial, and with no writable audit log tools are disabled.
