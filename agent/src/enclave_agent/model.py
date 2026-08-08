"""The chat model: an OpenAI client aimed at the enclave.

Nothing here knows it is talking to a TEE; that is the point. eyesoff-ai's /v1
is OpenAI-shaped end to end (streaming chunks, tool_calls, role:"tool"), so
the whole LangChain/LangGraph toolchain works unmodified and this file stays
one function long.
"""

from __future__ import annotations

from langchain_openai import ChatOpenAI

from .config import Settings


def make_model(settings: Settings) -> ChatOpenAI:
    return ChatOpenAI(
        base_url=settings.base_url,
        api_key=settings.api_key,
        model=settings.model,
        temperature=settings.temperature,
        max_tokens=settings.max_tokens,
        streaming=settings.streaming,
    )
