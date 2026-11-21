# UI prototype

Clickable design prototype of the planned web UI, on sample data (not wired to the engine):
dashboards with a shared crosshair, an Add panel drawer and an Edit layout mode (drag to move,
drag the corner to resize on a 12-column × row grid, keyboard arrows on both handles), Explore (a pill query bar with a **Builder | TQS** switch that
edits the same query both ways), trace search with a waterfall, and log lines/patterns.

Open `index.html` in a browser. Fonts: Geist + Geist Mono. Chart colors follow the validated
categorical palette; status colors (good / warning / critical) are reserved for state.

Building it for real: each panel's place is `w` (columns, 1–12) and `h` (rows) in the dashboard
YAML, written back when a layout is saved; the panels map 1:1 to TQS documents (`schema/generated/tqs.schema.json`), the
Explore TQS editor becomes Monaco with that schema for completion, and every view reads the
existing HTTP API (`api/openapi.yaml`).
