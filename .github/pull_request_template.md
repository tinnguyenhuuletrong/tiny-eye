## Summary

<!-- What does this change do? -->

## Why

<!-- The problem it solves, or the issue it closes (Closes #123). -->

## How it was tested

<!-- New or changed tests. Every semantic rule needs a test that can fail. -->

- [ ] `make check` passes locally (lint, test incl. e2e vs Prometheus and schema sync, conformance)

## Checklist

- [ ] Docs updated in `docs/` (or not needed)
- [ ] Sources of truth edited, not generated output (`api/openapi.yaml`, `schema/tables.yaml`, `config/default.yaml`, `dashboards/*.yaml`)
- [ ] Generated files regenerated with `make schemas` (or not needed)
- [ ] `CHANGELOG.md` updated under `[Unreleased]` for user-visible changes
