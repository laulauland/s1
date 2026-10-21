"""A local TSFM.ai-compatible forecast server for Timestral.

s1 talks to it as a provider with `api = "tsfm"`: `POST /v1/forecast` with a
bearer key, the TSFM request and response shapes, and the TSFM error body.

Timestral differences from the TSFM contract:
- `mean` is the median (Timestral predicts quantiles only).
- `quantile_levels` must be among Timestral's 21 levels.
- `static_covariates`, `num_samples`, `temperature` and `top_p` are refused
  when set, because Timestral cannot use them.
"""

from __future__ import annotations

import json
import math
import os
import secrets
import time
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Protocol

import numpy as np
from fastapi import FastAPI, Request
from fastapi.responses import JSONResponse

ENDPOINT = "/v1/forecast"
DEFAULT_LEVELS = [0.1, 0.5, 0.9]
DEFAULT_HORIZON = 24
MAX_HORIZON = 4096
MAX_BODY = 64 * 1024 * 1024


class ApiError(Exception):
    def __init__(self, status: int, code: str, message: str):
        super().__init__(message)
        self.status, self.code, self.message = status, code, message


def bad(message: str) -> ApiError:
    return ApiError(400, "invalid_argument", message)


class Forecaster(Protocol):
    quantile_levels: tuple[float, ...]
    max_context: int

    def forecast(
        self, context: np.ndarray, horizon: int, past: np.ndarray | None, known: np.ndarray | None
    ) -> np.ndarray:
        """context (V, T) with NaN for missing; returns (V, H, Q)."""


@dataclass
class TimestralModel:
    inner: Any

    @property
    def quantile_levels(self) -> tuple[float, ...]:
        return tuple(float(q) for q in self.inner.quantile_levels)

    @property
    def max_context(self) -> int:
        return int(self.inner.max_context)

    def forecast(self, context, horizon, past, known):
        out = self.inner.forecast(
            context[None],
            prediction_length=horizon,
            past_covariates=None if past is None else past[None],
            known_covariates=None if known is None else known[None],
        )
        return out.quantiles[0].cpu().numpy()


def load_timestral(repo_id: str, device: str) -> TimestralModel:
    from timestral import TimestralForecaster

    return TimestralModel(TimestralForecaster.from_pretrained(repo_id, device=device))


# ---------------------------------------------------------------- request


def _number(v: Any, where: str) -> float:
    if v is None:
        return math.nan
    if isinstance(v, bool) or not isinstance(v, (int, float)) or not math.isfinite(v):
        raise bad(f"{where} must be a finite number or null")
    return float(v)


def _target(raw: Any, i: int) -> np.ndarray:
    """TSFM target [T][V] -> (V, T) float array, null -> NaN."""
    if not isinstance(raw, list) or not raw:
        raise bad(f"inputs[{i}].target must be a non-empty 2D array [time][channel]")
    width = None
    rows = []
    for t, row in enumerate(raw):
        if not isinstance(row, list) or not row:
            raise bad(f"inputs[{i}].target must be 2D: [[v], [v], ...] (row {t} is not an array)")
        if width is None:
            width = len(row)
        elif len(row) != width:
            raise bad(f"inputs[{i}].target rows must all have {width} channels (row {t} has {len(row)})")
        rows.append([_number(v, f"inputs[{i}].target[{t}]") for v in row])
    arr = np.asarray(rows, dtype=np.float32).T
    if not np.isfinite(arr).any():
        raise bad(f"inputs[{i}].target has no observed values")
    return arr


def _series(raw: Any, length: int, where: str, nulls: bool) -> np.ndarray:
    if not isinstance(raw, list) or len(raw) != length:
        raise bad(f"{where} must be an array of {length} values")
    vals = [_number(v, where) for v in raw]
    if not nulls and any(math.isnan(v) for v in vals):
        raise bad(f"{where} must not contain null")
    return np.asarray(vals, dtype=np.float32)


def _covariates(inp: dict, i: int, t: int, horizon: int):
    past = inp.get("past_covariates") or {}
    future = inp.get("future_covariates") or {}
    if not isinstance(past, dict) or not isinstance(future, dict):
        raise bad(f"inputs[{i}] covariates must be objects of name -> values")
    if inp.get("static_covariates"):
        raise bad("static_covariates are not supported by Timestral")
    known_rows, past_rows = [], []
    for name in sorted(future):
        if name not in past:
            raise bad(
                f"inputs[{i}].future_covariates.{name} needs past_covariates.{name} over the history too"
            )
        hist = _series(past[name], t, f"inputs[{i}].past_covariates.{name}", nulls=True)
        ahead = _series(future[name], horizon, f"inputs[{i}].future_covariates.{name}", nulls=False)
        known_rows.append(np.concatenate([hist, ahead]))
    for name in sorted(past):
        if name not in future:
            past_rows.append(_series(past[name], t, f"inputs[{i}].past_covariates.{name}", nulls=True))
    stack = lambda rows: np.stack(rows) if rows else None  # noqa: E731
    return stack(past_rows), stack(known_rows)


def _timestamps(start: Any, freq: Any, t: int, horizon: int) -> list[str] | None:
    if start is None or freq is None:
        return None
    import pandas as pd

    try:
        idx = pd.date_range(start=pd.Timestamp(start), periods=t + horizon, freq=freq)
    except (ValueError, TypeError) as e:
        raise bad(f"cannot build timestamps from start {start!r} and frequency {freq!r}: {e}")
    out = []
    for ts in idx[t:]:
        if ts.tzinfo is not None and ts.utcoffset().total_seconds() == 0:
            out.append(ts.strftime("%Y-%m-%dT%H:%M:%SZ"))
        else:
            out.append(ts.isoformat())
    return out


def _round(x: np.ndarray) -> list:
    return np.round(x.astype(np.float64), 6).tolist()


def forecast(body: dict, models: dict[str, Forecaster]) -> dict:
    started = time.perf_counter()
    if not isinstance(body, dict):
        raise bad("the body must be a JSON object")
    model_id = body.get("model")
    if not isinstance(model_id, str) or not model_id:
        raise bad("model is required")
    model = models.get(model_id)
    if model is None:
        raise ApiError(404, "request_rejected", f"model {model_id!r} is not served here; served: {sorted(models)}")
    inputs = body.get("inputs")
    if not isinstance(inputs, list) or not inputs:
        raise bad("inputs must be a non-empty array")
    params = body.get("parameters") or {}
    if not isinstance(params, dict):
        raise bad("parameters must be an object")
    for unsupported in ("num_samples", "temperature", "top_p"):
        if params.get(unsupported) is not None:
            raise bad(f"parameters.{unsupported} is not supported by Timestral")
    horizon = params.get("prediction_length", DEFAULT_HORIZON)
    if isinstance(horizon, bool) or not isinstance(horizon, int) or not 1 <= horizon <= MAX_HORIZON:
        raise bad(f"parameters.prediction_length must be an integer from 1 to {MAX_HORIZON}")
    levels = params.get("quantile_levels") or DEFAULT_LEVELS
    if not isinstance(levels, list) or not all(isinstance(q, (int, float)) for q in levels):
        raise bad("parameters.quantile_levels must be an array of numbers")
    known_levels = model.quantile_levels
    index = []
    for q in levels:
        match = [k for k, lv in enumerate(known_levels) if abs(lv - q) < 1e-9]
        if not match:
            raise bad(f"quantile level {q} is not available; Timestral produces {list(known_levels)}")
        index.append(match[0])
    context_length = params.get("context_length")
    if context_length is not None and (
        isinstance(context_length, bool) or not isinstance(context_length, int) or context_length < 1
    ):
        raise bad("parameters.context_length must be a positive integer")
    limit = min(context_length or model.max_context, model.max_context)
    median = list(known_levels).index(0.5)
    freq = params.get("frequency")

    outputs, points, truncated_from = [], 0, None
    for i, inp in enumerate(inputs):
        if not isinstance(inp, dict):
            raise bad(f"inputs[{i}] must be an object")
        target = _target(inp.get("target"), i)
        t = target.shape[1]
        past, known = _covariates(inp, i, t, horizon)
        if t > limit:
            truncated_from = max(truncated_from or 0, t)
            target = target[:, -limit:]
            past = None if past is None else past[:, -limit:]
            known = None if known is None else known[:, -(limit + horizon):]
        points += target.size
        q = model.forecast(target, horizon, past, known)  # (V, H, Q)
        to_rows = lambda a: _round(a.T)  # noqa: E731  (V, H) -> [H][V]
        outputs.append(
            {
                "mean": to_rows(q[:, :, median]),
                "quantile_predictions": [
                    {"level": float(levels[n]), "values": to_rows(q[:, :, k])} for n, k in enumerate(index)
                ],
                "samples": None,
                "timestamps": _timestamps(inp.get("start"), freq, t, horizon),
                "metadata": inp.get("metadata"),
            }
        )

    out_tokens = horizon * sum(len(o["mean"][0]) for o in outputs)
    resp = {
        "id": str(uuid.uuid4()),
        "object": "forecast",
        "created": int(time.time()),
        "model": model_id,
        "provider": "s1-timestral",
        "horizon": horizon,
        "prediction_length": horizon,
        "quantile_levels": [float(q) for q in levels],
        "input_points": points,
        "outputs": outputs,
        "usage": {"input_tokens": points, "output_tokens": out_tokens, "total_tokens": points + out_tokens},
        "latency_ms": round((time.perf_counter() - started) * 1000, 1),
        "metadata": body.get("metadata"),
    }
    if truncated_from is not None:
        resp["truncated_from"] = truncated_from
        resp["max_context_length"] = limit
    return resp


# ---------------------------------------------------------------- server


def read_key() -> str:
    """The bearer key: S1_TIMESTRAL_KEY, else the `tsfm` entry of ~/.s1/auth.json."""
    if key := os.environ.get("S1_TIMESTRAL_KEY"):
        return key
    home = Path(os.environ.get("S1_HOME") or Path.home() / ".s1")
    try:
        entry = json.loads((home / "auth.json").read_text())["tsfm"]
        return entry["key"]
    except (OSError, KeyError, TypeError, ValueError):
        raise SystemExit(f"no key: set S1_TIMESTRAL_KEY or run `s1 auth set tsfm` ({home}/auth.json)")


def create_app(models: dict[str, Forecaster], key: str) -> FastAPI:
    app = FastAPI(title="s1-timestral", docs_url=None, redoc_url=None, openapi_url=None)

    def error(e: ApiError) -> JSONResponse:
        return JSONResponse({"error": e.message, "code": e.code, "endpoint": ENDPOINT}, status_code=e.status)

    @app.get("/healthz")
    def healthz():
        return {"ok": True, "models": sorted(models)}

    @app.get("/v1/models")
    def list_models(request: Request):
        if not secrets.compare_digest(request.headers.get("authorization", ""), f"Bearer {key}"):
            return error(ApiError(401, "invalid_credential", "missing or invalid bearer key"))
        return {"object": "list", "data": [{"id": m, "object": "model"} for m in sorted(models)]}

    @app.post(ENDPOINT)
    async def post_forecast(request: Request):
        if not secrets.compare_digest(request.headers.get("authorization", ""), f"Bearer {key}"):
            return error(ApiError(401, "invalid_credential", "missing or invalid bearer key"))
        raw = await request.body()
        if len(raw) > MAX_BODY:
            return error(ApiError(413, "payload_too_large", f"body over {MAX_BODY} bytes"))
        try:
            body = json.loads(raw)
        except ValueError as e:
            return error(bad(f"body is not JSON: {e}"))
        try:
            return forecast(body, models)
        except ApiError as e:
            return error(e)

    return app


def main() -> None:
    import argparse

    import uvicorn

    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--host", default=os.environ.get("S1_TIMESTRAL_HOST", "127.0.0.1"))
    p.add_argument("--port", type=int, default=int(os.environ.get("S1_TIMESTRAL_PORT", "7342")))
    p.add_argument(
        "--model",
        action="append",
        help="Hugging Face repo id to serve (repeatable); default mistralai/Timestral-209M",
    )
    p.add_argument("--device", default=os.environ.get("S1_TIMESTRAL_DEVICE", "cpu"))
    a = p.parse_args()
    key = read_key()
    ids = a.model or os.environ.get("S1_TIMESTRAL_MODELS", "mistralai/Timestral-209M").split(",")
    models: dict[str, Forecaster] = {m: load_timestral(m, a.device) for m in ids}
    uvicorn.run(create_app(models, key), host=a.host, port=a.port, log_level="info")
