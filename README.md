# s1

A local gateway for **System One models**: small, fast models that do not chat or
generate text. They answer a fixed question with a probability (Decide), pull typed
values out of a document (Extract), or forecast a numeric series (Forecast).

s1 is one daemon and one CLI on your machine. It puts every System One model behind
one address, so you can try a new model the day it ships by changing one name. It
also exposes the models as MCP tools, so your coding agent can reach for them while
it works, not only when you remember they exist.

```console
$ s1 decide -t commit.json "bump tokio to 1.54 and fix clippy warnings"
{"model":"clef-flash","answers":{"kind":{"type":"choice","choice":"chore",
  "probabilities":{"feat":0.0058,"fix":0.0428,"chore":0.9483,"docs":0.0031},
  "confidence":0.8682},"risky":{"type":"noul","noul":0.2109}}, ...}
```

## Why this exists

New System One models keep appearing: TypeSafe Jev, Cloudflare Clef and Clairic,
OpenAI Decisions (`gpt-6-luna`), and the baselabs models on Baseten. Each one has its own API, key, limits, and quirks.
Trying a new one usually means a new SDK and a throwaway script, and then you forget
about it.

s1 removes that cost:

- **One place to try models.** Point a template at `typesafe/jev-latest`,
  `workers-ai/clef`, or `openai/gpt-6-luna` and compare. Add a new provider in a
  TOML file.
- **Agents can use them.** The MCP tools `decide`, `extract`, `forecast`, and
  `models` are always in the agent's tool list. When you ask the agent to "look
  through these 400 issues" or "pull the amounts out of these invoices", it has a
  cheap, calibrated classifier at hand instead of reading every item itself.
- **Safe loops.** The daemon handles rate limits, retries, queues, and a response
  cache. A naive `for` loop or shell pipe does not overload a provider, and a rerun
  of the same data is free.

## What it does

| Family | Question it answers | Output | Models (built in) |
| --- | --- | --- | --- |
| **Decide** | Yes/no, pick one of N, score on a scale | Probabilities and confidence | `workers-ai/clef`, `workers-ai/clef-flash`, `typesafe/jev-latest`, `typesafe/jev-preview`, `openai/gpt-6-luna`, `baseten/mammoth-1`, `baseten/elephant-1` |
| **Extract** | "What is the total, the date, the vendor?" | Typed values with confidence, spans, and estimates (p10 / median / p90) | `workers-ai/clairic` |
| **Forecast** | "What comes next in this series?" | Quantile forecasts | None built in; add any TSFM.ai-compatible server |

Aliases `decide` and `extract` pick a sensible default (`clef-flash`, then Jev as a
fallback; `clairic`).

s1 does not invent its own API. Each family uses the provider's existing format:

| Endpoint | Format |
| --- | --- |
| `POST /v1/systemone` | [TypeSafe System One](https://docs.typesafe.ai/api/system-one.md) (canonical for Decide) |
| `POST /v1/decisions` | [OpenAI Decisions](https://developers.openai.com/api/reference/typescript/resources/decisions/index.md), translated to and from systemone |
| `POST /v1/extract` | [Cloudflare Clairic](https://developers.cloudflare.com/workers-ai/models/clairic/) |
| `POST /v1/forecast` | [TSFM.ai](https://github.com/tsfm-ai) |
| `POST /v1/raw/{protocol}` | Pure passthrough |
| `POST /mcp` | MCP (2026-07-28, stateless) |
| `GET /v1/models`, `GET /status` | Catalog and backend state |

The `model` field selects the backend. Because the formats are translated, any
Decide request can go to any Decide model: a systemone request can run on
`gpt-6-luna`, and the official OpenAI and TypeSafe SDKs work with s1 as their base
URL.

What the daemon does for every request:

- **Routing.** An alias tries its models in order and skips any that is down or
  cannot take the request (too long, too many options, images).
- **Rate limits.** Per-provider token buckets (RPM and TPM), an in-flight cap that
  halves on a 429 and grows back (AIMD), retries with backoff, and a circuit breaker.
- **Queues.** A bounded queue per backend with an interactive lane ahead of a bulk
  lane. A full queue returns 429 with `Retry-After`.
- **Cache.** Successful answers are cached on disk (`~/.cache/s1/`, 1 GiB LRU),
  keyed by the request and the resolved model version. A new `-latest` model does
  not return stale answers.
- **No payload storage.** Logs hold metadata only.

## Install

Needs a recent Rust toolchain (edition 2024).

```sh
cargo install --path .
```

### Keys

s1 reads keys from `~/.s1/auth.json` (mode 0600). Add the providers you have:

```sh
s1 auth set typesafe                          # TypeSafe Jev
s1 auth set workers-ai --account-id <id>      # Cloudflare Clef and Clairic
s1 auth set openai                            # OpenAI Decisions
s1 auth set baseten                           # Baseten (baselabs mammoth-1, elephant-1)
s1 auth list
```

Environment variables override the file: `TYPESAFE_API_KEY`,
`CLOUDFLARE_API_TOKEN` + `CLOUDFLARE_ACCOUNT_ID`, `OPENAI_API_KEY`, `BASETEN_API_KEY`, `TSFM_API_KEY`.

### Daemon

The CLI starts the daemon on first use. MCP and HTTP clients need it running:

```sh
s1 util daemon start   # Unix socket for the CLI, http://127.0.0.1:7341 for everything else
s1 status              # queues, limits, breakers, cache size
s1 models              # models, aliases, availability
s1 util daemon logs
```

On Linux, let systemd run it instead. systemd then holds the socket and the TCP
port, so a restart refuses no connection, and a new `cargo install` restarts the
daemon by itself. Stop a daemon that runs now first (it holds the socket and the
port; an older `s1.service` without `s1.socket` must stop too):

```sh
s1 util daemon stop
systemctl --user stop s1.service 2>/dev/null
mkdir -p ~/.config/systemd/user
cp contrib/systemd/* ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now s1.socket s1-upgrade.path
```

On SIGTERM the daemon stops accepting, finishes open requests (up to 120 s), and
exits; new connections wait for the next daemon. A connection that is open but
has not sent its request yet gets 5 s to send it. An idle kept-alive connection
is closed; the client opens a new one, as HTTP keep-alive requires. With
`s1.socket` the TCP address comes from the unit (`ListenStream=127.0.0.1:7341`),
not from `config.toml`, and `s1 util daemon stop` refuses: use
`systemctl --user stop s1.socket s1.service`.

### Add it to your agent (MCP)

TCP clients need the local bearer token:

```sh
claude mcp add --scope user --transport http s1 http://127.0.0.1:7341/mcp \
  --header "Authorization: Bearer $(s1 auth token)"
```

Any MCP client that speaks the 2026-07-28 stateless HTTP transport works the same
way: URL `http://127.0.0.1:7341/mcp`, header `Authorization: Bearer <token>`.

## Usage

### Templates

A template is a normal provider request without the input. There is no s1 schema.

`commit.json` (Decide, systemone format):

```json
{
  "model": "decide",
  "questions": {
    "kind": {
      "type": "choice",
      "instructions": "What kind of commit is this?",
      "criteria": {
        "feat": "new feature",
        "fix": "bug fix",
        "chore": "tooling, deps, cleanup",
        "docs": "documentation"
      }
    },
    "risky": { "type": "noul", "instructions": "Could this change break existing users?" }
  }
}
```

Question types: `noul` (yes/no; the answer is P(yes)), `choice` (2 to 255 named
options), `score` (an ordered list of up to 10 levels).

`invoice.json` (Extract, Clairic format):

```json
{
  "model": "extract",
  "fields": {
    "vendor": { "type": "text" },
    "total":  { "type": "number", "decimals": 2, "unit": "EUR" },
    "due":    { "type": "date" }
  }
}
```

Field types: `text`, `number`, `date`, `time`, `datetime`, `duration`, `list`,
`estimate` (a distribution with `min`, `max`), and `forecast`.

### One input

```sh
s1 decide  -t commit.json "fix off-by-one in queue drain rate"
s1 decide  -t commit.json -m openai/gpt-6-luna "fix off-by-one in queue drain rate"
s1 extract -t invoice.json < invoice.txt
```

```console
$ s1 extract -t invoice.json --flat "Invoice from Hetzner Online GmbH. Amount due: 42,50 EUR by 3 November 2026."
{"vendor":"Hetzner Online GmbH","vendor__conf":1,"total":42.5,"total__conf":0.99996,
 "due":"2026-11-03","due__conf":0.999998, ...}
```

### Many inputs: JSON lines in, JSON lines out

When stdin is a pipe, `decide` and `extract` become stream filters. One line in,
one line out, in input order.

```sh
git log -n 200 --format='%h%x09%s' \
  | jq -Rc 'split("\t") | {id: .[0], msg: .[1]}' \
  | s1 decide -t commit.json --state-field msg --keep msg --flat \
  | duckdb -c "select kind, count(*), avg(risky) from read_json('/dev/stdin') group by 1"
```

- `--state-field msg` sends only that field as the state (default: the whole line).
- `--keep a,b` copies input fields to the output; `--keep '*'` copies the whole line.
- `--flat` writes flat columns: `kind`, `kind__conf`, `kind__p__fix`, …, `risky`.
- `--image-field` reads image paths from a field.
- One failed line writes `{"id": …, "error": …}` and the stream continues.
- The CLI keeps as many requests in flight as the backend allows, and stops
  reading stdin when that window is full.

**Cascades.** With `--keep '*'`, each step's output is the next step's input, so
you ask the narrow (or more expensive) question only where it matters:

```sh
cat tickets.jsonl \
  | s1 decide  -t topic.json   --state-field body --keep '*' --flat \
  | jq -c 'select(.topic == "billing" and .topic__conf > 0.8)' \
  | s1 decide  -t billing.json --state-field body --keep '*' --flat \
  | jq -c 'select(.threatens_cancel > 0.8)' \
  | s1 extract -t refund.json  --state-field body --keep '*' --flat \
  > at_risk.jsonl
```

### Forecast

```sh
s1 forecast --csv sales.csv --id sku --time ds --value y -h 28 -q 0.1,0.5,0.9 --flat
s1 forecast --csv sales.csv --time ds --value y -h 28 --print-request   # no call
```

The CSV helper puts each series on a regular grid (inferred step, or `--freq`)
and sends gaps as missing values. No forecast backend is built in. Add any
TSFM.ai-compatible server (TSFM.ai itself, or a self-hosted one such as
[Sidus](https://pypi.org/project/sidus/)) in `~/.config/s1/models.toml`:

```toml
[providers.tsfm]
base_url = "https://<your TSFM server>"   # s1 calls {base_url}/v1/forecast
api = "tsfm"
auth = "tsfm"

[[providers.tsfm.models]]
id = "chronos-bolt-small"
upstream = "amazon/chronos-bolt-small"

[aliases.forecast]
models = ["tsfm/chronos-bolt-small"]
```

### From an agent (MCP tools)

| Tool | Input | Output |
| --- | --- | --- |
| `decide` | systemone `template` + `states: [{id, state}]` (1 to 1,000) | `results: [{id, ok, answers \| error}]` |
| `extract` | Clairic `template` + `states: [{id, state, images?}]` (1 to 1,000) | same shape |
| `forecast` | a TSFM request, or `csv_path` + column names | TSFM response |
| `models` | none | catalog, aliases, state |

Each item succeeds or fails on its own. Before a long list starts, s1 estimates the
run time; if the call would pass `timeout_secs` (default 60), it is refused with a
`suggested_chunk_size`, so the agent can split the list.

The lists are meant for code. If an agent pastes 1,000 states into a tool call by
hand, it fills its own context. Better: the agent writes a short script that
builds the list (or pipes JSON lines through the CLI) and only reads the summary.

### From code (HTTP)

The daemon speaks the providers' own formats, so the official SDKs work unchanged:

```python
from openai import OpenAI
client = OpenAI(base_url="http://127.0.0.1:7341/v1", api_key="<s1 auth token>")
client.decisions.create(model="decide", input="...", questions=[...])
```

Useful headers: `Cache-Control: no-cache` (refresh) or `no-store` (skip the cache),
`s1-lane: bulk`. Responses carry `s1-backend`, `s1-model`, `s1-cache`, and
`s1-retries`.

## Trying a new model

1. Add the provider and model to `~/.config/s1/models.toml` (same shape as
   `src/catalog/builtin.toml`; any field there can be overridden), and its key with
   `s1 auth set <provider>`.
2. Run your existing templates against it with `-m provider/model`.
3. Compare against the model you use today on the same file:

```sh
for m in workers-ai/clef-flash typesafe/jev-latest openai/gpt-6-luna; do
  s1 decide -t commit.json -m "$m" --state-field msg --keep msg --flat < commits.jsonl \
    | jq -c --arg m "$m" '. + {model: $m}'
done | duckdb -c "pivot read_json('/dev/stdin') on model using first(kind) group by msg"
```

The cache keeps every answer, so you can rerun the comparison as you change the
query at no cost.

## Use cases for one developer with an agent

These models are not chat models. They are cheap, fast, and give you a number you
can sort, filter, and threshold. That suits work where an agent would otherwise
read every item itself, or where you want a score, not an opinion. Some ideas:

**Triage a pile of text**

- *GitHub issues and PRs.* "Which of these 300 open issues are bug reports with a
  reproduction? Which are duplicates of #123? Which are questions?" Score them, sort
  by probability, read the top 20.
- *Your own TODOs and notes.* Classify `TODO`/`FIXME` comments or a notes folder by
  area and urgency, and get a short list to act on.
- *Email or Slack exports.* "Does this message need a reply from me?" as a `noul`
  over a week of messages.

**Code and repository analysis**

- *Commit history.* Label commits (feat / fix / chore / revert, risky or not) to
  draft a changelog, or to find when a class of bug fixes started.
- *Log lines and errors.* Group error messages by probable cause, or score "is this
  line worth a human look?" across a big log before the agent reads any of it.
- *Dependency and config review.* "Does this changelog entry mention a breaking
  change?" for every dependency bump in a lockfile diff.
- *Test failures.* "Flaky, environment, or real regression?" over a CI history.

**Data analysis with the agent**

- *Add a column to a dataset.* Let the agent pipe a CSV or JSONL through
  `s1 decide --flat` to add sentiment, category, or "mentions pricing" columns, then
  analyze with duckdb. This turns a "read and summarize" task into a query.
- *Pull structure out of documents.* Invoices, receipts, bank statements, job
  posts, release notes: `s1 extract` turns them into rows with dates, amounts, and
  vendors, with confidence and spans you can check.
- *Estimates instead of guesses.* `estimate` fields give a distribution (p10,
  median, p90): "how many hours of work does this issue describe?", "what is the
  salary range in this posting?"
- *Quick forecasts.* With a forecast backend added, point the agent at a CSV of
  daily signups, API costs, or disk use and get quantile forecasts without writing a
  model.

**Agent self-checks**

- *Cheap guards.* Before the agent posts a comment or sends a message, a `noul`
  check: "Does this text contain a secret or a personal email address?"
- *Pick the right path.* Route a request to a playbook with a `choice` question
  instead of a long prompt.
- *Grade the agent's own output.* Score summaries or answers against a rubric over
  many runs, and compare prompts or models by the score distribution.

**Model evaluation (the original reason for s1)**

- Keep a small labeled set for each use case above (`id`, input, expected label).
  When a new model comes out, run the set through it with `-m` and compare the
  agreement and confidence with the model you use now. The cache makes reruns free.

## Files

| Path | Holds |
| --- | --- |
| `~/.s1/auth.json` | Provider keys and the local bearer token (mode 0600) |
| `~/.config/s1/config.toml` | Listener address, queue sizes, cache size, log level |
| `~/.config/s1/models.toml` | Your providers, models, aliases, and rate limits |
| `~/.cache/s1/cache.redb` | Response cache (`s1 util cache clear`) |
| `~/.local/state/s1/` | Logs (7 days), PID, last resolved model versions |
| `$XDG_RUNTIME_DIR/s1.sock` | Unix socket for the CLI (mode 0600) |

The rate limits in the built-in catalog are placeholders. Set your account's real
limits in `models.toml` under `[limits.<provider>]` (`rpm`, `tpm`,
`max_in_flight`).

## Development

```sh
cargo test                                   # unit, property, and mock-backend tests
S1_CONFORMANCE=1 cargo test --test conformance -- --ignored   # official SDKs against mocks (uses uv)
S1_LIVE=1 cargo test --test live -- --ignored                  # small live calls; needs keys
```

The design, the provider quirks, and every as-built decision are in
[`docs/spec.md`](docs/spec.md).

## Status

A personal tool, built in seven stages (see the commit history). Providers that are
previews (Clairic, OpenAI Decisions, Jev `box`) can change format; recorded fixtures
in `tests/fixtures/` catch that.
