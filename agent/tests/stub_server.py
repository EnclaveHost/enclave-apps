"""A pocket llm-chat: just enough of the /v1 passthrough contract to prove
the agent loop without a GPU, a wallet, or a network.

Faithful to the real thing where the contract matters:
  - exactly ONE tool call per model turn (llm-chat stops generation at the
    first completed call)
  - `arguments` is a JSON-encoded STRING
  - finish_reason "tool_calls", content null on a call turn
  - a role:"tool" message in the request flips the reply to a final answer
    that quotes the result, which is how the tests see the round trip landed
"""

from __future__ import annotations

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class _Handler(BaseHTTPRequestHandler):
    def log_message(self, *args: object) -> None:  # keep test output clean
        pass

    def do_POST(self) -> None:
        if self.path.rstrip("/") != "/v1/chat/completions":
            self._json(404, {"error": {"message": "not found"}})
            return
        body = json.loads(self.rfile.read(int(self.headers["content-length"])))
        tools = body.get("tools") or []
        # only THIS turn's tool results count: a multi-turn conversation
        # carries older ones in history, and those must not short-circuit
        # the new turn's call
        messages = body["messages"]
        last_user = max(i for i, m in enumerate(messages)
                        if m.get("role") == "user")
        tool_results = [m for m in messages[last_user:] if m.get("role") == "tool"]

        if tool_results:
            msg = {"role": "assistant",
                   "content": f"The tool said: {tool_results[-1]['content']}"}
            finish = "stop"
        elif tools:
            # always ask for the first declared tool, echoing the user's text
            # into its first parameter - enough for the loop to be observable
            fn = tools[0]["function"]
            param = next(iter(fn.get("parameters", {}).get("properties", {"x": 0})))
            user = next(m["content"] for m in reversed(body["messages"])
                        if m.get("role") == "user")
            msg = {"role": "assistant", "content": None,
                   "tool_calls": [{"id": "call_stub0", "type": "function",
                                   "function": {"name": fn["name"],
                                                "arguments": json.dumps({param: user})}}]}
            finish = "tool_calls"
        else:
            msg = {"role": "assistant", "content": "No tools were offered."}
            finish = "stop"

        self._json(200, {
            "id": "chatcmpl-stub", "object": "chat.completion", "created": 0,
            "model": body.get("model", "stub"),
            "choices": [{"index": 0, "message": msg, "finish_reason": finish}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
        })

    def _json(self, status: int, payload: dict) -> None:
        data = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


def start() -> tuple[ThreadingHTTPServer, str]:
    """Serve on an ephemeral port; returns (server, base_url ending in /v1)."""
    srv = ThreadingHTTPServer(("127.0.0.1", 0), _Handler)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv, f"http://127.0.0.1:{srv.server_address[1]}/v1"
