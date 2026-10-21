# s1-timestral

A local forecast server for [Timestral](https://github.com/mistralai/timestral).
It speaks the TSFM.ai `/v1/forecast` format, so s1 can use it as a provider
with `api = "tsfm"`. It runs on CPU. Timestral-209M loads in about 10 seconds
and answers a small request in under 0.2 seconds.

## Differences from TSFM.ai

- `mean` is the median. Timestral predicts quantiles only.
- `quantile_levels` must be among Timestral's 21 levels (0.01, 0.05, 0.1, 0.15, …, 0.95, 0.99).
  The default is `[0.1, 0.5, 0.9]`.
- A `future_covariates` entry needs a `past_covariates` entry with the same name.
- The server refuses `static_covariates`, `num_samples`, `temperature` and `top_p`.

## Install

```sh
uv tool install ./workers/timestral
openssl rand -hex 32 | s1 auth set tsfm   # the bearer key; the worker reads it from ~/.s1/auth.json
s1-timestral --port 7342                  # or S1_TIMESTRAL_KEY=... s1-timestral
```

Options: `--model <hf repo>` (you can use it more than one time; the default is
`mistralai/Timestral-209M`), `--device cpu|cuda`, `--host` and `--port`. The
matching environment variables are `S1_TIMESTRAL_MODELS`, `S1_TIMESTRAL_DEVICE`,
`S1_TIMESTRAL_HOST` and `S1_TIMESTRAL_PORT`.

## Connect s1

Add this to `~/.config/s1/models.toml`, then restart the daemon:

```toml
[providers.tsfm]
base_url = "http://127.0.0.1:7342"
api = "tsfm"
auth = "tsfm"

[[providers.tsfm.models]]
id = "timestral-209m"
upstream = "mistralai/Timestral-209M"

[aliases.forecast]
models = ["tsfm/timestral-209m"]
```

## Test

```sh
cd workers/timestral && uv run pytest
# against the running worker, through s1:
S1_LIVE=1 S1_LIVE_TSFM_BASE_URL=http://127.0.0.1:7342 \
  S1_LIVE_TSFM_UPSTREAM=mistralai/Timestral-209M \
  cargo test --test live live_forecast -- --ignored
```
