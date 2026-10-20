Workers AI Clairic fixtures, recorded from live calls to
`POST /client/v4/accounts/{id}/ai/run/@cf/cloudflare/clairic` on 2026-10-20.
Clairic is a preview API: these files pin its format, so a format change
shows up as a test failure. Account ids and tokens are not recorded.

Successes (Workers AI envelope `{result, success, errors, messages}` around
`{model, answers, truncated, usage}`; `result.model` is `clairic`):

- `invoice.request.json` / `invoice.response.json`: the Cloudflare model page
  example (text, date, number, list, null text, estimate).
- `types.request.json` / `types.response.json`: `time`, `datetime`,
  `duration` (answer has `unit`), `forecast` (`value`, `median`, `p10`, `p90`
  are arrays, plus `steps`), and `estimate`, with
  `options.include_quantiles` (99 values) and `options.return_state_text`.
- `image.request.json` / `image.response.json`: one 1x1 red PNG as a data URL
  and an empty state. The text answer has `span: null`, `source: "image"`,
  and `spell_confidence`.
- `truncated.response.json`: a 306,986-byte string state (about 76k tokens
  by `bytes / 4`). Clairic did not reject it: it cut the state to fit
  (`usage.state_tokens` 65,417) and returned `truncated: true`. The request
  is not recorded (size). Bodies up to 519,986 bytes passed this way.

Errors (envelope with `success: false`, `result: {}`):

- `context_estimate.413.json`: a 2,688,986-byte body (HTTP 413, code 5021).
  The pre-check is `bytes / 4` (plus about 15) against twice the context
  (131,072): 519,986 bytes passed, 528,986 bytes failed.
- `image_too_large.422.json`: one 4,291,493-byte PNG (over 4 MiB) (HTTP 422, code 3030).
- `too_many_images.422.json`: five images (HTTP 422, code 3030).
- `remote_image.422.json`: an `https://` image URL (HTTP 422, code 3030).
- `empty_state.422.json`: `"state": ""` and no images (HTTP 422, code 3030).
- `extra_field.422.json`: an unknown top-level field (HTTP 422, code 3030).
- `bad_type.400.json`: field type `bogus` (HTTP 400, code 5006).
- `object_state.400.json`: an object as `state` (HTTP 400, code 5006).
- `model_mismatch.400.json`: body `"model": "clef"` to the Clairic path (HTTP 400, code 5006).

Field names with `__` are accepted by Clairic; `s1 extract` rejects them in
templates so that `--flat` columns never collide.
