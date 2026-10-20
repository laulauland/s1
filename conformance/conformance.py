"""SDK conformance: the official OpenAI and TypeSafe Python SDKs work
unchanged against s1 when only the base URL (and the key: s1's local TCP
token) changes.

Run by `tests/conformance.rs` against an s1 daemon with mock providers:

    S1_BASE_URL=http://127.0.0.1:PORT S1_TOKEN=... uv run --frozen conformance.py

The mock providers decide deterministically (see `tests/common/mod.rs`):
a predicate or noul is 0.9 when the text holds "yes", a choice picks the
first option, a score picks the top level. With `S1_CONFORMANCE_LIVE=1`
the same calls go to real providers through a real s1, and only shapes
are checked.
"""

import base64
import os
import struct
import sys
import zlib

import openai
import typesafe_sdk as ts

BASE = os.environ["S1_BASE_URL"].rstrip("/")
TOKEN = os.environ["S1_TOKEN"]
LIVE = os.environ.get("S1_CONFORMANCE_LIVE") == "1"

failures: list[str] = []


def check(name: str, fn) -> None:
    try:
        fn()
    except Exception as e:  # noqa: BLE001 - report every failure
        failures.append(f"{name}: {type(e).__name__}: {e}")
        print(f"FAIL {name}: {type(e).__name__}: {e}")
    else:
        print(f"ok   {name}")


# ------------------------------------------------------------------ OpenAI

oa = openai.OpenAI(base_url=f"{BASE}/v1", api_key=TOKEN, max_retries=0)

QUESTIONS = [
    {"type": "predicate", "name": "billing", "instructions": "Is this message about billing?"},
    {
        "type": "choice",
        "instructions": "Which department?",
        "choices": [{"value": "billing", "description": "Payments"}, {"value": "technical"}, {"value": True}],
    },
    {
        "type": "score",
        "name": "urgency",
        "instructions": "How urgent?",
        "levels": [{"label": "Low"}, {"label": "Medium", "description": "this week"}, {"label": "High"}],
    },
]


def openai_answers(model: str) -> None:
    raw = oa.decisions.with_raw_response.create(
        model=model, input="I was charged twice for my order. yes", questions=QUESTIONS
    )
    d = raw.parse()
    assert raw.headers.get("s1-backend"), "no s1-backend header"
    assert [a.type for a in d.answers] == ["predicate", "choice", "score"], d.answers
    assert [a.name for a in d.answers] == ["billing", None, "urgency"], d.answers
    choice = d.answers[1]
    values = [p.value for p in choice.probabilities]
    assert values == ["billing", "technical", True], values
    assert isinstance(values[2], bool)
    assert [p.label for p in d.answers[2].probabilities] == ["Low", "Medium", "High"]
    assert d.usage.input_tokens > 0
    if not LIVE:
        assert d.answers[0].probability == 0.9 or model.startswith("openai/"), d.answers[0]
        assert choice.choice == "billing"


def png_data_url(size: int = 512) -> str:
    """A grey RGB PNG. Live Jev rejects small images ("Invalid image
    dimensions." for 256x256 and 384x185; 512x512 works)."""

    def chunk(kind: bytes, data: bytes) -> bytes:
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    rows = b"".join(b"\x00" + b"\x80\x80\x80" * size for _ in range(size))
    png = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(rows))
        + chunk(b"IEND", b"")
    )
    return "data:image/png;base64," + base64.b64encode(png).decode()


def openai_images() -> None:
    png = png_data_url()
    raw = oa.decisions.with_raw_response.create(
        model="decide",
        input=[
            {
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "Is there a cat?"},
                    {"type": "input_image", "image_url": png},
                ],
            }
        ],
        questions=[{"type": "predicate", "name": "cat", "instructions": "Is there a cat?"}],
    )
    d = raw.parse()
    assert raw.headers["s1-model"] == "typesafe/jev-latest", raw.headers["s1-model"]
    assert d.answers[0].name == "cat"


def openai_error(model: str, status: type, code: str, questions=QUESTIONS) -> None:
    try:
        oa.decisions.create(model=model, input="x", questions=questions)
    except status as e:
        assert e.code == code, (e.code, e.body)
        return
    raise AssertionError(f"no {status.__name__}")


check("openai: native gpt-6-luna", lambda: openai_answers("openai/gpt-6-luna"))
check("openai: decide alias via translation", lambda: openai_answers("decide"))
check("openai: jev via translation", lambda: openai_answers("typesafe/jev-latest"))
check("openai: image input goes to jev", openai_images)
check(
    "openai: wrong family is BadRequestError",
    lambda: openai_error("workers-ai/clairic", openai.BadRequestError, "wrong_family"),
)
check(
    "openai: 11 levels is UnprocessableEntityError",
    lambda: openai_error(
        "decide",
        openai.UnprocessableEntityError,
        "unsupported",
        [{"type": "score", "instructions": "?", "levels": [{"label": str(i)} for i in range(11)]}],
    ),
)


def openai_bad_token() -> None:
    bad = openai.OpenAI(base_url=f"{BASE}/v1", api_key="not-the-token", max_retries=0)
    try:
        bad.decisions.create(model="decide", input="x", questions=QUESTIONS)
    except openai.AuthenticationError:
        return
    raise AssertionError("no AuthenticationError")


check("openai: wrong s1 token is AuthenticationError", openai_bad_token)

# ---------------------------------------------------------------- TypeSafe

tsc = ts.TypeSafeClient(base_url=BASE, api_key=TOKEN, retry=ts.RetryPolicy(max_retries=0))

TS_QUESTIONS = {
    "billing": ts.Noul(instructions="Is this message about billing?", criteria={"true": "Money is involved"}),
    "tone": ts.Choice(instructions="What is the tone?", criteria={"angry": "Upset", "calm": None}),
    "severity": ts.Score(instructions="How severe?", criteria=["Minor", "Major", "Critical"]),
}

# Over 1 KiB: the SDK sends `Content-Encoding: zstd` (Python 3.14+).
LONG = "I was charged twice. yes. " + "The order page shows two payments. " * 40


def typesafe_answers(model: str) -> None:
    assert len(LONG) > 1024
    r = tsc.system_one(state=LONG, questions=TS_QUESTIONS, model=model)
    assert set(r.answers) == {"billing", "tone", "severity"}, r.answers
    assert list(r.answers) == ["billing", "tone", "severity"], list(r.answers)
    assert 0 <= r.nouls["billing"].noul <= 1
    assert r.choices["tone"].choice in {"angry", "calm"}
    assert r.scores["severity"].score >= 0
    if not LIVE:
        assert r.nouls["billing"].noul == 0.9
        assert r.choices["tone"].choice == "angry"


def typesafe_refusal_is_dropped_not_fatal() -> None:
    # OpenAI refuses on empty instructions (mock: always); s1 sends a
    # per-question error, which this SDK skips as an unknown answer type.
    raw = tsc.system_one(
        state="rate 7",
        questions={"ok": ts.Noul(instructions="Is this a rating? yes"), "blank": ts.Noul(instructions="")},
        model="openai/gpt-6-luna",
    )
    assert "ok" in raw.answers
    if not LIVE:
        assert "blank" not in raw.answers


def typesafe_unsupported() -> None:
    try:
        tsc.system_one(
            state="x",
            questions={"where": {"type": "box", "instructions": "Where?"}},
            model="openai/gpt-6-luna",
        )
    except ts.TypeSafeUnprocessableEntityError as e:
        assert "box" in str(e).lower() or "unsupported" in str(e).lower(), str(e)
        return
    raise AssertionError("no TypeSafeUnprocessableEntityError")


check("typesafe: decide alias (zstd body)", lambda: typesafe_answers("decide"))
check("typesafe: jev (zstd body)", lambda: typesafe_answers("typesafe/jev-latest"))
check("typesafe: openai via translation (zstd body)", lambda: typesafe_answers("openai/gpt-6-luna"))
check("typesafe: refusal does not break parsing", typesafe_refusal_is_dropped_not_fatal)
check("typesafe: box to openai is unprocessable", typesafe_unsupported)

if failures:
    print(f"\n{len(failures)} conformance check(s) failed", file=sys.stderr)
    sys.exit(1)
print("\nall conformance checks passed")
