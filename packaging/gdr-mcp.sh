#!/usr/bin/env bash
# System wrapper for the gdr MCP server (stdio).
exec node /usr/share/gdr/mcp-server/dist/index.js "$@"
