#!/usr/bin/env python3
"""A stand-in for every upstream the custom-endpoints train can reach.

Usage: mock-endpoints.py <name> <port> <log.jsonl>

One process per upstream name. Each answers by path shape, so one script plays
OpenAI chat, Anthropic messages, Gemini generateContent, System One / Merge
Decisions, Merge's priced model list, Bedrock invoke / Converse /
ConverseStream, Vertex generateContent / rawPredict, and a Google token
endpoint. Every reply names the instance ("from <name>") so a test can tell
which upstream served a request. Every request is logged (path, auth header
names, model) — header values are never logged, only whether the Vertex
bearer equals the minted mock token.

A request whose credential contains "flaky" gets a 429, so failover across
two keys on one endpoint can be observed.
"""
import binascii
import json
import re
import struct
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

NAME, PORT, LOG = sys.argv[1], int(sys.argv[2]), sys.argv[3]
MINTED = "ya29.mock-minted-token"
AUTH_HEADERS = ("authorization", "x-api-key", "x-goog-api-key", "api-key")


def log(entry):
    with open(LOG, "a") as f:
        f.write(json.dumps(entry) + "\n")


def es_frame(event_type, payload):
    """One AWS event-stream message (application/vnd.amazon.eventstream)."""
    headers = b""
    for k, v in ((":event-type", event_type), (":content-type", "application/json"), (":message-type", "event")):
        kb, vb = k.encode(), v.encode()
        headers += bytes([len(kb)]) + kb + b"\x07" + struct.pack(">H", len(vb)) + vb
    body = json.dumps(payload).encode()
    total = 12 + len(headers) + len(body) + 4
    prelude = struct.pack(">II", total, len(headers))
    prelude += struct.pack(">I", binascii.crc32(prelude) & 0xFFFFFFFF)
    msg = prelude + headers + body
    return msg + struct.pack(">I", binascii.crc32(msg) & 0xFFFFFFFF)


def merge_listing():
    def m(model, vendors, out=("text",), status="available"):
        return {"model": model, "provider": model.split("/")[0], "display_name": model.split("/")[1].upper(),
                "availability_status": status, "access_required": False, "aliases": [],
                "vendors": {v: {"context_window": 128000, "max_output_tokens": 8192, "availability_status": "available",
                                "capabilities": {"input": ["text"], "output": list(out), "supports_tool_calling": True,
                                                 "supports_reasoning": False},
                                "pricing": {"input_per_million": i, "output_per_million": o,
                                            "cache_read_per_million": None, "cache_write_per_million": None}}
                            for v, (i, o) in vendors.items()}}
    return {"object": "list", "has_more": False, "next_cursor": None, "data": [
        m("zai/glm-5.3-flash", {"pricey": (0.15, 0.5), "particle": (0.015, 0.05)}),
        m("deepseek/deepseek-v4-flash", {"deepseek": (0.035, 0.07)}),
        m("openai/tts-1", {"openai": (15.0, 0.0)}, out=("audio",)),
        m("old/retired-model", {"x": (1.0, 2.0)}, status="deprecated"),
    ]}


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def send_json(self, status, obj, ctype="application/json"):
        b = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("content-type", ctype)
        self.send_header("content-length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def send_bytes(self, status, b, ctype):
        self.send_response(status)
        self.send_header("content-type", ctype)
        self.send_header("content-length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def record(self, body):
        cred = " ".join(self.headers.get(h, "") for h in AUTH_HEADERS)
        model = None
        try:
            model = json.loads(body or b"{}").get("model")
        except Exception:
            pass
        entry = {"ts": time.time(), "upstream": NAME, "method": self.command, "path": self.path,
                 "auth": [h for h in AUTH_HEADERS if self.headers.get(h)],
                 "bearer_is_minted": self.headers.get("authorization", "") == "Bearer " + MINTED,
                 "sigv4": self.headers.get("authorization", "").startswith("AWS4-HMAC-SHA256"),
                 "sigv4_scope": list(m.groups()) if (m := re.search(r"Credential=[^/]+/\d+/([^/]+)/([^/]+)/", self.headers.get("authorization", ""))) else None,
                 "flaky": "flaky" in cred, "model": model,
                 "anthropic_version": None}
        try:
            entry["anthropic_version"] = json.loads(body or b"{}").get("anthropic_version")
        except Exception:
            pass
        log(entry)
        return entry

    def do_GET(self):
        e = self.record(b"")
        p = self.path.split("?")[0]
        if NAME.startswith("merge") and p.endswith("/v1/models"):
            return self.send_json(200, merge_listing())
        if p.endswith("/models"):
            return self.send_json(200, {"object": "list", "data": [{"id": f"{NAME}-small", "object": "model"},
                                                                   {"id": f"{NAME}-large", "object": "model"}]})
        self.send_json(404, {"error": {"message": f"{NAME}: no GET {p}"}})

    def do_POST(self):
        n = int(self.headers.get("content-length") or 0)
        body = self.rfile.read(n)
        e = self.record(body)
        p = self.path.split("?")[0]
        req = json.loads(body or b"{}") if body[:1] in (b"{", b"[") else {}
        text = f"from {NAME}"
        if p.endswith("/token"):
            return self.send_json(200, {"access_token": MINTED, "expires_in": 3600, "token_type": "Bearer"})
        if e["flaky"]:
            return self.send_json(429, {"error": {"message": "rate limited (flaky key)", "type": "rate_limit"}})
        if p.endswith("/chat/completions"):
            if req.get("stream"):
                chunks = [{"choices": [{"index": 0, "delta": {"role": "assistant", "content": text}}]},
                          {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                           "usage": {"prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10}}]
                b = "".join(f"data: {json.dumps(dict(c, id='c1', object='chat.completion.chunk', model=req.get('model')))}\n\n" for c in chunks) + "data: [DONE]\n\n"
                return self.send_bytes(200, b.encode(), "text/event-stream")
            return self.send_json(200, {"id": "c1", "object": "chat.completion", "created": 1, "model": req.get("model"),
                                        "choices": [{"index": 0, "message": {"role": "assistant", "content": text}, "finish_reason": "stop"}],
                                        "usage": {"prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10}})
        anth = {"id": "msg_1", "type": "message", "role": "assistant", "model": req.get("model") or "m",
                "content": [{"type": "text", "text": text}], "stop_reason": "end_turn", "stop_sequence": None,
                "usage": {"input_tokens": 7, "output_tokens": 3}}
        if p.endswith("/v1/messages") or p.endswith(":rawPredict") or p.endswith("/invoke"):
            return self.send_json(200, anth)
        if p.endswith(":streamRawPredict") or p.endswith("/invoke-with-response-stream"):
            ev = [("message_start", {"type": "message_start", "message": dict(anth, content=[], usage={"input_tokens": 7, "output_tokens": 0})}),
                  ("content_block_start", {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
                  ("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
                  ("content_block_stop", {"type": "content_block_stop", "index": 0}),
                  ("message_delta", {"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 3}}),
                  ("message_stop", {"type": "message_stop"})]
            b = "".join(f"event: {k}\ndata: {json.dumps(v)}\n\n" for k, v in ev)
            return self.send_bytes(200, b.encode(), "text/event-stream")
        gem = {"candidates": [{"content": {"role": "model", "parts": [{"text": text}]}, "finishReason": "STOP", "index": 0}],
               "usageMetadata": {"promptTokenCount": 7, "candidatesTokenCount": 3, "totalTokenCount": 10}}
        if p.endswith(":generateContent"):
            return self.send_json(200, gem)
        if p.endswith(":streamGenerateContent"):
            return self.send_bytes(200, f"data: {json.dumps(gem)}\n\n".encode(), "text/event-stream")
        if p.endswith("/converse"):
            return self.send_json(200, {"output": {"message": {"role": "assistant", "content": [{"text": text}]}},
                                        "stopReason": "end_turn", "usage": {"inputTokens": 7, "outputTokens": 3, "totalTokens": 10},
                                        "metrics": {"latencyMs": 1}})
        if p.endswith("/converse-stream"):
            frames = [es_frame("messageStart", {"role": "assistant"}),
                      es_frame("contentBlockDelta", {"contentBlockIndex": 0, "delta": {"text": text}}),
                      es_frame("contentBlockStop", {"contentBlockIndex": 0}),
                      es_frame("messageStop", {"stopReason": "end_turn"}),
                      es_frame("metadata", {"usage": {"inputTokens": 7, "outputTokens": 3, "totalTokens": 10}, "metrics": {"latencyMs": 1}})]
            return self.send_bytes(200, b"".join(frames), "application/vnd.amazon.eventstream")
        if p.endswith("/v1/systemone") or p.endswith("/v1/decisions"):
            answers = {}
            for k, q in (req.get("questions") or {}).items():
                if q.get("type") == "choice":
                    labels = list((q.get("criteria") or {}).keys()) or ["none"]
                    answers[k] = {"type": "choice", "choice": labels[0], "confidence": 0.8,
                                  "probabilities": {l: (0.8 if i == 0 else 0.2) for i, l in enumerate(labels)}}
                else:
                    answers[k] = {"type": "noul", "noul": 0.97}
            out = {"model": req.get("model") or "jev-latest", "answers": answers,
                   "usage": {"input_tokens": 42, "output_tokens": 0}}
            if p.endswith("/v1/decisions"):
                out.update({"object": "decision", "vendor": "typesafe"})
                out["usage"].update({"total_tokens": 42, "cost": 0.0000017})
            return self.send_json(200, out)
        self.send_json(404, {"error": {"message": f"{NAME}: no POST {p}"}})


ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
