"""A terminal front end for the loop: a REPL by default, one-shot with -p.

Tool activity is narrated as it happens - the whole point of watching an
agent is seeing what it chose to do - and the reply's <think> block is folded
away unless -v asks for it.
"""

from __future__ import annotations

import argparse
import re
import sys

from langchain_core.messages import AIMessage, ToolMessage

from .agent import build_agent, run_once
from .config import Settings
from .model import make_model


def _strip_think(text: str) -> str:
    return re.sub(r"<think>.*?</think>\s*", "", text, flags=re.S).strip()


def _narrate(messages: list, start: int, verbose: bool) -> None:
    """Print everything after `start`: tool calls, results, and the answer."""
    for m in messages[start:]:
        if isinstance(m, AIMessage) and m.tool_calls:
            for c in m.tool_calls:
                print(f"  ⚒ {c['name']}({c['args']})", file=sys.stderr)
        elif isinstance(m, ToolMessage):
            preview = str(m.content).replace("\n", " ")[:120]
            print(f"    → {preview}", file=sys.stderr)
        elif isinstance(m, AIMessage):
            text = m.content if isinstance(m.content, str) else str(m.content)
            print(text if verbose else _strip_think(text))


def main() -> int:
    ap = argparse.ArgumentParser(
        prog="enclave-agent",
        description="LangGraph agent backed by an Enclave llm-chat deployment")
    ap.add_argument("-p", "--prompt", help="run one prompt and exit")
    ap.add_argument("-v", "--verbose", action="store_true",
                    help="keep <think> blocks in the printed answer")
    args = ap.parse_args()

    settings = Settings()
    agent = build_agent(make_model(settings), system_prompt=settings.system_prompt)
    print(f"[enclave-agent] {settings.base_url}", file=sys.stderr)

    if args.prompt:
        messages = run_once(agent, args.prompt, settings)
        _narrate(messages, 1, args.verbose)
        return 0

    history: list = []
    while True:
        try:
            line = input("you> ").strip()
        except (EOFError, KeyboardInterrupt):
            print()
            return 0
        if not line or line in (":q", "exit", "quit"):
            if line:
                return 0
            continue
        before = len(history) + 1  # +1 for the user turn being added
        try:
            history = run_once(agent, line, settings, history)
        except Exception as e:  # a dead deployment should not kill the REPL
            print(f"[error] {e}", file=sys.stderr)
            continue
        _narrate(history, before, args.verbose)


if __name__ == "__main__":
    sys.exit(main())
