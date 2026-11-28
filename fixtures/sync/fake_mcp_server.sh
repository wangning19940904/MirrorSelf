#!/bin/sh
# A synthetic stdio MCP server for tests. It answers from canned files and
# never touches the network.
#
#   FAKE_MCP_ERA     modern (2026-07-28, stateless) or legacy (initialize first)
#   FAKE_MCP_TOOLS   file holding the tools/list result
#   FAKE_MCP_RESULT  file holding the tools/call result
#   FAKE_MCP_LOG     file every incoming message is appended to
#   FAKE_MCP_TOKEN   when set, "token:ok" or "token:wrong" is logged
#
# Messages are one JSON object per line. The id and method are the first
# such keys in the top-level object, before any nested object.

era="${FAKE_MCP_ERA:-legacy}"
initialized=no

reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
fail() { printf '{"jsonrpc":"2.0","id":%s,"error":{"code":%s,"message":"%s"}}\n' "$1" "$2" "$3"; }

if [ -n "$FAKE_MCP_LOG" ] && [ -n "$FAKE_MCP_TOKEN" ]; then
  if [ "$FAKE_MCP_TOKEN" = "synthetic-token-not-real" ]; then
    echo "token:ok" >> "$FAKE_MCP_LOG"
  else
    echo "token:wrong" >> "$FAKE_MCP_LOG"
  fi
fi
echo "fake MCP server started ($era)" >&2

while IFS= read -r line || [ -n "$line" ]; do
  [ -n "$FAKE_MCP_LOG" ] && printf '%s\n' "$line" >> "$FAKE_MCP_LOG"
  id=$(printf '%s' "$line" | sed -n 's/^{[^{]*"id":\([0-9][0-9]*\).*/\1/p')
  method=$(printf '%s' "$line" | sed -n 's/^{[^{]*"method":"\([^"]*\)".*/\1/p')
  [ -z "$id" ] && continue   # a notification
  if [ "$era" = modern ]; then
    case "$line" in
      *'"io.modelcontextprotocol/protocolVersion":"2026-07-28"'*) ;;
      *) fail "$id" -32022 "Unsupported protocol version"; continue ;;
    esac
    case "$method" in
      server/discover) reply "$id" '{"supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"0"}}' ;;
      tools/list) reply "$id" "$(tr -d '\n' < "$FAKE_MCP_TOOLS")" ;;
      tools/call) reply "$id" "$(tr -d '\n' < "$FAKE_MCP_RESULT")" ;;
      *) fail "$id" -32601 "Method not found" ;;
    esac
  else
    case "$method" in
      initialize)
        initialized=yes
        reply "$id" '{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"0"}}' ;;
      tools/list|tools/call)
        if [ "$initialized" != yes ]; then fail "$id" -32002 "Server not initialized"; continue; fi
        if [ "$method" = tools/list ]; then reply "$id" "$(tr -d '\n' < "$FAKE_MCP_TOOLS")"
        else reply "$id" "$(tr -d '\n' < "$FAKE_MCP_RESULT")"; fi ;;
      *) fail "$id" -32601 "Method not found" ;;
    esac
  fi
done
