TypeSafe System One fixtures, from the OpenAPI examples and live error probes
recorded in the provider notes (docs.typesafe.ai/api/system-one.md, 2026-10-20).

- `noul.request.json`: OpenAPI noul example, reformatted with extra whitespace
  so tests can check that bytes pass through unchanged.
- `noul.response.json`: OpenAPI noul response, with `model` set to the
  versioned id (`jev-latest` resolves to `jev-1.13.0`).
- `missing_key.403.json`: live body when no `Authorization` header is sent (HTTP 403).
- `invalid_key.401.json`: live body for `Authorization: Bearer invalid` (HTTP 401).
- `triage.response.json`: live Jev answer (2026-10-20) to the Clef doc example
  (noul + choice + score) sent as `typesafe/jev-latest` through
  `POST /v1/systemone`; recorded with `S1_LIVE=1 S1_RECORD=1 cargo test --test live -- --ignored`.
