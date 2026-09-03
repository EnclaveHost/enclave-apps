"""The tool belt. Everything here runs CLIENT-SIDE, on the machine driving
the agent - that is the passthrough contract: the enclave relays the model's
call and never executes a caller's tool.

That placement is a feature, not a compromise. The fleet's outbound egress is
IPv6-only, so most of the web is unreachable from inside eyesoff-ai; read_url
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
import functools
import html.parser
import json
import operator
import os
import urllib.error
import urllib.parse
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


# ---- the notebook (jot) ------------------------------------------------------
# An Enclave `jot` deployment: the agent's notes as plain objects in the
# deployer's own S3 bucket, behind a bearer key. These six tools are the
# client side of its API (GET /api/tools on the deployment describes the same
# verbs). They are only offered when ENCLAVE_AGENT_NOTES_URL is set, so an
# agent without a notebook is not shown tools that would fail.
#
# What crosses the boundary: the note names and contents travel between this
# machine and the jot enclave over TLS, and land as objects in the bucket the
# deployer configured. That is the point (memory that outlives the process),
# and it is also why the bucket is the deployer's own.

_NOTES_TIMEOUT = 30
_NOTES_MAX_CHARS = 20000


class NotesClient:
    """Thin HTTP client for a jot deployment. Stdlib only, like the rest."""

    def __init__(self, base_url: str, api_key: str = "", user: str = "") -> None:
        self.base_url = base_url.rstrip("/")
        self.api_key = api_key
        # a per-user jot deployment needs a name beside the key: the Enclave
        # account (0x wallet address or acct_ id) whose notebook this agent
        # writes. Trusted by jot only together with the api_key.
        self.user = user

    @staticmethod
    def _path(name: str) -> str:
        return "/".join(urllib.parse.quote(seg, safe="") for seg in name.split("/"))

    def call(self, method: str, path: str, body: dict | None = None,
             query: dict | None = None) -> dict:
        url = self.base_url + path
        if query:
            url += "?" + urllib.parse.urlencode({k: v for k, v in query.items() if v})
        data = None
        headers = {"accept": "application/json"}
        if self.api_key:
            # X-Api-Key, not a bearer: the platform's app gateway consumes
            # Authorization (its own session carriage) and never forwards it
            headers["x-api-key"] = self.api_key
        if self.user:
            headers["x-user"] = self.user
        if body is not None:
            data = json.dumps(body).encode()
            headers["content-type"] = "application/json"
        req = urllib.request.Request(url, data=data, method=method, headers=headers)
        try:
            with urllib.request.urlopen(req, timeout=_NOTES_TIMEOUT) as resp:
                return json.loads(resp.read().decode())
        except urllib.error.HTTPError as e:
            try:
                msg = json.loads(e.read().decode())["error"]["message"]
            except Exception:  # noqa: BLE001 - any shape of error body
                msg = f"HTTP {e.code}"
            raise RuntimeError(msg) from None
        except OSError as e:
            raise RuntimeError(f"notebook unreachable: {e}") from None


def make_notes_tools(client: NotesClient) -> list:
    """The six notebook tools, bound to one client."""

    def guard(fn):
        # functools.wraps keeps the signature @tool reads for the schema
        @functools.wraps(fn)
        def run(*a, **kw):
            try:
                return fn(*a, **kw)
            except RuntimeError as e:
                return f"error: {e}"
        return run

    @tool
    @guard
    def notes_list(prefix: str = "") -> str:
        """List the notes in the notebook (name, size, last modified). Call this
        first when unsure what has already been written down. `prefix` narrows
        to names starting with it, e.g. 'projects/'."""
        r = client.call("GET", "/api/notes", query={"prefix": prefix, "limit": "500"})
        if not r["notes"]:
            return "(no notes)" if not prefix else f"(no notes under {prefix})"
        lines = [f"{n['name']}  ({n['size']} B, {n.get('modified', '')})" for n in r["notes"]]
        if r.get("truncated"):
            lines.append("[more notes not listed]")
        return "\n".join(lines)

    @tool
    @guard
    def notes_read(name: str) -> str:
        """Read one note's full text by name (e.g. 'projects/enclave.md')."""
        r = client.call("GET", "/api/notes/" + client._path(name))
        text = r["content"]
        if len(text) > _NOTES_MAX_CHARS:
            text = text[:_NOTES_MAX_CHARS] + "\n[truncated]"
        return text if text else "(empty note)"

    @tool
    @guard
    def notes_write(name: str, content: str) -> str:
        """Create or replace a note with the given full text. Prefer notes_append
        to add to an existing note without rewriting it. Names are relative
        paths of letters, digits, - _ . and spaces; markdown is a good default."""
        r = client.call("PUT", "/api/notes/" + client._path(name), body={"content": content})
        return f"saved {r['name']} ({r['size']} B)"

    @tool
    @guard
    def notes_append(name: str, content: str) -> str:
        """Append a paragraph to a note, creating it if it does not exist. The
        right verb for logging something learned or a decision made."""
        r = client.call("POST", "/api/notes/" + client._path(name) + "/append",
                        body={"content": content})
        return f"appended to {r['name']} (now {r['size']} B)"

    @tool
    @guard
    def notes_search(query: str, prefix: str = "") -> str:
        """Case-insensitive substring search across all note bodies. Returns
        'name:line: text' for each matching line."""
        r = client.call("GET", "/api/search", query={"q": query, "prefix": prefix, "limit": "50"})
        if not r["hits"]:
            return f"no matches for {query!r} in {r['scanned']} notes"
        lines = [f"{h['name']}:{h['line']}: {h['text']}" for h in r["hits"]]
        if r.get("truncated"):
            lines.append("[more matches not listed]")
        return "\n".join(lines)

    @tool
    @guard
    def notes_delete(name: str) -> str:
        """Delete a note by name."""
        r = client.call("DELETE", "/api/notes/" + client._path(name))
        return f"deleted {r['name']}"

    return [notes_list, notes_read, notes_write, notes_append, notes_search, notes_delete]


def notes_tools_from_env() -> list:
    """The notebook tools when ENCLAVE_AGENT_NOTES_URL points at a jot
    deployment (ENCLAVE_AGENT_NOTES_KEY is its api_key, ENCLAVE_AGENT_NOTES_USER
    the account whose notebook it is, on a per-user deployment); otherwise none."""
    url = os.environ.get("ENCLAVE_AGENT_NOTES_URL", "").strip()
    if not url:
        return []
    return make_notes_tools(NotesClient(url, os.environ.get("ENCLAVE_AGENT_NOTES_KEY", ""),
                                        os.environ.get("ENCLAVE_AGENT_NOTES_USER", "")))


DEFAULT_TOOLS = [calculator, read_url, utc_now, *notes_tools_from_env()]
