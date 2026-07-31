"""The tool belt. Everything here runs CLIENT-SIDE, on the machine driving
the agent - that is the passthrough contract: the enclave relays the model's
call and never executes a caller's tool.

That placement is a feature, not a compromise. The fleet's outbound egress is
IPv6-only, so most of the web is unreachable from inside llm-chat; read_url
running here reaches the IPv4 internet the enclave cannot. The trade travels
the other way too: what these tools fetch and compute is visible to THIS
machine, so nothing here should handle material that must stay in the TEE.

Stdlib only, deliberately: the follow-up home for this agent is CPython
inside a RISC Box guest, where every native wheel is a port and urllib is
free.
"""

from __future__ import annotations

import ast
import datetime
import html.parser
import operator
import urllib.request

from langchain_core.tools import tool

_MAX_PAGE_CHARS = 6000

_OPS = {
    ast.Add: operator.add,
    ast.Sub: operator.sub,
    ast.Mult: operator.mul,
    ast.Div: operator.truediv,
    ast.FloorDiv: operator.floordiv,
    ast.Mod: operator.mod,
    ast.Pow: operator.pow,
    ast.USub: operator.neg,
    ast.UAdd: operator.pos,
}


def _eval_node(node: ast.AST) -> float:
    if isinstance(node, ast.Expression):
        return _eval_node(node.body)
    if isinstance(node, ast.Constant) and isinstance(node.value, (int, float)):
        return node.value
    if isinstance(node, ast.BinOp) and type(node.op) in _OPS:
        return _OPS[type(node.op)](_eval_node(node.left), _eval_node(node.right))
    if isinstance(node, ast.UnaryOp) and type(node.op) in _OPS:
        return _OPS[type(node.op)](_eval_node(node.operand))
    raise ValueError(f"unsupported expression element: {ast.dump(node)[:80]}")


@tool
def calculator(expression: str) -> str:
    """Evaluate an arithmetic expression exactly. Supports + - * / // % **,
    parentheses, and numbers; nothing else (no names, no calls)."""
    try:
        tree = ast.parse(expression.strip(), mode="eval")
        result = _eval_node(tree)
    except ZeroDivisionError:
        return "error: division by zero"
    except (ValueError, SyntaxError) as e:
        return f"error: {e}"
    if isinstance(result, float) and result.is_integer():
        result = int(result)
    return str(result)


class _TextExtractor(html.parser.HTMLParser):
    """Visible text from an HTML page: scripts, styles and tags dropped,
    block boundaries kept as newlines. Crude and dependency-free on purpose."""

    _SKIP = {"script", "style", "noscript", "template", "svg", "head"}
    _BLOCK = {"p", "br", "div", "li", "tr", "h1", "h2", "h3", "h4", "h5", "h6",
              "section", "article", "blockquote", "pre"}

    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self.parts: list[str] = []
        self._skip_depth = 0

    def handle_starttag(self, tag: str, attrs: object) -> None:
        if tag in self._SKIP:
            self._skip_depth += 1
        elif tag in self._BLOCK:
            self.parts.append("\n")

    def handle_endtag(self, tag: str) -> None:
        if tag in self._SKIP and self._skip_depth:
            self._skip_depth -= 1
        elif tag in self._BLOCK:
            self.parts.append("\n")

    def handle_data(self, data: str) -> None:
        if not self._skip_depth and data.strip():
            self.parts.append(data)


def extract_text(html_src: str, limit: int = _MAX_PAGE_CHARS) -> str:
    p = _TextExtractor()
    p.feed(html_src)
    text = "".join(p.parts)
    lines = [ln.strip() for ln in text.splitlines()]
    text = "\n".join(ln for ln in lines if ln)
    if len(text) > limit:
        text = text[:limit] + "\n[truncated]"
    return text


@tool
def read_url(url: str) -> str:
    """Fetch a web page and return its visible text (truncated). Runs on the
    agent host, which reaches the IPv4 internet the enclave cannot."""
    if not url.startswith(("http://", "https://")):
        return "error: only http(s) URLs"
    req = urllib.request.Request(url, headers={
        "User-Agent": "Mozilla/5.0 (compatible; enclave-agent/0.1)",
        "Accept": "text/html,application/xhtml+xml,text/plain;q=0.9,*/*;q=0.5",
    })
    try:
        with urllib.request.urlopen(req, timeout=20) as resp:
            ctype = resp.headers.get("content-type", "")
            body = resp.read(1_500_000)
    except OSError as e:
        return f"error: fetch failed: {e}"
    charset = "utf-8"
    if "charset=" in ctype:
        charset = ctype.split("charset=")[-1].split(";")[0].strip() or "utf-8"
    try:
        text = body.decode(charset, errors="replace")
    except LookupError:
        text = body.decode("utf-8", errors="replace")
    if "html" in ctype or text.lstrip()[:1] == "<":
        return extract_text(text) or "error: page had no extractable text"
    return text[:_MAX_PAGE_CHARS]


@tool
def utc_now() -> str:
    """The current date and time, UTC. The model's sense of 'today' is frozen
    at training; this is the ground truth."""
    return datetime.datetime.now(datetime.timezone.utc).strftime(
        "%Y-%m-%d %H:%M:%S UTC (%A)")


DEFAULT_TOOLS = [calculator, read_url, utc_now]
