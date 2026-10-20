# s1 — System One gateway spec

Oct 20, 2026 · @Laurynas Keturakis

## Overview

`s1` is a local Rust daemon and CLI that gives agents one place to call System One models. It speaks the existing provider APIs unchanged, routes by the `model` field, and protects providers from overload.

**Users:** the author and the author's agents, through shell, MCP, or HTTP.

**Model families in v1**

| Family | What it returns | Wire format | Backends in v1 |
| --- | --- | --- | --- |
| Decide | Probabilities for yes/no, choice, score | Jev `/v1/systemone`; OpenAI `/v1/decisions` | TypeSafe Jev, Cloudflare Clef, OpenAI Decisions |
| Extract | Typed values: spans, numbers, dates, estimates | Clairic | Cloudflare Clairic |
| Forecast | Quantile forecasts for numeric series | TSFM.ai `/v1/forecast` | None in v1 (no local runtime); format reserved |

**Principles**

- Do not invent a new API. Each family uses its existing de facto standard.
- Routing hides behind model names. An agent sends a normal request with `"model": "decide"` or `"model": "typesafe/jev-latest"`.
- The daemon solves the hard problems once: rate limits, retries, queues, cache, size checks.
- Clients stay thin. Batch work is a loop in the client (shell pipe or agent code), not a feature.

**Non-goals for v1**

- No local model runtime. All backends are remote APIs.
- No `map` command, no run summaries, no file formats. `jq`, `duckdb`, and agent code do this.
- No payload storage, no cost accounting, no eval or distillation.
- No new request schema of our own.

## Architecture

One daemon process serves all clients. Each request goes through an ingress adapter, the router, and a queue per backend before it reaches a provider.

> Diagram (not embedded here): clients (CLI, MCP, HTTP SDKs) → daemon (ingress adapters → router → per-backend queues) → providers.

The forecast ingress exists in v1 so that the format is reserved, but it has no backend until a local runtime or a hosted TSFM provider is added.

**Transports**

- **Unix socket** (`$XDG_RUNTIME_DIR/s1.sock`, mode 0600) for the CLI. File permissions give access control.
- **TCP on 127.0.0.1** for MCP clients and HTTP SDKs, because most MCP clients accept only `http://` URLs. Requests need the local bearer token from the auth file.
- One `axum` router serves both listeners.
- The CLI starts the daemon on first use if it is not running (`s1 daemon start` also exists).

**Large inputs**

- Forward the original request bytes, unchanged except the top-level `model` field: on a typed ingress (`/v1/systemone` and the other family endpoints) s1 replaces it with the resolved upstream id (for Workers AI the short body selector: an upstream of `@cf/cloudflare/clef` gives `clef`). `/v1/raw` changes nothing. Parse only the fields that routing needs (`model`, input size, images, option counts) with borrowed or `RawValue` parsing.
- Local clients may send **file paths** for images instead of base64. A path-valued systemone asset is `{"type": "image", "s1_path": "/absolute/path.png"}` in `assets`; `s1_path` is not a TypeSafe asset field (`type`, `format`, `data_base64`, `part`), so it cannot be confused with provider input. The daemon reads the file and replaces the asset with `{"type": "image", "format": <png or jpg, from the bytes>, "data_base64": ...}` before routing, so routing and the cache key see the image content, never the path. Only those assets are rebuilt; every other member keeps its exact bytes. A key spelled with JSON escapes (for example `"s1\u005fpath"`) counts as `s1_path`.
  - Only on the Unix socket of the typed `/v1/systemone` ingress. On TCP the request fails with `invalid_request` (400) and s1 never reads the file. `/v1/raw` sends `s1_path` to the provider as is.
  - The path must be absolute (`s1 decide --image-field` makes relative paths absolute). A missing or unreadable file, or a file that is not PNG or JPEG, is `invalid_request`. A file over 16 MiB (TypeSafe's per-image limit), or an expanded body over the daemon's body limit, is `too_long` (413).
  - `s1 decide --image-field` sends paths this way, so the CLI and other local clients share one code path.
- Reuse connections to providers (HTTP/2 where the provider supports it, else keep-alive pools).

**Crates:** `tokio`, `axum`, `hyper`, `reqwest`, `serde_json`, `tokenizers`, `governor`, `clap`, `blake3`, `tracing`, `proptest`. MCP: `rmcp` if it supports the 2026-07-28 stateless transport, else a small hand-written handler.

## API families and wire formats

The endpoint selects the family. The `model` field selects the backend. Request and response bodies are the providers' own formats, unchanged, except the top-level `model` field of a request: s1 replaces it with the resolved upstream id (for example `decide` becomes `clef-flash`). Typed ingresses may also expand local image paths (see "Large inputs") and fix provider quirks in responses (see "Backend quirks"). `/v1/raw/{protocol}` is pure passthrough: request and response (status and body) unchanged.

| Endpoint on s1 | Family | Format source | Translated? |
| --- | --- | --- | --- |
| `POST /v1/systemone` | Decide | [TypeSafe API reference](https://docs.typesafe.ai/api/system-one.md) | Canonical for Decide |
| `POST /v1/decisions` | Decide | [OpenAI Decisions reference](https://developers.openai.com/api/reference/typescript/resources/decisions/index.md) | To and from systemone |
| `POST /v1/extract` | Extract | [Clairic model page](https://developers.cloudflare.com/workers-ai/models/clairic/) | No, passthrough |
| `POST /v1/forecast` | Forecast | [TSFM.ai API](https://github.com/tsfm-ai) | No, passthrough |
| `GET /v1/models` | All | pi and opencode catalog shape | s1 only |
| `GET /status` | All | s1 only | s1 only |
| `POST /mcp` | All | MCP 2026-07-28, stateless | Maps tools to the endpoints above |

### Decide: systemone (Jev)

- Request: `model`, `state` (string, object, or array), `questions` (map; each key is a name you choose), optional `assets`.
- Types: `noul` (optional `criteria.true` / `criteria.false`), `choice` (`criteria` = map of name to description, 1 to 255), `score` (`criteria` = ordered list of 1 to 10 levels), `box` (preview, images only).
- Images: `assets` map with placeholders, plus header `TypeSafe-Danger-Zone-Images: true`. Long context needs `TypeSafe-Danger-Zone-Long-Context: true`.
- Response: `answers` keyed by question name. Choice: `choice`, `confidence`, `probabilities` by name. Score: `score` (expected level), `confidence`, `legend`, `probabilities` by level index. Noul: `noul` only, no confidence.

### Decide: OpenAI Decisions

- Request: `model`, `input` (string, or user messages with `input_text` and `input_image` parts; up to 128 images; data URLs or public HTTPS URLs), `questions` (ordered array, `name` optional).
- Types: `predicate` (`instructions`), `choice` (`choices`: 2 to 255, each `value` string or boolean + optional `description`), `score` (`levels`: `label` + optional `description`).
- Response: `answers` array in question order. Predicate: `probability`. Choice: `choice`, `confidence`, `probabilities` as a list of `{value, probability}`. Score: `score`, `confidence`, `probabilities` as `{label, probability, value}`. Any question can return `type: refusal`.

### Extract: Clairic

- Request: `model`, `state` (string), `fields` (map; no upper limit), optional `images` (max 4, embedded PNG/JPEG/WebP, 4 MiB each, 8 MiB total), `reference_date`, `options` (`include_quantiles`).
- Field types: `text`, `number` (`decimals`, `integer`, `unit`), `date`, `time`, `datetime`, `duration`, `list` (`items`), `estimate` (`min`, `max`, `step`, scale), `forecast` (up to 64 steps).
- Response: `answers` keyed by field: `value`, `p_null`, `confidence`, `guess`, `span` / `spans`. Estimates: `mean`, `median`, `p10`, `p90`, `p_below`, `p_above`. Top level: `truncated`, `state_text`, `usage` with `passes`.
- Fields that do not fit in one 8,192-token pass are split into parallel passes by Clairic. Context window 65,536 tokens.
- Preview API. Pin with recorded fixtures.

### Forecast: TSFM.ai contract

- Request: `model`, `inputs: [{target: [...], ...covariates}]`, `parameters: {prediction_length, quantiles}`. Many series per request.
- s1 returns the quantile levels the backend actually produced. It does not interpolate or map to the nearest level.
- The daemon rejects irregular series. The CLI helper builds a regular grid and fills gaps with NaN.
- No backend in v1. Candidates later: a hosted TSFM provider, or a local worker such as [Sidus](https://pypi.org/project/sidus/) with Timestral added.

## Model catalog and routing

The catalog follows the pattern of pi and opencode: a small compiled-in catalog plus a user overlay file. Model IDs are `provider/model`.

**Sources, in order of priority**

1. The `model` field in the request (an ID or an alias).
2. User overlay: `~/.config/s1/models.toml`. It can add providers and models, or override any field.
3. Compiled-in catalog for the four v1 backends.

**Catalog entry**

```toml
[providers.typesafe]
base_url = "https://api.typesafe.ai"
api = "systemone"            # protocol: systemone | openai-decisions | clairic | tsfm
auth = "typesafe"            # entry name in ~/.s1/auth.json

[[providers.typesafe.models]]
id = "jev-latest"             # s1 id: typesafe/jev-latest
upstream = "jev-latest"       # id sent to the provider
context_tokens = 65536
max_options = 255
max_levels = 10
images = "preview"            # none | yes | preview (needs a header)
compat = { noul_confidence = false }

[aliases.decide]
models = ["workers-ai/clef-flash", "typesafe/jev-latest"]
spill = false                 # use later entries only when the first is unavailable, not when throttled
```

**Routing rules**

- The endpoint fixes the family. A model from another family is an error (`wrong_family`).
- An ID resolves to one backend. An alias resolves to an ordered list. The router takes the first entry that is available and whose capabilities fit the request (context, images, option count). The context fit counts text bytes as tokens and each image as at most 1,027 tokens (Clef: up to 1,024 plus 3 marker tokens), never its base64 bytes.
- Spill to the next entry when the first is throttled happens only if the alias sets `spill = true`.
- Every response records the backend and the resolved model in a response header (`s1-backend`, `s1-model`). The body is not changed.

**Cache key**

- `blake3(family, resolved model version, canonical JSON of the request without the model field)`.
- The resolved version comes from the `model` field of the provider response, so a `-latest` alias does not return stale answers after an update. On the first call with an alias, the router uses the last resolved version it has seen; a response with a new version replaces it.
- Canonical JSON: sorted keys, no insignificant whitespace, numbers as given. Image content is hashed, not its path or URL: local image paths are read into inline data before the key is made, and image URLs that s1 fetches (toward systemone) are fetched and inlined before the cache lookup. A request with image URLs that the provider fetches itself (native OpenAI Decisions) skips the cache (`s1-cache: bypass`), because s1 never sees the content.
- A request nested deeper than 128 levels gets no key: it skips the cache (no lookup, no store) and goes to the provider as usual.

**`GET /v1/models`** returns the merged catalog and aliases, with capabilities and current state (`available`, `throttled_until`, `breaker_open`).

## Adapters

Two adapter layers keep wire formats and provider quirks apart. A Protocol knows a format. A Backend knows how to reach one provider.

```rust
/// One wire format (systemone, openai-decisions, clairic, tsfm).
trait Protocol: Send + Sync {
    fn family(&self) -> Family;
    /// Read only what routing needs; keep the body as bytes.
    fn inspect(&self, body: &[u8]) -> Result<RouteInfo>; // model, tokens, images, options
    /// Turn a response into flat columns for `--flat`.
    fn flatten(&self, resp: &[u8]) -> Result<Vec<(String, Value)>>;
}

/// One provider endpoint.
#[async_trait]
trait Backend: Send + Sync {
    fn name(&self) -> &str;                          // provider name in the catalog
    fn protocols(&self) -> &[ProtocolId];            // formats it accepts natively
    fn caps(&self, model: &str) -> Option<&Model>;   // from the catalog; None if unknown
    async fn call(&self, req: BackendRequest) -> Result<ProviderResponse>;
    /// Typed ingress only (never /v1/raw): fix response quirks, e.g. 403 -> 401,
    /// remove the Workers AI envelope, map provider overflow errors to too_long.
    fn typed_response(&self, upstream_model: &str, resp: ProviderResponse)
        -> Result<ProviderResponse> { Ok(resp) }
}

/// What a backend sends upstream.
struct BackendRequest {
    upstream_model: String,  // catalog `upstream` id
    body: Bytes,             // request bytes: unchanged on /v1/raw; on a typed
                             // ingress only `model` is set to the upstream id
    headers: HeaderMap,      // client headers the provider must see (content type,
                             // opt-in headers such as TypeSafe-Danger-Zone-Images);
                             // never the client's Authorization
}

/// Only between formats of one family.
trait Translate {
    fn request(&self, from: ProtocolId, to: ProtocolId, body: &[u8]) -> Result<Translated>;
    fn response(&self, from: ProtocolId, to: ProtocolId, body: &[u8], map: &NameMap) -> Result<Bytes>;
}
```

**Backend quirks handled in v1**

| Backend | Protocols | Quirks |
| --- | --- | --- |
| TypeSafe Jev | systemone | Image header; long-context header (s1 adds it when the request is over 64k tokens or one question, that is the state plus that question, is over 32k); missing key returns 403, not 401 (the typed `/v1/systemone` ingress turns it into a clear 401 with `s1-upstream-status: 403`; `/v1/raw` returns the 403 unchanged) |
| Workers AI Clef | systemone | Model in the URL path and in the body (must match); Workers AI envelope around the response; docs say long input is silently truncated, but live calls get an explicit error (mapped to `too_long`); no `assets` (takes an `images` array); 1 to 64 questions, at least 2 options and 2 levels; every question needs non-blank `instructions`; question names `[A-Za-z0-9_.-]`, 1 to 100 characters (alias routing skips Clef when these do not fit) |
| Workers AI Clairic | clairic | Same URL and envelope as Clef; preview format |
| OpenAI Decisions | openai-decisions | Public beta; refusals per question |

**Translation rules: OpenAI Decisions and systemone**

| Topic | OpenAI Decisions | systemone | Rule |
| --- | --- | --- | --- |
| Question list | Ordered array, `name` optional | Map keyed by name | Unnamed question gets `q0`, `q1`, …; answers return in request order |
| Yes/no | `predicate` | `noul` | Same meaning; `probability` and `noul` are both P(yes) |
| Noul criteria | No field | `criteria.true` / `.false` | Append to `instructions` when going to OpenAI |
| Choice values | `value` string or boolean | Name is a string | Boolean `true` becomes name `bool:true`; decoded on return |
| Choice descriptions | `description` per choice | Value in the `criteria` map | Direct mapping |
| Score levels | `levels: [{label, description}]` | `criteria: [...]`, 1 to 10 | Label + description joined; more than 10 levels → `unsupported` |
| Images | `input_image` parts, URL or data URL | `assets` with placeholders + header | Convert; URLs are fetched by s1 only when going to systemone |
| Refusal | `type: refusal` | None | To a systemone client: per-question error object, no invented value |
| `box` | None | Preview type | `unsupported` for non-Jev backends |
| Noul confidence | None | None | Never added |

**As built (stage 4)**

- `src/translate/`: `decide.rs` (the table above, both directions), `errors.rs` (provider error bodies in the client's format, status kept), `fetch.rs` (image URLs to data URLs, toward systemone only; PNG or JPEG, 16 MiB each, 128 per request). Registration: `translate::translator`.
- Image URL fetch (toward systemone): only `https` URLs whose host is, or resolves only to, public addresses (no loopback, private, link-local, shared, documentation, multicast, or reserved ranges, and no IPv6 forms of them). The check runs in the fetch client's DNS resolver, so the checked address is the one connected to, and again on each redirect (at most 5); no proxy. All images of one request share one budget, checked while the bytes arrive: the body with images inlined must stay within the 64 MiB body limit, else `too_long`. At most 16 downloads run at a time in the daemon. Errors and logs name an image by position (`input[0].content[1]`), never by URL. A key spelled with JSON escapes (`"image\u005furl"`) counts as `image_url`.
- Placeholders made toward systemone are absent from the input text and from every string in the questions. Toward OpenAI, a structured (object or list) `state` goes as its compact JSON text, cut into image parts only at asset references inside its string values (not object keys), matched before JSON escaping.
- Flat columns of OpenAI choice values are one-to-one: `true` / `false` are `bool:true` / `bool:false`, other non-strings are `json:<compact JSON>` (`json:1`, `json:null`), and a string that starts with `bool:`, `json:`, or `str:` gets a `str:` prefix (`str:bool:true`). Score columns stay `q__p__<level>`.
- A missing or wrong TCP bearer token is 401 in the endpoint's format: `/v1/systemone` gives `{"detail": {"error_type": "unauthorized", …}}`, `/v1/decisions` the OpenAI error shape with `type` `authentication_error` and `code` `unauthorized`; other endpoints keep the generic `{"error": {"code", "message"}}`.
- The router accepts any backend whose format can be translated from the ingress (`Route.api`); the ingress translates the request after the cache lookup and the answer before the cache store, so the cache holds client-format answers.
- Names: a free client name is kept; an unnamed question gets `q{i}`; a taken name gets `_1`, `_2`, …. Answers to OpenAI clients have the client's `name` (or `null`).
- Noul criteria go to OpenAI as `\n\nTrue if: …` and `\n\nFalse if: …` suffixes on `instructions`; from OpenAI they are read back into `criteria`. Client text that holds `True if: ` or `False if: ` is escaped with a U+200B (zero width space) before the colon (one more for each U+200B already there), so only s1's own suffixes have none and the client text reads back unchanged.
- Choice names `bool:true` and `bool:false` are the names of OpenAI's boolean values in both directions: a systemone choice named `bool:true` goes to OpenAI as the boolean `true`, and its answer comes back as `bool:true`. A systemone client has only string names, so nothing is lost. Toward systemone, an OpenAI string value `"bool:true"` or `"bool:false"` is `unsupported` (it would come back as a boolean).
- Systemone `state`, `instructions`, and `criteria` values that are not strings (objects, lists) go to OpenAI as compact JSON text; OpenAI has only text, so a round trip returns them as JSON strings.
- OpenAI `input` messages must be `user` role; adjacent text parts join with a newline, messages with a blank line; each image becomes a `<s1-image-{i}>` placeholder (no separator next to it) and an asset; the same image data twice is one asset. Toward OpenAI, `state` is split at the placeholders into text and image parts of one user message. Placeholder names, the asset format (`jpg` becomes `jpeg`), and message bounds do not survive a round trip. An asset placeholder in a question's `instructions` or `criteria` is `unsupported` toward OpenAI (OpenAI takes images only in the input). Clef `images` items become `input_image` parts.
- Toward OpenAI, every image occurrence counts: one asset referenced many times expands once per reference. The image data URLs of one request (assets per reference plus Clef `images`) may total at most 64 MiB (the body limit), checked before each image is built; over that is `too_long`. More than 128 occurrences (OpenAI's per-request image limit) is `unsupported` for an OpenAI model: an alias skips it, a named model fails before the call. Routing context checks and the limiter cost count each occurrence as one image (at most 1,027 tokens).
- On `/v1/decisions`, the Clef limits (missing instructions, names outside `[A-Za-z0-9_.-]{1,100}`) are read from the questions as a systemone backend gets them: an unnamed question is `q{i}`, and a predicate whose `instructions` hold only appended `True if: ` / `False if: ` criteria has no instructions. Such requests on an alias skip Clef.
- Answers toward a systemone client are keyed by the client's names; when an OpenAI request repeats a `name`, later answers use the unique systemone name (`a_1`), so no answer is lost.
- A typed or raw ingress reads at most 64 MiB of body; over that is `too_long` (413) in the ingress's own error format with `s1-request-id`.
- A refusal or any non-answer reaches a systemone client as `{"type": "error", "error": {"code": "refusal", "message": …}}`; a systemone `error` answer reaches an OpenAI client as `refusal`.
- Typed ingresses decode `Content-Encoding: zstd` and `gzip` request bodies (the TypeSafe Python SDK sends zstd for bodies of 1 KiB or more on Python 3.14+). `/v1/raw` forwards the compressed bytes and `Content-Encoding` unchanged, but sizes the limiter cost (and reads `model`) from a decoded copy, so compression does not get around `tpm`; a decoded body over 64 MiB is `too_long`. Multipart bodies (TypeSafe SDK `part` assets) are not supported.
- Live (2026-10-20): `gpt-6-luna` takes 1 to 400 questions; Jev rejects small images (`Invalid image dimensions.` for 256x256; 512x512 works) and takes asset formats `png`, `jpg`, `jpeg`.
- SDK conformance: `conformance/` (uv project, Python 3.14, `openai` 3.35.1 and `typesafe-sdk` 0.7.5), run by `tests/conformance.rs` with `S1_CONFORMANCE=1` (mocks) and with `S1_LIVE=1` also (real providers).

**Errors.** Each ingress returns errors in its own format. s1-specific errors use these codes in every format: `queue_full` (429), `too_long` (413), `unsupported` (422), `wrong_family` (400), `backend_unavailable` (503).

## Backpressure and rate limits

The daemon absorbs short bursts and signals clearly when a burst is too large. Providers never see more than their limits, so a naive client loop is also a safe loop.

**Toward providers (one limiter per backend and API key)**

- Token buckets for requests per minute and input tokens per minute. Tokens are counted before the call with `tokenizers` (or a byte estimate when no tokenizer is known).
- A cap on requests in flight.
- Adaptive control: obey rate-limit headers and `Retry-After`. On 429, halve the in-flight cap; then add one per success window (AIMD).
- Retries: exponential backoff with jitter, at most 4 attempts, only on 429, 5xx, and connection errors.
- Circuit breaker: after 5 failures in 30 s, stop for a cool-down and mark the backend `breaker_open`.

```toml
[limits.typesafe]
rpm = 600
tpm = 2_000_000
max_in_flight = 32
adaptive = true
```

The numbers above are placeholders. Set them from each provider's real limits.

**Toward clients**

- A bounded queue per backend, measured in requests and in tokens.
- Two lanes: an interactive lane (single requests) has priority over a bulk lane (MCP list calls, and CLI streams after the first N requests).
- When a queue is full: HTTP 429 with `Retry-After`, computed from the current drain rate. The OpenAI and TypeSafe SDKs already retry on this.
- MCP list calls: the daemon estimates the run time from the drain rate before it starts. If the estimate exceeds the client timeout, it rejects the call with a suggested chunk size and a retry time.
- The CLI keeps a window of requests in flight sized to the backend's current cap. When the window is full, it stops reading stdin, so the pipe blocks the producer. The window counts requests in flight and finished replies that wait for an earlier line (ordered output), so one slow line also stops stdin reads. Each reply is written as soon as its turn comes, also while stdin stays open.

**`GET /status`** (and `s1 status`) per backend: queue depth per lane, current rate, current in-flight cap, limits, `throttled_until`, breaker state. `throttled_until` (here and in `/v1/models`) is the earliest time a new request could start: the latest of the provider pause (`Retry-After` and quota headers, capped at 24 h) and the RPM and TPM bucket waits.

Spend budgets wait for cost accounting (later).

**As built (stage 3)**

- Limiter: a token bucket per RPM and TPM, written in s1 (not `governor`), so that tests can drive it with a simulated clock and one check covers both buckets at once. The burst is a tenth of the limit and the refill fills the rest. Each bucket also keeps a log of the last 60 seconds of sends, and a request goes only when the logged tokens plus its cost are at most the limit, so no 60-second window ever holds more than the limit. A request larger than the burst goes only when the bucket is full and leaves it in debt. A request larger than the limit itself goes alone: after 60 seconds with no sends, and nothing else goes in the 60 seconds after it.
- Tokens are a byte estimate (text bytes / 3, plus at most 1,027 tokens per image); no `tokenizers` yet.
- `[limits.<provider>]` also takes `success_window_ms`, `retry_attempts`, `retry_base_ms`, `retry_max_ms`, `retry_after_max_ms`, `breaker_failures`, `breaker_window_ms`, and `breaker_cooldown_ms`. `retry_attempts` is 1 to 4 (default 4); a larger value is rejected. A provider without a table has no RPM or TPM limit (in-flight cap 32).
- AIMD: a 429 halves the cap once per success window; a 429 for a request sent before the last decrease only extends the hold.
- Retries obey `Retry-After` (seconds or HTTP date), `retry-after-ms`, Cloudflare `ratelimit` and OpenAI `x-ratelimit-*` headers. Every `ratelimit` policy counts, across comma-separated lists and repeated header values; s1 pauses until the longest reset among the exhausted policies. A pause longer than `retry_after_max_ms` (default 60 s) ends the retries: the client gets the provider's 429 and the backend stays throttled.
- The breaker counts each failed attempt (5xx or a connection error), not each request. A 429 is not a failure. An open breaker gives `backend_unavailable` (503) with `Retry-After`.
- Queue sizes are `[queue] max_requests` and `max_tokens` in `config.toml`, per backend and key. The key is resolved once, when the request is queued, and the request is sent with that key: a key changed with `s1 auth set` applies to new requests, not to ones already waiting under the old key's limits. Retries go to the front of their lane and are not bounded.
- Request headers: `s1-lane: bulk` (default interactive), `Cache-Control: no-cache` (skip the lookup, store the fresh answer), `Cache-Control: no-store` (no cache). Response headers: `s1-cache` (`hit`, `miss`, `refresh`, `bypass`), `s1-capacity` (the backend's current in-flight cap), `s1-retries`. Cache hits also carry `s1-capacity` and `s1-retries: 0`.
- `/v1/raw` goes through the same queue, limits, retries, and breaker, but not the cache.
- The CLI sends its first 4 stream requests in the interactive lane and the rest in bulk. Its window starts at 4 and then follows `s1-capacity`; `-j` is an upper bound.

## Clients

Clients are thin. The verbs match the model families: `decide`, `extract`, `forecast`. A request template is a normal provider request body without the input; there is no s1 schema format.

### CLI

```
s1 decide   -t triage.systemone.json "ticket text"
s1 decide   -t triage.decisions.json --openai "ticket text"
s1 extract  -t invoice.clairic.json < invoice.txt
s1 forecast -m <model> --csv sales.csv --id sku --time ds --value y -h 28
s1 models
s1 status
s1 raw systemone < body.json          # pure passthrough
s1 daemon start|stop|logs
```

**Streaming filter mode** (when stdin is not a terminal, or with `--jsonl`):

```
cat tickets.jsonl \
  | s1 decide -t triage.systemone.json --state-field body --flat \
  | duckdb -c "select urgency, count(*) from read_json('/dev/stdin') group by 1"
```

- One JSON line in, one JSON line out. The `id` field passes through (`--id-field` to change it).
- `--state-field <name>`: what the model reads. With it, s1 sends only that field as the state. Default: the whole line. For systemone the line is sent as an object; for Clairic it is sent as JSON text, because Clairic takes only a string. `--image-field <name>` reads image paths.
- `--keep <fields>`: what the output carries besides `id` and the answers. Default: nothing. `--keep body,customer` copies those input fields; `--keep '*'` copies the whole input line, including answer columns from an earlier step. If a kept field has the same name as a new answer column, s1 stops with an error and does not overwrite it.
- Output order: input order by default; `--unordered` writes in completion order.
- `--flat` writes flat columns (below). Without it, each line holds `id`, the kept fields, and the provider response under `response`.
- Errors for one line go to stdout as `{"id": …, "error": {…}}` plus the kept fields, and do not stop the stream. Exit code is non-zero if any line failed.

**Cascades.** With `--keep`, each step's output is the next step's input, so narrower questions run only where they matter, in one pipe:

```
cat tickets.jsonl \
  | s1 decide  -t topic.json   --state-field body --keep '*' --flat \
  | jq -c 'select(.topic == "billing" and .topic__conf > 0.8)' \
  | s1 decide  -t billing.json --state-field body --keep '*' --flat \
  | jq -c 'select(.threatens_cancel > 0.8)' \
  | s1 extract -t refund.json  --state-field body --keep '*' --flat \
  > at_risk.jsonl
```

Each final line holds the ticket, the topic answers, the billing answers, and the extracted values. Question names must differ between steps, or the collision check stops the stream.

**Flat columns.** The separator is `__` (double underscore). Question and field names with `__` are rejected in templates, so column names never collide.

| Family | Columns per question or field |
| --- | --- |
| Decide: choice | `q`, `q__conf`, `q__p__<option>` |
| Decide: score | `q`, `q__conf`, `q__p__<level>` |
| Decide: noul / predicate | `q` (probability of yes) |
| Extract: scalar | `f`, `f__conf`, `f__p_null`, `f__guess`, `f__span` |
| Extract: estimate | `f`, `f__median`, `f__p10`, `f__p90` |
| Forecast | Long format: `id`, `step`, `timestamp`, `q<level>` per quantile |

### MCP

- Transport: Streamable HTTP at `POST /mcp`, protocol 2026-07-28, stateless. No stdio and no sessions in v1. A fallback for 2025-11-25 clients is added only if a client in use needs it.
- `tools/list` is static per catalog version and marked cacheable.

| Tool | Input | Output |
| --- | --- | --- |
| `decide` | systemone `template` + `states: [{id, state}]` (1 to 1,000) | `results: [{id, ok, answers \| error}]` |
| `extract` | Clairic `template` + `states: [{id, state, images?}]` (1 to 1,000) | Same shape as `decide` |
| `forecast` | TSFM request, or `csv_path` + column names | TSFM response |
| `models` | none | Catalog, aliases, state |

- Each tool declares an output schema, so programmatic callers can use results without parsing.
- One failed item never fails the call. Each item has its own `ok` and `error`.
- Tool descriptions state the item limit and say that lists are for use from code, so a model calling the tool directly does not fill its context.
- Progress notifications during long list calls, if the stateless transport allows them in the response stream. Otherwise the item cap keeps calls inside client timeouts.

## Cache, logging, secrets, config

No request payloads are stored. The cache keeps only key hashes and responses; logs keep metadata only.

**Cache**

- Store: one embedded key-value file (`redb` or SQLite) in `~/.cache/s1/`.
- Entry: key hash (see Model catalog and routing) → response bytes + resolved model + time.
- Size cap (default 1 GiB) with LRU eviction. No TTL by default; `--no-cache` per request and `s1 cache clear`.
- As built: `~/.cache/s1/cache.redb`; `[cache] enabled` and `max_bytes` in `config.toml`; `POST /cache/clear` on the daemon. `max_bytes` caps the file on disk: stored responses get 3/4 of it, each charged 256 bytes for its keys and index rows (the rest is room for redb pages and freed space), the file is compacted when it grows past the cap (plus 2 MiB for the fixed size of an empty redb file) and, if it is still over, the oldest entries go until it fits, and `s1 cache clear` and a daemon start with a lower cap evict and compact at once. `/status` reports `bytes` (responses plus the per-entry charge) and `file_bytes`. The key also holds the long-context opt-in. Last resolved versions are in `~/.local/state/s1/versions.json`.
- Only successful responses are cached. Partial results (a refused question) are cached as returned.

**Logging**

- `tracing` with JSON output to daily files `~/.local/state/s1/s1.log.YYYY-MM-DD` (one file per day), 7 days kept.
- Per request: request ID, ingress, alias, resolved backend and model, lane, queue wait, upstream latency, status, retries, input tokens, cache hit.
- Per backend events: limit changes (AIMD), breaker open and close, 429s with `Retry-After`.
- No payloads. A `--debug-payloads` flag for local debugging may come later.

**Secrets**

- API keys live in one JSON file, `~/.s1/auth.json`, in the same shape as pi's `~/.pi/agent/auth.json`. No OS keychain.
- The file has mode 0600 and the directory `~/.s1/` has mode 0700. s1 refuses to read the file if group or others can read it. Writes go to a temporary file and then an atomic rename.
- One entry per provider, keyed by the provider name in the catalog:

  ```json
  {
    "typesafe":   { "type": "api_key", "key": "…" },
    "workers-ai": { "type": "api_key", "key": "…", "accountId": "…" },
    "openai":     { "type": "api_key", "key": "…" },
    "s1-local":   { "type": "api_key", "key": "…" }
  }
  ```

- `type` is `api_key` in v1. The pi `oauth` shape (`access`, `refresh`, `expires`) is reserved for providers that need it later.
- `s1 auth set <provider>` reads the key from a prompt with no echo (or from stdin) and writes the entry. `s1 auth list` shows which providers have keys, never the values.
- Environment variables (`TYPESAFE_API_KEY`, `CLOUDFLARE_API_TOKEN` + `CLOUDFLARE_ACCOUNT_ID`, `OPENAI_API_KEY`) override the file.
- `s1-local` holds the local bearer token for TCP clients. s1 makes it on first start. `s1 auth token` prints it for MCP client setup.

**Config files**

| File | Holds |
| --- | --- |
| `~/.s1/auth.json` | Provider API keys and the local bearer token (mode 0600) |
| `~/.config/s1/config.toml` | Listeners, queue sizes, cache size, log level |
| `~/.config/s1/models.toml` | Provider and model overlay, aliases, limits |
| `~/.cache/s1/` | Response cache |
| `~/.local/state/s1/` | Logs, PID, last resolved model versions |

## Testing

Property tests with `proptest` carry most of the weight. Example tests and recorded fixtures cover provider formats.

**Properties**

| Area | Property |
| --- | --- |
| Translation | systemone → OpenAI Decisions → systemone returns the same request, for every request with string texts (empty strings included) and no images; non-string texts come back as their JSON text, and empty `instructions` come back as absent (OpenAI always has `instructions`) |
| Translation | OpenAI Decisions → systemone → OpenAI Decisions keeps image input exactly (one user message of text and image parts) |
| Translation | Answers come back with the same names and in request order, for any mix of named and unnamed questions |
| Translation | Boolean and string choice values with the same text stay distinct after a round trip |
| Cache key | JSON key order and whitespace do not change the key; any change to a question, option, or field does; new content at the same image URL misses; image URLs that s1 does not fetch bypass the cache |
| Rate limiter | With a simulated clock and random arrivals, sent requests never exceed RPM, TPM, or the in-flight cap in any window |
| AIMD | After a 429, the in-flight cap never rises above half its earlier value until a success window passes |
| Queue | Depth never exceeds its bound; every accepted request ends with exactly one result or error |
| Lanes | An interactive request never waits behind more than the bulk requests already in flight |
| Flatten | Column names are unique for any valid set of question and field names |
| Streaming CLI | Output has one line per input line, with the same ids, in input order unless `--unordered` |

**Other tests**

- **Mock backends** (in-process `axum` servers) that return 429 with `Retry-After`, slow responses, 5xx, and dropped connections. Used by the property tests above.
- **Fixtures:** recorded request/response pairs per provider, from the docs and from real calls. A format change in the Clairic preview shows up as a fixture failure.
- **Conformance:** the official OpenAI and TypeSafe SDKs call s1 as their base URL and must work unchanged.
- **Live smoke tests** (manual, behind a flag) against each provider with tiny requests.

## Milestones

Each milestone ends with something usable from the shell.

1. **Skeleton:** daemon on Unix socket and TCP, catalog loading, auth file, `s1 raw` passthrough to TypeSafe, logs.
2. **Decide:** systemone ingress, Jev and Clef backends, router with aliases, `s1 decide` single and streaming, `--flat`.
3. **Protection:** limiters, AIMD, retries, breaker, bounded queues with two lanes, `/status`, cache. Property tests for all of these.
4. **OpenAI Decisions:** ingress and backend, translation both ways, SDK conformance tests.
5. **Extract:** Clairic ingress and backend, `s1 extract`, fixtures.
6. **MCP:** stateless `/mcp` with `decide`, `extract`, `models` and list inputs.
7. **Forecast format:** TSFM ingress and CLI CSV helper, ready for a backend.

## Open questions

- Does `rmcp` support the 2026-07-28 stateless transport, and can a stateless call stream progress?
- ~~Is Clef fully systemone-compatible in practice (limits, error shapes, truncation)?~~ Answered in milestone 2 (live calls, 2026-10-20): answers and usage have the systemone shape; requests differ (no `box`, no `assets`, unknown fields rejected, 1 to 64 questions, at least 2 options and 2 levels, non-blank `instructions` required, question names `[A-Za-z0-9_.-]{1,100}`). Errors use the Workers AI envelope. Over-context input is rejected (413 code 5021 when `bytes / 4` exceeds the context, image data not counted; 422 code 3030 "Request requires N tokens"), not silently truncated. Fixtures: `tests/fixtures/workers-ai/`.
- Real rate limits for each provider account, to replace the placeholder numbers.
- Item cap for MCP list calls (1,000 is a guess). Measure against client timeouts.
- Which forecast backend comes first: a hosted TSFM provider or a local worker?
- Name collision for `s1` on crates.io and Homebrew; fallback crate name `s1-gateway`.

## Sources

- [TypeSafe System One API reference](https://docs.typesafe.ai/api/system-one.md)
- [OpenAI Decisions API reference](https://developers.openai.com/api/reference/typescript/resources/decisions/index.md)
- [Cloudflare Clairic model page](https://developers.cloudflare.com/workers-ai/models/clairic/)
- [Cloudflare Clairic changelog](https://developers.cloudflare.com/changelog/post/2026-10-19-clairic-workers-ai/)
- [TSFM.ai](https://github.com/tsfm-ai)
- [Sidus (TSFM-compatible server)](https://pypi.org/project/sidus/)
- [Timestral model card](https://huggingface.co/mistralai/Timestral-209M)
- [MCP 2026-07-28 specification release](https://blog.modelcontextprotocol.io/posts/2026-07-28/)
- [opencode providers](https://v2.opencode.ai/docs/providers)
- [pi models.json with a gateway](https://docs.merge.dev/merge-gateway/coding-agents/pi.md)
