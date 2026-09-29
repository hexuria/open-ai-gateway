#!/usr/bin/env python3
"""A stand-in for Jev's System One API, for verification only.

`systemone-verify.sh` points the gateway's `provider_base_urls.jev` here. It
answers the two calls the gateway makes with a Jev key:

  POST /v1/systemone   every question answered by its type: a noul with 0.97, a
                       choice with its first label at 0.8, a score with its last
                       index at 0.7; usage 42 in, 3 out; `x-typesafe-request-id`
  GET  /v1/models      one model, `jev-latest`

Answers are indented JSON with a trailing newline: valid, and not what any
serialiser writes. A gateway that parsed the answer and wrote it out again
would send different bytes, so comparing the bytes it relays with `/_last` is
a check that can fail.

Standard library only, like `mock-upstream.py`, and for the same reason.

Behaviour is set by environment:
  MOCK_FAIL_FOR_KEY    `credential=status` pairs, comma separated: only these
                       Jev keys fail, each with its own status. A `Bearer `
                       prefix is stripped before matching.

GET /_seen            System One POSTs so far, as plain text, without counting.
GET /_last            the exact bytes of the last answer sent.
GET /_credentials     one short hash per distinct key seen, never the key;
                      `?reset=1` clears the list after reading it.

`MOCK_SELFTEST=1` checks the mock's own parsing instead of serving.
"""

import hashlib
import json
import os
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

FAIL_FOR_KEY = {}
for _pair in filter(None, os.environ.get("MOCK_FAIL_FOR_KEY", "").split(",")):
    _cred, _, _status = _pair.partition("=")
    FAIL_FOR_KEY[_cred] = int(_status or 500)

_lock = threading.Lock()
_seen = 0
_credentials = []
_last = b""


def _credential(headers):
    """The Jev key a request arrived with, recording a hash of it."""
    raw = headers.get("authorization") or ""
    scheme, _, rest = raw.partition(" ")
    if scheme.lower() == "bearer" and rest:
        raw = rest
    if raw:
        digest = hashlib.sha256(raw.encode()).hexdigest()[:8]
        with _lock:
            if digest not in _credentials:
                _credentials.append(digest)
    return raw


def _answer(question):
    """One answer, of the type the question asks for."""
    kind = question.get("type")
    if kind == "choice":
        labels = list((question.get("criteria") or {}).keys()) or ["none"]
        rest = round(0.2 / max(len(labels) - 1, 1), 6)
        return {
            "type": "choice",
            "choice": labels[0],
            "confidence": 0.8,
            "probabilities": {l: (0.8 if i == 0 else rest) for i, l in enumerate(labels)},
        }
    if kind == "score":
        criteria = question.get("criteria") or ["only"]
        last = len(criteria) - 1
        return {
            "type": "score",
            "score": float(last),
            "confidence": 0.7,
            "legend": {str(i): c for i, c in enumerate(criteria)},
            "probabilities": {str(i): (0.7 if i == last else 0.1) for i in range(last + 1)},
        }
    return {"type": "noul", "noul": 0.97}


def _respond(request):
    """The System One response to a parsed request, as the bytes to send."""
    body = {
        "model": request.get("model") or "jev-latest",
        "usage": {"input_tokens": 42, "output_tokens": 3},
        "answers": {name: _answer(q) for name, q in (request.get("questions") or {}).items()},
    }
    return (json.dumps(body, indent=2) + "\n").encode()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        sys.stderr.write("mock-jev: " + fmt % args + "\n")

    def _send(self, status, payload, content_type="application/json", request_id=None):
        self.send_response(status)
        self.send_header("content-type", content_type)
        self.send_header("content-length", str(len(payload)))
        if request_id:
            self.send_header("x-typesafe-request-id", request_id)
        self.end_headers()
        self.wfile.write(payload)

    def do_GET(self):
        route, _, query = self.path.partition("?")
        route = route.rstrip("/")
        if route in ("/_seen", "/_last", "/_credentials"):
            with _lock:
                if route == "/_seen":
                    payload = str(_seen).encode()
                elif route == "/_last":
                    payload = _last
                else:
                    payload = "\n".join(_credentials).encode()
                    if "reset=1" in query:
                        _credentials.clear()
            self._send(200, payload, "application/octet-stream")
            return
        if route == "/v1/models":
            key = _credential(self.headers)
            status = FAIL_FOR_KEY.get(key)
            if status:
                self._send(status, b'{"detail":"mock Jev failure"}')
                return
            listing = {"models": [{"name": "jev-latest", "description": "Mock Jev", "release_date": "2026-08-01"}]}
            self._send(200, (json.dumps(listing, indent=2) + "\n").encode(), request_id="models-1")
            return
        self._send(404, b'{"detail":"not found"}')

    def do_POST(self):
        global _seen, _last
        body = self.rfile.read(int(self.headers.get("content-length", 0) or 0))
        if self.path.partition("?")[0].rstrip("/") != "/v1/systemone":
            self._send(404, b'{"detail":"not found"}')
            return
        key = _credential(self.headers)
        with _lock:
            _seen += 1
            n = _seen
        # The request as the gateway forwarded it, for a script that wants to
        # see what went upstream. Never the key.
        sys.stderr.write("mock-jev-request: " + body.decode(errors="replace") + "\n")
        sys.stderr.flush()
        status = FAIL_FOR_KEY.get(key)
        if status:
            self._send(status, json.dumps({"detail": f"mock Jev failure {status}"}).encode())
            return
        try:
            request = json.loads(body or b"{}")
        except json.JSONDecodeError:
            self._send(422, b'{"detail":"body is not JSON"}')
            return
        payload = _respond(request)
        with _lock:
            _last = payload
        self._send(200, payload, request_id=f"req-{n}")


def _selftest():
    """The mock's own parsing, which every assertion about keys and answers trusts."""
    global _credentials
    failures = []
    for headers, want in [
        ({"authorization": "Bearer abc"}, "abc"),
        ({"authorization": "abc"}, "abc"),
        ({}, ""),
    ]:
        got = _credential(headers)
        if got != want:
            failures.append(f"_credential({headers!r}) == {got!r}, wanted {want!r}")

    _credentials = []
    _credential({"authorization": "Bearer same"})
    _credential({"authorization": "Bearer same"})
    _credential({"authorization": "Bearer other"})
    if len(_credentials) != 2:
        failures.append(f"two keys must be two credentials, not {len(_credentials)}")

    sent = json.loads(_respond({
        "questions": {
            "b": {"type": "noul"},
            "t": {"type": "choice", "criteria": {"calm": None, "angry": None}},
            "u": {"type": "score", "criteria": ["low", "high"]},
        }
    }))
    if sent["model"] != "jev-latest" or sent["usage"] != {"input_tokens": 42, "output_tokens": 3}:
        failures.append(f"model or usage wrong: {sent}")
    answers = sent["answers"]
    if [answers[k]["type"] for k in ("b", "t", "u")] != ["noul", "choice", "score"]:
        failures.append(f"an answer is of the wrong type: {answers}")
    if answers["t"]["choice"] != "calm" or answers["u"]["score"] != 1.0:
        failures.append(f"an answer is not the one documented: {answers}")

    for line in failures:
        print(f"mock-jev selftest: {line}", file=sys.stderr)
    if failures:
        sys.exit(1)
    print("mock-jev selftest: ok", file=sys.stderr)


if __name__ == "__main__":
    if os.environ.get("MOCK_SELFTEST"):
        _selftest()
        sys.exit(0)
    port = int(os.environ.get("PORT", "8097"))
    print(f"mock Jev on :{port} (fail_for_key={len(FAIL_FOR_KEY)} keys)", file=sys.stderr)
    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
