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
- The CLI starts the daemon on first use if it is not running (`s1 util daemon start` also exists).
- As built: systemd socket activation (`contrib/systemd/`). With `LISTEN_PID` equal to its pid, the daemon serves the passed fds (one Unix, one loopback TCP listener, told apart by family) and never binds, replaces, or removes the socket file. SIGTERM drains open requests, and a connection accepted before it may send its first request within 5 s (hyper alone closes such a connection as idle); an idle kept-alive connection is closed. Connections made during a restart wait in the backlog that systemd keeps. `/health` reports `socket_activated`, and `s1 util daemon stop` then refuses and names `systemctl --user stop s1.socket s1.service`. `s1-upgrade.path` restarts the service when `~/.cargo/bin/s1` changes.

**Large inputs**

- Forward the original request bytes, unchanged except the top-level `model` field: on a typed ingress (`/v1/systemone` and the other family endpoints) s1 replaces it with the resolved upstream id (for Workers AI the short body selector: an upstream of `@cf/cloudflare/clef` gives `clef`). `/v1/raw` changes nothing. Parse only the fields that routing needs (`model`, input size, images, option counts) with borrowed or `RawValue` parsing.
- Local clients may send **file paths** for images instead of base64. A path-valued systemone asset is `{"type": "image", "s1_path": "/absolute/path.png"}` in `assets`; `s1_path` is not a TypeSafe asset field (`type`, `format`, `data_base64`, `part`), so it cannot be confused with provider input. The daemon reads the file and replaces the asset with `{"type": "image", "format": <png or jpg, from the bytes>, "data_base64": ...}` before routing, so routing and the cache key see the image content, never the path. Only those assets are rebuilt; every other member keeps its exact bytes. A key spelled with JSON escapes (for example `"s1\u005fpath"`) counts as `s1_path`.
  - Only on the Unix socket of the typed `/v1/systemone` ingress. On TCP the request fails with `invalid_request` (400) and s1 never reads the file. `/v1/raw` sends `s1_path` to the provider as is.
  - The path must be absolute (`s1 decide --image-field` makes relative paths absolute). A missing or unreadable file, or a file that is not PNG or JPEG, is `invalid_request`. A file over 16 MiB (TypeSafe's per-image limit), or an expanded body over the daemon's body limit, is `too_long` (413).
  - `s1 decide --image-field` sends paths this way, so the CLI and other local clients share one code path.
- Reuse connections to providers (HTTP/2 where the provider supports it, else keep-alive pools).

**Crates:** `tokio`, `axum`, `hyper`, `reqwest`, `serde_json`, `tokenizers`, `governor`, `clap`, `blake3`, `tracing`, `proptest`. MCP: a small hand-written handler for the 2026-07-28 stateless transport (`server/discover`, `tools/list`, `tools/call` only). `rmcp` 3.x supports that transport too, but s1 does not use it: three methods do not justify the dependency, and the tools call the ingress handler in-process (see "As built (stage 6)" under MCP).

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

**As built (stage 5)** (live calls, 2026-10-20; fixtures in `tests/fixtures/clairic/`)

- Same URL, auth, and envelope as Clef (`.../ai/run/@cf/cloudflare/clairic`, body `model` = `clairic`). On `/v1/extract` the client gets `result` only; provider errors keep the Workers AI envelope, which is the Clairic error format. s1 errors use the same envelope with the s1 code: `{"result": {}, "success": false, "errors": [{"code": "too_long", "message": "…"}], "messages": []}`. A missing or wrong TCP bearer token on `/v1/extract` is 401 in this envelope with code `unauthorized`.
- Long state is cut to fit and flagged with `truncated: true` (passed through). Workers AI refuses only when `bytes / 4` exceeds twice the context (131,072): 413 code 5021, mapped to `too_long`. The catalog marks this with `compat = { truncates = true, max_request_tokens = 131072 }`, and the router checks only that limit for such models.
- s1 refuses before the call what Clairic surely refuses: a non-string or empty state without images, no `fields`, and remote image URLs (`invalid_request`); more than 4 images (`unsupported`); an image over 4 MiB or images over 8 MiB in total, decoded (`too_long`). Other validation (field specs, unknown top-level fields) is left to Clairic.
- Undocumented answer keys seen live: `duration` answers carry `unit`; image-sourced text answers have `span: null`, `source: "image"`, and `spell_confidence`; `forecast` answers have `value`, `median`, `p10`, `p90` as arrays plus `steps`.

### Forecast: TSFM.ai contract

- Request: `model`, `inputs: [{target: [[...]], start, past_covariates, future_covariates, static_covariates, metadata}]`, `parameters: {prediction_length, frequency, quantile_levels}`. Many series per request. `target` is 2D (`[timesteps][channels]`); `null` is a missing value. The time axis is `start` plus the pandas offset alias `frequency`.
- Response: `quantile_levels`, and `outputs[]` with `mean`, `quantile_predictions: [{level, values}]`, `timestamps`. Errors: `{"error", "code", "endpoint"}`.
- s1 returns the quantile levels the backend actually produced. It does not interpolate or map to the nearest level.
- The daemon rejects irregular series. The CLI helper builds a regular grid and fills gaps with NaN.
- No backend in v1. Candidates later: a hosted TSFM provider, or a local worker such as [Sidus](https://pypi.org/project/sidus/) with Timestral added.

**As built (stage 7)**

- Irregular means: a 1D or ragged `target`, `past_covariates` with another length than `target`, a `start` or `frequency` s1 cannot read, or an `inputs[].timestamps` list (not a TSFM field) that is not evenly spaced or does not match `start` and `frequency`. With timestamps, a declared `frequency` is the grid they must follow: s, min, h, D, MS (with a count) and weekly `W`/`W-<DAY>` (`W` = `W-SUN`; every point on that weekday). Weekday and month-start anchors are checked on each timestamp's wall-clock time in its own offset (as pandas does), so `2026-01-04T00:00:00+14:00` is a Sunday although it is a Saturday in UTC. A frequency s1 cannot interpret (for example `Q` or `B`) cannot vouch for timestamps, so such a request is rejected; without timestamps it is passed on. These get `invalid_request` (400) in the TSFM error format.
- With no forecast model in the catalog, `/v1/forecast` returns `backend_unavailable` (503) in the TSFM error format. A provider with `api = "tsfm"` in `models.toml` uses a generic TSFM-compatible backend (`POST {base_url}/v1/forecast`, bearer key, env `TSFM_API_KEY`). The compiled-in catalog has no such provider.
- `s1 forecast --csv`: rows are grouped by `--id`. The step is `--freq` (s, min, h, D, W as 7D, MS, with a count), else the template's `parameters.frequency`, else the smallest gap (month starts when all times are month starts). Each series starts at its first time. NaN is sent as `null`, because JSON has no NaN. A time off the grid, a repeated time, or a non-finite value (`inf`, `1e999`) stops the command; only empty, NaN, NA, N/A, null, and none are missing values. Without `--csv`, stdin holds a TSFM request; `-h`, `--freq`, and `-q` then set its `parameters.prediction_length`, `frequency`, and `quantile_levels`, and `--id`, `--time`, and `--value` are refused. `--print-request` prints the request without sending it. All series together may have at most 1,000,000 grid points; this is checked before any grid is built (over it: an error in the CLI, `too_long` in the MCP tool). A template `parameters` that is not an object, or a `parameters.frequency` that is not a string, is refused (null means not given).
- Flat rows: `id` (echoed `metadata.id`, then `metadata.item_id`, else the series index), `step` (1 is the first forecast step), `timestamp`, `channel` (multivariate series only), `mean`, and `q<level>` per produced quantile, in the produced order.

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
| Workers AI Clairic | clairic | Same URL and envelope as Clef; preview format; long state cut and flagged `truncated` (not refused) up to `bytes / 4` = 2 × context; errors keep the envelope |
| OpenAI Decisions | openai-decisions | Public beta; refusals per question |
| Baseten (baselabs `mammoth-1`, `elephant-1`) | systemone | Same client as Jev, bearer key `BASETEN_API_KEY` (`Api-Key` also works); upstream ids are Model API slugs (`decision-models/baseten-mammoth-27b-latest`, `decision-models/baseten-elephant-9b-latest`); text only, no `box`, at most 10 levels and 255 options; `instructions` optional. Live (2026-10-22): one prompt (state plus one question) takes at most 32768 tokens, the whole request has no token limit of its own; Baseten refuses (422) a state over 50000 characters and over 512 options per request (a noul counts 2), and s1 leaves those checks to it. Docs ask for a string or `{"messages": [...]}` state; other JSON is taken but read in another form, so on the typed ingress s1 sends it as JSON text indented by 2, the trained form (`compat.string_state`; key order kept, numbers printed again, other members byte for byte; a state nested too deep to read is refused with 422; raw passthrough sends the bytes unchanged). s1 also refuses a question whose prompt is surely over 32768 tokens (`question_context_tokens` with no long-context opt-in). The cache key is canonical JSON, so two states that differ only in key order share an entry. Errors are `{"error", "error_type"}`; a bad key is 403, passed through |

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
- A missing or wrong TCP bearer token is 401 in the endpoint's format: `/v1/systemone` gives `{"detail": {"error_type": "unauthorized", …}}`, `/v1/decisions` the OpenAI error shape with `type` `authentication_error` and `code` `unauthorized`, `/v1/extract` the Workers AI envelope, `/v1/forecast` the TSFM body `{"error", "code": "unauthorized", "endpoint": "/v1/forecast"}`; endpoints without a typed ingress (`/status`, `/v1/raw/...`, `/mcp`, ...) keep the generic `{"error": {"code", "message"}}`.
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
s1 util raw systemone < body.json     # pure passthrough
s1 util daemon start|stop|logs
```

**Streaming filter mode** (when stdin is not a terminal, or with `--jsonl`):

```
cat tickets.jsonl \
  | s1 decide -t triage.systemone.json --state-field body --flat \
  | duckdb -c "select urgency, count(*) from read_json('/dev/stdin') group by 1"
```

- One JSON line in, one JSON line out. The `id` field passes through (`--id-field` to change it).
- `s1 extract` input: a text argument is the state, and `-` reads all of stdin as the state. Without an argument, stdin is JSON lines when `--jsonl` is given or its first non-blank line is a JSON object; otherwise all of stdin is one state (so `s1 extract -t t.json < invoice.txt` works).
- `--state-field <name>`: what the model reads. With it, s1 sends only that field as the state. Default: the whole line. For systemone the line is sent as an object; for Clairic it is sent as JSON text, because Clairic takes only a string. `--image-field <name>` reads image paths (a path or a list). `s1 decide` sends them as path assets; `s1 extract` reads the files and sends them inline as data URLs in Clairic `images`, after any images in the template, because Clairic has no path assets.
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
| Extract: forecast field | `f`, `f__median`, `f__p10`, `f__p90` (arrays, one item per step) |
| Extract: list | `f`, `f__conf`, `f__spans` |
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

**As built (stage 6)**

- A small hand-written handler in `src/mcp/` (not `rmcp`). `rmcp` 3.x does support the 2026-07-28 stateless transport and progress on the response stream, but s1 needs only three methods, and its list tools call the typed ingress handler in-process. No new crate apart from `http-body`.
- Methods: `server/discover`, `tools/list`, `tools/call`. Other methods get 404 and `-32601`. `initialize` gets `-32022` with the supported versions. Notifications get 202. GET and DELETE get 405.
- Each request needs the `MCP-Protocol-Version` and `Mcp-Method` headers (and `Mcp-Name` for `tools/call`, also in the `=?base64?…?=` form) that match the body, else 400 and `-32020`. `params._meta` needs `protocolVersion` and `clientCapabilities`, else 400 and `-32602`. Any version other than 2026-07-28 gets 400 and `-32022`. A foreign `Origin` gets 403. TCP needs the `s1-local` bearer token (401 with `WWW-Authenticate: Bearer`); the Unix socket does not.
- `tools/list` has `ttlMs` (1 h), `cacheScope: public`, and `_meta["s1/catalogVersion"]` (a hash of the tool list). The descriptions name the models and aliases of the family, so the list changes only with the catalog.
- List items: each state is one request to the typed ingress (`/v1/systemone` or `/v1/extract` logic) in the bulk lane, so it uses routing, the cache, limits, retries, and the breaker. The `id` defaults to the index. `extract` sends a non-string state as JSON text and takes `images` (data URLs) per item. A result item is `{id, ok, answers, model}` (`extract` adds `truncated`) or `{id, ok: false, error: {code, message, status?}}`. Bad items, bad template fields, and provider errors are item errors. Calls with 0 or more than 1,000 states, or with bad arguments, are tool errors (`isError: true`, code `invalid_request`).
- Items in flight follow the backend's in-flight cap (8 when there is no scheduler yet).
- Pre-run estimate: every item is built, inspected, and routed (one at a time; nothing is kept), and items are grouped by backend. Per group: `(waiting + in flight + items) / rate + provider pause`, with the group's mean input tokens. The rate is the larger of the observed drain rate and the in-flight cap per second, then capped by the RPM and TPM limits (the limits always win). Items that cannot be built, fail the protocol's own checks (for example too many images for Clairic), or cannot be routed are left out of the estimate and get their own per-item error. If any group's estimate is more than 80 % of `timeout_secs` (a tool argument, default 60 s), the call fails before any request is sent: a tool error with code `queue_full`, `items` (the items estimated), `estimated_secs`, `suggested_chunk_size` (what fits now, or after the backlog drains: the longest prefix of the estimated items whose own per-group estimate fits, so resubmitting that many items is not refused again however uneven their sizes), and `retry_after_secs` (0 = now). When not even one item fits an idle backend, no chunk size helps: the error also has `required_timeout_secs`, the `timeout_secs` that lets one item run now. Cache hits are not subtracted, so the estimate is high when many items are cached.
- Item bodies are built when their item starts, not up front: at most the in-flight window of bodies, and at most 64 MiB of them (one body always runs), are held at once.
- `tools/call` `arguments`: omitted means `{}`; any other non-object (null, array, scalar) is JSON-RPC error -32602.
- `forecast` (stage 7): the arguments are a TSFM request (`inputs` present; `model` defaults to `forecast`), or `csv_path` with `time_column`, `value_column`, and optional `id_column`, where the other arguments (`model`, `parameters`, `metadata`) are the request template and the grid is built as in `s1 forecast --csv` (step from `parameters.frequency`, else inferred). The request goes through the `/v1/forecast` ingress in the interactive lane; the result is the TSFM response (output schema requires `outputs`; per output, `quantile_predictions`, `timestamps`, and `metadata` may be null, as TSFM allows). Ingress and provider errors are tool errors with the TSFM `code`, the message, and `status` (no backend: `backend_unavailable`, 503). `csv_path` is read on both listeners, unlike `s1_path` images (Unix socket only): MCP over TCP is loopback-only and needs the bearer token, and this table lists `csv_path` as an MCP input. The path must be absolute and name a regular file of at most 64 MiB (a symlink to a regular file is followed; a FIFO or directory is refused).
- Progress: with `_meta.progressToken`, the reply becomes `text/event-stream` (`event: message`, `data: <json>`) when the first item finishes: `notifications/progress` (`progress` = items done, `total` = items, at most one every 100 ms), then the result. A call that ends before its first progress event gets plain JSON. Closing the stream cancels the call: requests not yet sent are not sent.

## Cache, logging, secrets, config

No request payloads are stored. The cache keeps only key hashes and responses; logs keep metadata only.

**Cache**

- Store: one embedded key-value file (`redb` or SQLite) in `~/.cache/s1/`.
- Entry: key hash (see Model catalog and routing) → response bytes + resolved model + time.
- Size cap (default 1 GiB) with LRU eviction. No TTL by default; `--no-cache` per request and `s1 util cache clear`.
- As built: `~/.cache/s1/cache.redb`; `[cache] enabled` and `max_bytes` in `config.toml`; `POST /cache/clear` on the daemon. `max_bytes` caps the file on disk: stored responses get 3/4 of it, each charged 256 bytes for its keys and index rows (the rest is room for redb pages and freed space), the file is compacted when it grows past the cap (plus 2 MiB for the fixed size of an empty redb file) and, if it is still over, the oldest entries go until it fits, and `s1 util cache clear` and a daemon start with a lower cap evict and compact at once. `/status` reports `bytes` (responses plus the per-entry charge) and `file_bytes`. The key also holds the long-context opt-in. Last resolved versions are in `~/.local/state/s1/versions.json`.
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

1. **Skeleton:** daemon on Unix socket and TCP, catalog loading, auth file, `s1 raw` passthrough (now `s1 util raw`) to TypeSafe, logs.
2. **Decide:** systemone ingress, Jev and Clef backends, router with aliases, `s1 decide` single and streaming, `--flat`.
3. **Protection:** limiters, AIMD, retries, breaker, bounded queues with two lanes, `/status`, cache. Property tests for all of these.
4. **OpenAI Decisions:** ingress and backend, translation both ways, SDK conformance tests.
5. **Extract:** Clairic ingress and backend, `s1 extract`, fixtures.
6. **MCP:** stateless `/mcp` with `decide`, `extract`, `models` and list inputs.
7. **Forecast format:** TSFM ingress and CLI CSV helper, ready for a backend.

## Open questions

- ~~Does `rmcp` support the 2026-07-28 stateless transport, and can a stateless call stream progress?~~ Answered in milestone 6: yes (`rmcp` 3.0 and later, stateless by default, progress on the response stream). s1 uses a small hand-written handler anyway (see "As built (stage 6)" under MCP).
- ~~Is Clef fully systemone-compatible in practice (limits, error shapes, truncation)?~~ Answered in milestone 2 (live calls, 2026-10-20): answers and usage have the systemone shape; requests differ (no `box`, no `assets`, unknown fields rejected, 1 to 64 questions, at least 2 options and 2 levels, non-blank `instructions` required, question names `[A-Za-z0-9_.-]{1,100}`). Errors use the Workers AI envelope. Over-context input is rejected (413 code 5021 when `bytes / 4` exceeds the context, image data not counted; 422 code 3030 "Request requires N tokens"), not silently truncated. Fixtures: `tests/fixtures/workers-ai/`.
- Real rate limits for each provider account, to replace the placeholder numbers.
- Item cap for MCP list calls (1,000 is a guess). Measure against client timeouts.
- Which forecast backend comes first: a hosted TSFM provider or a local worker?
- Name collision for `s1` on crates.io and Homebrew; fallback crate name `s1-gateway`.

## Sources

- [TypeSafe System One API reference](https://docs.typesafe.ai/api/system-one.md)
- [OpenAI Decisions API reference](https://developers.openai.com/api/reference/typescript/resources/decisions/index.md)
- [Baseten System One reference](https://docs.baseten.co/reference/inference-api/system-one)
- [Baseten decision models](https://docs.baseten.co/inference/model-apis/decision-models)
- [baselabs models](https://huggingface.co/baselabs)
- [Cloudflare Clairic model page](https://developers.cloudflare.com/workers-ai/models/clairic/)
- [Cloudflare Clairic changelog](https://developers.cloudflare.com/changelog/post/2026-10-19-clairic-workers-ai/)
- [TSFM.ai](https://github.com/tsfm-ai)
- [Sidus (TSFM-compatible server)](https://pypi.org/project/sidus/)
- [Timestral model card](https://huggingface.co/mistralai/Timestral-209M)
- [MCP 2026-07-28 specification release](https://blog.modelcontextprotocol.io/posts/2026-07-28/)
- [opencode providers](https://v2.opencode.ai/docs/providers)
- [pi models.json with a gateway](https://docs.merge.dev/merge-gateway/coding-agents/pi.md)
