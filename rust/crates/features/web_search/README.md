# charliebot-web-search

The web search an agent runs through the server: `POST /api/web-search/mcp`, an MCP streamable-HTTP endpoint with one
tool, `web_search`.

The endpoint backends (`cc-anthropic-compatible` and `cc-openai-compatible`) disallow Claude Code's built-in
`WebSearch`, because their endpoint either refuses it or answers without a search. Their session command mounts this
endpoint instead with `--mcp-config`, under the server name `charliebot-web`. The mount's header carries
`Bearer ${CHARLIEBOT_RUN_TOKEN}`, which Claude Code expands from the run's environment, so no token travels in an
argument. The endpoint sits under the server's access gate.

## The search

`tools/call` with a query runs one Google `generateContent` request on the model of the
`backends.calls.web_search` option, at that option's `api_base` with `models/<model>:generateContent` appended. The
request carries `tools: [{"google_search": {}}]` and authenticates with the key of the credential the option names.
The answer is the candidate's text, and the sources are `groundingMetadata.groundingChunks`.

Each chunk URI is a Google redirect. One GET that does not follow redirects reads its `Location`, and all of them run
in parallel.

The tool result is the answer, one `Search queries:` line, then `Sources:` with one `[n] <title> — <url>` line per
source. A URL that does not resolve keeps its redirect URL on the line with the failure text. An unset
`backends.calls.web_search` key, a credential the profile lacks, a non-200 answer, and an answer without a candidate
are each a tool error (`isError: true`) whose text names the key, the credential or the status code and the start of
the upstream body.

## The MCP surface

The endpoint answers `initialize`, `notifications/initialized`, `ping`, `tools/list` and `tools/call`. Every other
method answers JSON-RPC method-not-found with code `-32601`, and a notification gets an empty `202`. Claude Code
2.1.310 sends `server/discover` first and initializes after that refusal.
