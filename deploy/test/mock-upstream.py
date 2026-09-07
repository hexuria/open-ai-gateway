#!/usr/bin/env python3
"""A stand-in for Anthropic's Messages API, for verification only.

Exists because the properties worth verifying — that a rolling restart severs no
stream, that a failing credential trips its breaker — are all about what the
gateway does over the *lifetime* of an upstream response. You cannot test that
against a real provider without paying for it and without the answer depending
on their load.

Standard library only, and deliberately so: it runs in-cluster from a ConfigMap
with no image to build, push, or keep in step with anything.

Behaviour is set by environment:
  MOCK_STREAM_SECONDS  how long a streamed response takes end to end (default 20)
  MOCK_CHUNKS          how many content deltas to spread across it (default 20)
  MOCK_FAIL_STATUS     if set, every POST returns this status instead
  MOCK_FAIL_FIRST      fail only the first N POSTs, then serve normally
  MOCK_FAIL_FOR_KEY    `credential=status` pairs, comma separated: only these
                       upstream credentials fail, each with its own status, and
                       every other credential is served. Matched against the
                       credential itself, whichever header carried it: a
                       `Bearer ` prefix is stripped first
  MOCK_FAIL_FIRST_CREDENTIAL
                       fail every POST from whichever credential POSTed first
                       since the last `/_credentials?reset=1`, with this status.
                       For failover: which of two equal accounts the scheduler
                       picks is its own business, so naming one in advance makes
                       the test depend on that choice

A request whose `thinking` block the real API would refuse is refused here too
(400): `type: "enabled"` requires `budget_tokens` of at least 1024 and strictly
below `max_tokens`. Without that, a gateway rendering an impossible budget
passes every check in this repository and fails in production.

GET /_seen returns the POST count as plain text, without incrementing it.
GET /_credentials returns one short hash per distinct credential seen — never
the value — so a script can assert that two accounts were both used without a
secret reaching CI output. `?reset=1` clears the list after reading it, which is
how a script scopes the question to one request rather than to the whole run.
"""

import hashlib
import json
import os
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

STREAM_SECONDS = float(os.environ.get("MOCK_STREAM_SECONDS", "20"))
CHUNKS = int(os.environ.get("MOCK_CHUNKS", "20"))
FAIL_STATUS = os.environ.get("MOCK_FAIL_STATUS")
FAIL_FIRST = int(os.environ.get("MOCK_FAIL_FIRST", "0"))
# Per-credential failure, because `MOCK_FAIL_STATUS` fails everyone and a
# gateway that never failed over then looks exactly like one that failed over to
# a second dead credential. It is a map rather than a single name because the
# two behaviours worth telling apart need different statuses: 408 is retried on
# the same account, 5xx moves to another one.
FAIL_FOR_KEY = {}
for _pair in filter(None, os.environ.get("MOCK_FAIL_FOR_KEY", "").split(",")):
    _cred, _, _status = _pair.partition("=")
    FAIL_FOR_KEY[_cred] = int(_status or FAIL_STATUS or 408)
FAIL_FIRST_CREDENTIAL = os.environ.get("MOCK_FAIL_FIRST_CREDENTIAL")

_lock = threading.Lock()
_seen = 0
_credentials = []


def _credential(headers):
    """The upstream credential a POST arrived with, recording a hash of it.

    The value never reaches the log or `/_credentials`. A short digest answers
    the only question a script asks — were these two requests made with
    different credentials — and a mock that printed the real thing would put a
    secret in CI output on every run.
    """
    raw = headers.get("x-api-key") or headers.get("authorization") or ""
    # `authorization` arrives as `Bearer <token>`, so matching the raw header
    # against a credential never matched for a bearer provider — MOCK_FAIL_FOR_KEY
    # worked only because Anthropic sends `x-api-key`. The scheme is not part of
    # the credential, and the digest should not depend on which header carried it.
    scheme, _, rest = raw.partition(" ")
    if scheme.lower() == "bearer" and rest:
        raw = rest
    if raw:
        digest = _digest(raw)
        with _lock:
            if digest not in _credentials:
                _credentials.append(digest)
    return raw


def _digest(raw):
    return hashlib.sha256(raw.encode()).hexdigest()[:8]


def _record_and_status(credential=""):
    """Count this POST, then decide whether it fails.

    Every POST increments, including health probes that POST an empty body.
    MOCK_FAIL_FOR_KEY fails only the credentials it names; MOCK_FAIL_STATUS
    fails every request; MOCK_FAIL_FIRST fails only the first N. Counted under a
    lock so N is total rather than N per thread.
    """
    global _seen
    with _lock:
        _seen += 1
        n = _seen
    if credential in FAIL_FOR_KEY:
        return FAIL_FOR_KEY[credential]
    if FAIL_FIRST_CREDENTIAL:
        # Whichever credential went first. Two accounts on a rung are equal as
        # far as this mock is concerned, and which one the scheduler reaches for
        # is not a property worth pinning — that the request survives the one it
        # picked failing is.
        with _lock:
            first = _credentials[0] if _credentials else None
        if first is not None and first == _digest(credential):
            return int(FAIL_FIRST_CREDENTIAL)
        return None
    if FAIL_FOR_KEY:
        return None
    if FAIL_STATUS:
        return int(FAIL_STATUS)
    if n <= FAIL_FIRST:
        return 529
    return None


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        sys.stderr.write("mock: " + fmt % args + "\n")

    def do_GET(self):
        # The breaker harness needs the POST count without adding to it.
        route, _, query = self.path.partition("?")
        route = route.rstrip("/")
        if route in ("/_seen", "/_credentials"):
            with _lock:
                if route == "/_seen":
                    payload = str(_seen).encode()
                else:
                    payload = "\n".join(_credentials).encode()
                    # Scoped to one request when asked. Without this the list is
                    # cumulative over the run, and "more credentials than
                    # before" is then true whenever a later stage uses a
                    # different account — which is not failover.
                    if "reset=1" in query:
                        _credentials.clear()
            self.send_response(200)
            self.send_header("content-type", "text/plain")
            self.send_header("content-length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            return
        self.send_error(404)

    def _refuse(self, status, kind, message):
        """An error in the shape this API actually returns them."""
        payload = json.dumps(
            {"type": "error", "error": {"type": kind, "message": message}}
        ).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("content-length", 0) or 0))
        try:
            request = json.loads(body or b"{}")
        except json.JSONDecodeError:
            request = {}

        # Onto stderr, which the verify scripts keep as `mock.log`. Without
        # it a script can only assert what came back, and a field the gateway
        # dropped on the way *out* — a system message it failed to render, a
        # thinking budget it never sent — is invisible in the reply.
        sys.stderr.write("mock-request: " + json.dumps(request, separators=(",", ":")) + "\n")
        sys.stderr.flush()

        status = _record_and_status(_credential(self.headers))
        if status:
            payload = json.dumps(
                {"type": "error", "error": {"type": "overloaded_error", "message": "mock failure"}}
            ).encode()
            self.send_response(status)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            return

        # The two constraints the real Messages API puts on thinking, because a
        # mock that accepts anything cannot see the class of bug this exists to
        # catch. `translate-verify.sh` sends `reasoning_effort: "high"`, and the
        # gateway's first attempt at bridging that rendered a budget of 16384
        # inside a 4096-token ceiling — a 400 in production and a pass here.
        #
        #   budget_tokens >= 1024, and strictly < max_tokens
        #
        # `type: "adaptive"` carries no budget and is not checked: its depth
        # rides on `output_config.effort`, which has no numeric constraint.
        thinking = request.get("thinking") or {}
        if thinking.get("type") == "enabled":
            budget = thinking.get("budget_tokens")
            ceiling = request.get("max_tokens", 4096)
            if not isinstance(budget, int) or budget < 1024 or budget >= ceiling:
                self._refuse(
                    400,
                    "invalid_request_error",
                    f"thinking.budget_tokens must be >= 1024 and < max_tokens "
                    f"(got {budget!r} against max_tokens {ceiling!r})",
                )
                return

        if request.get("stream"):
            self._stream(request)
        else:
            self._whole(request)

    # A stream slow enough that a rolling restart lands in the middle of it,
    # which is the entire point of the drain test.
    def _stream(self, request):
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        # An SSE body has no content-length, so under HTTP/1.1 the *only* signal
        # that it has ended is the connection closing. Without this the reader
        # sits waiting for more frames until its idle watchdog fires — a
        # six-second stream that takes three minutes to finish, and a drain test
        # that measures the timeout rather than the drain.
        self.send_header("connection", "close")
        self.close_connection = True
        self.end_headers()

        model = request.get("model", "claude-mock")
        self._event(
            "message_start",
            {
                "type": "message_start",
                "message": {
                    "id": "msg_mock",
                    "type": "message",
                    "role": "assistant",
                    "model": model,
                    "content": [],
                    # Split across message_start and message_delta exactly as
                    # Anthropic does, so the gateway's usage merge is exercised
                    # rather than bypassed.
                    "usage": {"input_tokens": 100, "output_tokens": 0},
                },
            },
        )
        self._event(
            "content_block_start",
            {"type": "content_block_start", "index": 0, "block": {"type": "text", "text": ""}},
        )

        gap = STREAM_SECONDS / max(CHUNKS, 1)
        for i in range(CHUNKS):
            time.sleep(gap)
            self._event(
                "content_block_delta",
                {
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": f"chunk {i} "},
                },
            )

        self._event("content_block_stop", {"type": "content_block_stop", "index": 0})
        self._event(
            "message_delta",
            {
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn"},
                "usage": {"output_tokens": CHUNKS * 3},
            },
        )
        self._event("message_stop", {"type": "message_stop"})
        # Close now rather than letting the handler unwind; the reader is
        # waiting on EOF.
        try:
            self.wfile.flush()
            self.connection.shutdown(1)
        except OSError:
            pass

    def _whole(self, request):
        payload = json.dumps(
            {
                "id": "msg_mock",
                "type": "message",
                "role": "assistant",
                "model": request.get("model", "claude-mock"),
                "content": [{"type": "text", "text": "mock response"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 100, "output_tokens": 12},
            }
        ).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def _event(self, name, data):
        frame = f"event: {name}\ndata: {json.dumps(data)}\n\n".encode()
        self.wfile.write(frame)
        self.wfile.flush()


def _selftest():
    """Check the mock's own credential handling, which no script can see.

    `_credential` reads two headers and strips one scheme, and everything the
    breaker and failover checks assert about *which* credential served a request
    goes through it. A mock that got this wrong would not fail — it would agree
    with itself, report one credential where two were used, and the scripts
    would pass while proving nothing. That is rr-C9: the fix was verified by
    reading the diff, because the mock is the thing the tests trust.

    Run with `MOCK_SELFTEST=1`, from CI, instead of serving.
    """
    global _credentials

    checks = [
        ({"x-api-key": "abc"}, "abc", "an x-api-key arrives as the credential"),
        ({"authorization": "Bearer abc"}, "abc", "and a bearer token without its scheme"),
        ({"authorization": "abc"}, "abc", "an unschemed authorization is the credential itself"),
        ({}, "", "no header, no credential"),
    ]
    failures = []
    for headers, want, why in checks:
        got = _credential(headers)
        if got != want:
            failures.append(f"{why}: _credential({headers!r}) == {got!r}, wanted {want!r}")

    # The two spellings are the same credential, so a script comparing counts
    # sees one and not two. This is the half MOCK_FAIL_FOR_KEY depends on.
    _credentials = []
    _credential({"x-api-key": "same-secret"})
    _credential({"authorization": "Bearer same-secret"})
    if len(_credentials) != 1:
        failures.append(
            "the same secret in either header must digest to one credential, "
            f"not {len(_credentials)}"
        )

    for line in failures:
        print(f"mock selftest: {line}", file=sys.stderr)
    if failures:
        sys.exit(1)
    print("mock selftest: ok", file=sys.stderr)


if __name__ == "__main__":
    if os.environ.get("MOCK_SELFTEST"):
        _selftest()
        sys.exit(0)
    port = int(os.environ.get("PORT", "8088"))
    print(
        f"mock upstream on :{port} "
        f"(stream={STREAM_SECONDS}s chunks={CHUNKS} fail_status={FAIL_STATUS} fail_first={FAIL_FIRST})",
        file=sys.stderr,
    )
    ThreadingHTTPServer(("0.0.0.0", port), Handler).serve_forever()
