# OpenAI Decisions fixtures

Recorded live from `POST https://api.openai.com/v1/decisions` on 2026-10-20
(`gpt-6-luna`, public beta, no beta header). No keys are stored; the
invalid-key body echoes only the fake key `sk-invalid`.

| File | What |
| --- | --- |
| `triage.request.json` / `triage.response.json` | predicate + unnamed choice with a boolean value + score with a level description |
| `refusal.request.json` / `refusal.response.json` | empty `instructions` returns `type: refusal` for that question |
| `typed_choice.response.json` | `"true"` and `true` as two distinct choice values |
| `one_choice.400.json` | 1 choice (needs 2 to 255) |
| `too_many_levels.400.json` | 12 score levels (max 10; min is 2) |
| `model_not_found.404.json` | unknown model |
| `invalid_key.401.json` | bad key |
| `systemone_via_openai.response.json` | s1 answer to a systemone client routed to `openai/gpt-6-luna` (`tests/live.rs`) |
| `systemone_refusal.response.json` | the refusal case through s1 to a systemone client: a per-question `error` |
| `decisions_via_jev.response.json` | s1 answer to an OpenAI client routed to `typesafe/jev-latest` |

Response headers seen: `x-request-id`, `openai-processing-ms`,
`x-ratelimit-{limit,remaining,reset}-{requests,tokens}`.
