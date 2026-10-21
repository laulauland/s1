import numpy as np
import pytest
from fastapi.testclient import TestClient

from s1_timestral import create_app

LEVELS = (0.01, 0.05, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 0.95, 0.99)
KEY = "test-key"


class Fake:
    quantile_levels = LEVELS
    max_context = 8

    def __init__(self):
        self.calls = []

    def forecast(self, context, horizon, past, known):
        self.calls.append((context, horizon, past, known))
        last = np.nanmean(context, axis=1)  # (V,)
        q = np.array(LEVELS)
        return last[:, None, None] + q[None, None, :] + np.zeros((1, horizon, 1))


@pytest.fixture
def fake():
    return Fake()


@pytest.fixture
def client(fake):
    return TestClient(create_app({"m": fake}, KEY))


def post(client, body, key=KEY):
    return client.post("/v1/forecast", json=body, headers={"Authorization": f"Bearer {key}"})


def test_basic_shape(client):
    r = post(client, {"model": "m", "inputs": [{"target": [[1], [2], [3]]}], "parameters": {"prediction_length": 2}})
    assert r.status_code == 200, r.text
    b = r.json()
    assert b["object"] == "forecast" and b["horizon"] == 2 and b["quantile_levels"] == [0.1, 0.5, 0.9]
    out = b["outputs"][0]
    assert out["mean"] == [[2.5], [2.5]]  # median level
    assert [p["level"] for p in out["quantile_predictions"]] == [0.1, 0.5, 0.9]
    assert out["quantile_predictions"][0]["values"] == [[2.1], [2.1]]
    assert out["timestamps"] is None
    assert b["usage"] == {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5}


def test_auth(client):
    assert post(client, {"model": "m"}, key="wrong").json()["code"] == "invalid_credential"
    assert client.post("/v1/forecast", json={}).status_code == 401


def test_nulls_and_channels(client, fake):
    r = post(client, {"model": "m", "inputs": [{"target": [[1, 10], [None, 20], [3, None]]}], "parameters": {"prediction_length": 1}})
    assert r.status_code == 200, r.text
    ctx = fake.calls[0][0]
    assert ctx.shape == (2, 3) and np.isnan(ctx[0, 1]) and np.isnan(ctx[1, 2])
    assert r.json()["outputs"][0]["mean"] == [[2.5, 15.5]]


def test_timestamps(client):
    r = post(client, {"model": "m", "inputs": [{"target": [[1], [2]], "start": "2026-01-01T00:00:00Z"}],
                      "parameters": {"prediction_length": 2, "frequency": "h"}})
    assert r.json()["outputs"][0]["timestamps"] == ["2026-01-01T02:00:00Z", "2026-01-01T03:00:00Z"]


def test_covariates(client, fake):
    r = post(client, {"model": "m", "inputs": [{"target": [[1], [2]],
        "past_covariates": {"temp": [5, 6], "other": [1, None]}, "future_covariates": {"temp": [7]}}],
        "parameters": {"prediction_length": 1}})
    assert r.status_code == 200, r.text
    _, _, past, known = fake.calls[0]
    assert past.shape == (1, 2) and known.tolist() == [[5, 6, 7]]


def test_future_without_past(client):
    r = post(client, {"model": "m", "inputs": [{"target": [[1]], "future_covariates": {"t": [1]}}], "parameters": {"prediction_length": 1}})
    assert r.status_code == 400 and r.json()["code"] == "invalid_argument" and r.json()["endpoint"] == "/v1/forecast"


def test_truncation(client, fake):
    r = post(client, {"model": "m", "inputs": [{"target": [[i] for i in range(20)]}], "parameters": {"prediction_length": 1, "context_length": 4}})
    b = r.json()
    assert b["truncated_from"] == 20 and b["max_context_length"] == 4 and fake.calls[0][0].shape == (1, 4)


@pytest.mark.parametrize("body,needle", [
    ({"model": "m", "inputs": [{"target": [1, 2]}]}, "2D"),
    ({"model": "m", "inputs": [{"target": [[1], [1, 2]]}]}, "channels"),
    ({"model": "m", "inputs": [{"target": [[None]]}]}, "no observed"),
    ({"model": "m", "inputs": [{"target": [[1]]}], "parameters": {"quantile_levels": [0.15]}}, "not available"),
    ({"model": "m", "inputs": [{"target": [[1]]}], "parameters": {"num_samples": 3}}, "num_samples"),
    ({"model": "m", "inputs": [{"target": [[1]]}], "parameters": {"prediction_length": 0}}, "prediction_length"),
    ({"model": "m", "inputs": [{"target": [[1]], "static_covariates": {"a": 1}}]}, "static"),
])
def test_invalid(client, body, needle):
    r = post(client, body)
    assert r.status_code == 400 and r.json()["code"] == "invalid_argument" and needle in r.json()["error"]


def test_unknown_model(client):
    r = post(client, {"model": "x", "inputs": [{"target": [[1]]}]})
    assert r.status_code == 404 and r.json()["code"] == "request_rejected"


def test_bad_json(client):
    r = client.post("/v1/forecast", content=b"{", headers={"Authorization": f"Bearer {KEY}"})
    assert r.json()["code"] == "invalid_argument"
