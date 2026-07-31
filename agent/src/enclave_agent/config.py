"""Where the agent points and how it talks.

Everything is env-driven with working defaults, because the common case is
"run it against the fleet deployment" and the uncommon cases (a stub in tests,
a local wasmtime serve, a future RISC Box guest) only need a different URL.
"""

from __future__ import annotations

import os
from dataclasses import dataclass, field


def _env(name: str, default: str) -> str:
    return os.environ.get(name, default)


@dataclass
class Settings:
    # The llm-chat deployment's OpenAI surface. Client-declared tools need
    # catalog 1.4.0+ (crate 0.26.0); older versions refuse the tools array
    # with a 400 that says so.
    base_url: str = field(default_factory=lambda: _env(
        "ENCLAVE_AGENT_BASE_URL", "https://cc1f4f3f.app.enclave.host/v1"))
    # llm-chat only checks this when the deployment config sets an api_key;
    # the OpenAI client insists on some value either way.
    api_key: str = field(default_factory=lambda: _env(
        "ENCLAVE_AGENT_API_KEY", "unused"))
    # Any name works: llm-chat resolves unknown models to the largest one the
    # deployment serves (GET /v1/models lists what is attached).
    model: str = field(default_factory=lambda: _env(
        "ENCLAVE_AGENT_MODEL", "auto"))
    temperature: float = field(default_factory=lambda: float(_env(
        "ENCLAVE_AGENT_TEMPERATURE", "0.6")))
    max_tokens: int = field(default_factory=lambda: int(_env(
        "ENCLAVE_AGENT_MAX_TOKENS", "4096")))
    # Streaming keeps long turns alive: the gateway cuts a response stream
    # that goes quiet for ~180s, and llm-chat heartbeats SSE comments while
    # it thinks. The buffered path has no heartbeat to send.
    streaming: bool = field(default_factory=lambda: _env(
        "ENCLAVE_AGENT_STREAMING", "1") not in ("0", "false", "no"))
    # LangGraph recursion cap. llm-chat's passthrough yields ONE tool call
    # per model turn, so a task that needs N calls costs 2N+1 graph steps.
    recursion_limit: int = field(default_factory=lambda: int(_env(
        "ENCLAVE_AGENT_RECURSION_LIMIT", "25")))
    system_prompt: str = field(default_factory=lambda: _env(
        "ENCLAVE_AGENT_SYSTEM_PROMPT",
        "You are a capable assistant with tools. Use a tool when it settles "
        "the question better than memory; answer directly when it does not. "
        "After a tool result arrives, either call the next tool you need or "
        "write the final answer. Be concise and concrete."))
