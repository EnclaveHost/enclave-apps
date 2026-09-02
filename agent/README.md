# enclave-agent

A LangGraph agent whose model is an Enclave `eyesoff-ai` deployment. The graph,
the tools, and the conversation state live on your machine; the model, its
weights, and every token of inference live inside the attested enclave. The
two meet at eyesoff-ai's OpenAI-compatible `/v1/chat/completions`, so the whole
LangChain toolchain works unmodified: `ChatOpenAI(base_url=...)` and nothing
else knows a TEE is involved.

Requires eyesoff-ai catalog 1.4.0+ (crate 0.26.0): that release added the
client-tools passthrough, where an OpenAI `tools: [...]` array is rendered
into the model's prompt and its call comes back as `tool_calls` for THIS side
to execute. Older versions refuse the array with a 400 that says so.

## The trust shape, stated plainly

- What the enclave sees: the conversation, the tool signatures, and the tool
  results you send back. That is inference input; it never leaves the TEE.
- What this machine sees: everything the tools do. `read_url` fetches from
  here (which is a feature: the fleet's egress is IPv6-only, and the agent
  host reaches the IPv4 web the enclave cannot). Do not put a tool here whose
  inputs must stay inside the TEE; that is what eyesoff-ai's own server-side
  tool registry is for.

## Quick start

    cd agent
    python3 -m venv .venv && .venv/bin/pip install -e .
    .venv/bin/enclave-agent -p "What is 1337 * 42 - 7? Use the calculator."

No arguments starts a REPL; `-v` keeps the model's `<think>` blocks in the
output. Tool activity is narrated to stderr as it happens.

## Configuration (env)

| Variable | Default | Meaning |
| --- | --- | --- |
| `ENCLAVE_AGENT_BASE_URL` | `https://cc1f4f3f.app.enclave.host/v1` | the deployment's OpenAI surface |
| `ENCLAVE_AGENT_API_KEY` | `unused` | only checked if the deployment config sets `api_key` |
| `ENCLAVE_AGENT_MODEL` | `auto` | unknown names resolve to the largest attached model |
| `ENCLAVE_AGENT_MAX_TOKENS` | `4096` | per-generation cap |
| `ENCLAVE_AGENT_TEMPERATURE` | `0.6` | sampling |
| `ENCLAVE_AGENT_STREAMING` | `1` | keep on: the gateway cuts streams silent for ~180s and eyesoff-ai heartbeats only while streaming |
| `ENCLAVE_AGENT_RECURSION_LIMIT` | `25` | LangGraph step cap; the passthrough yields ONE tool call per model turn, so N calls cost 2N+1 steps |
| `ENCLAVE_AGENT_SYSTEM_PROMPT` | (see config.py) | the agent's standing instructions |
| `ENCLAVE_AGENT_NOTES_URL` | (unset) | a [jot](../jot) deployment (`https://<id8>.app.enclave.host`); when set, the six notebook tools (`notes_list/read/write/append/search/delete`) join the belt |
| `ENCLAVE_AGENT_NOTES_KEY` | (empty) | that deployment's `api_key`, sent as a bearer |

## Layout

    src/enclave_agent/
      agent.py    the graph: model -> (tool_calls? tools : END) -> model
      tools.py    the client-side tool belt (calculator, read_url, utc_now,
                  and the jot notebook tools when ENCLAVE_AGENT_NOTES_URL is set)
      model.py    ChatOpenAI factory aimed at the deployment
      config.py   env-driven settings
      cli.py      REPL / one-shot front end
    tests/        stub OpenAI server faithful to the passthrough contract, a
                  stub jot notebook, plus tool unit tests:
                  python -m unittest discover -s tests

The tool belt is stdlib-only on purpose. The intended follow-up is running
this same package from CPython inside a RISC Box guest, so the entire agent
loop sits inside a TEE as well; every dependency beyond langgraph itself is
a port that future has to pay for.

## Adding a tool

Write a function, decorate it with `@tool` from `langchain_core.tools`, add
it to `DEFAULT_TOOLS` in tools.py. The enclave renders its name, description
and JSON schema into the model's prompt verbatim; the model calls it by name;
LangGraph's ToolNode executes it here and returns the result. Remember what
crossing the boundary means before adding anything secret-bearing.
