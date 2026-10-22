# Mail safety catalogue report

- Revision: `796c297dbc4ddde75a5bfc771a629d7524bc984a`
- Environment: Node v22.23.2 on darwin/arm64
- Requested lane: `all`
- Scenarios in report: 161
- Implemented source evidence: 109
- Gaps: 39
- Manual-required: 11
- Unsupported: 2
- Traceable inputs: 126 source tests, 9 checked-in fixtures, 11 manual fixtures, 15 not yet captured
- Outcome oracles: 251 concrete expected test oracles, 232 not-applicable with reasons, 0 still need refinement

This report validates catalogue declarations only. Outcome oracles are acceptance expectations, not proof that their named source test executed. It does not execute Rust, frontend, provider, native, or release tests; it must not be treated as a passing test result.

## P0 work not counted as a pass

- MS-K-015: A live Wry WebView processes actual AppHandle delivery without a stale listener
