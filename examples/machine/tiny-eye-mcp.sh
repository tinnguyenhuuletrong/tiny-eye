#!/bin/sh
# Starts tiny-eye's MCP bridge (stdio) against the node on TINY_EYE_URL. Used by .mcp.json, so an
# assistant opened in this folder gets the discover_telemetry_catalog, validate_tqs and run_tqs tools.
here=$(cd "$(dirname "$0")" && pwd)
exec "$here/../../target/release/tiny-eye" mcp --url "${TINY_EYE_URL:-http://127.0.0.1:7070}"
