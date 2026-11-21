# UI prototype

Clickable design prototype of the planned web UI, on sample data (not wired to the engine):
dashboards with a shared crosshair, Explore (a pill query bar with a **Builder | TQS** switch that
edits the same query both ways), trace search with a waterfall, and log lines/patterns.

Open `index.html` in a browser. Fonts: Geist + Geist Mono. Chart colors follow the validated
categorical palette; status colors (good / warning / critical) are reserved for state.

Building it for real: the panels map 1:1 to TQS documents (`doc/schema/tqs.schema.json`), the
Explore TQS editor becomes Monaco with that schema for completion, and every view reads the
existing HTTP API (`api/openapi.yaml`).
