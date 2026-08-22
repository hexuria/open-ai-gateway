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
  MOCK_FAIL_STATUS     if set, every request returns this status instead
  MOCK_FAIL_FIRST      fail only the first N requests, then serve normally
"""

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

_lock = threading.Lock()
_seen = 0


def _should_fail():
    """Whether this request fails, and why. Counted under a lock so
    MOCK_FAIL_FIRST means N requests total rather than N per thread."""
    global _seen
    if FAIL_STATUS:
        return int(FAIL_STATUS)
    with _lock:
        _seen += 1
        if _seen <= FAIL_FIRST:
            return 529
    return None


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        sys.stderr.write("mock: " + fmt % args + "\n")

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("content-length", 0) or 0))
        try:
            request = json.loads(body or b"{}")
        except json.JSONDecodeError:
            request = {}

        status = _should_fail()
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


if __name__ == "__main__":
    port = int(os.environ.get("PORT", "8088"))
    print(
        f"mock upstream on :{port} "
        f"(stream={STREAM_SECONDS}s chunks={CHUNKS} fail_status={FAIL_STATUS} fail_first={FAIL_FIRST})",
        file=sys.stderr,
    )
    ThreadingHTTPServer(("0.0.0.0", port), Handler).serve_forever()
