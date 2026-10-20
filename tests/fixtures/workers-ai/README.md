Workers AI Clef fixtures, recorded from live calls to
`POST /client/v4/accounts/{id}/ai/run/@cf/cloudflare/{clef,clef-flash}` on 2026-10-20.
Account ids and tokens are not recorded.

- `clef.request.json`: the Cloudflare doc example, sent to `clef-flash`.
- `clef.response.json`: the real response (Workers AI envelope around `{model, answers, usage}`).
  `result.model` is the body selector (`clef-flash`), not the `@cf/...` path id.
- `model_mismatch.422.json`: path `.../clef` with body `"model": "clef-flash"` (HTTP 422, code 3030).
- `too_many_tokens.422.json`: string state of about 27k tokens to `clef-flash` (HTTP 422, code 3030).
  Clef did not truncate silently; it rejected the request.
- `context_estimate.413.json`: a 263,080-byte body (estimate = bytes / 4 = 65,770 tokens)
  to `clef-flash` (HTTP 413, code 5021).
- `extra_field.422.json`: an unknown top-level field (`assets` is rejected the same way) (HTTP 422).
- `invalid_token.401.json`: `Authorization: Bearer invalid` (HTTP 401, code 10000).
- `large_image.response.json`: a 325,096-byte body with one 243,691-byte random-noise JPEG
  (data URL in `images`) to `clef-flash` (HTTP 200). The `bytes / 4` pre-check that gave
  `context_estimate.413.json` for a 263,080-byte text body does not count image data.
  The request is not recorded (size).
- `missing_instructions.400.json`: a noul question without `instructions` to `clef-flash`
  (HTTP 400, code 5006). `null` gives the same. The message names the top-level required
  properties, not the question.
- `blank_instructions.422.json`: `"instructions": "  "` (HTTP 422, code 3030). Numbers,
  booleans, `[]`, and `{}` were accepted (HTTP 200).
- `question_name.422.json`: question id `has space` (HTTP 422, code 3030; pattern
  `^[A-Za-z0-9_.-]{1,100}$`).
