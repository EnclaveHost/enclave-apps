//! TOOLS: HTTP endpoints and MCP servers, named in the DEPLOYMENT'S config,
//! that the model may call in the middle of an answer.
//!
//! Until now every outward capability was a pre-pass: route_web_search decided
//! about the web before generation started, author_vision_query wrote a question
//! for the image-reader, and each one arrived as its own config block, its own
//! router branch and its own SSE event. That works, but the decision has to be
//! made from four messages of context BEFORE the model has thought about the
//! question, and adding a fourth capability meant adding a fourth branch. A tool
//! registry replaces the branches with a list: the model is shown what exists
//! and asks for what it needs, when it needs it.
//!
//! THE SECURITY BOUNDARY, which is the reason this file is shaped the way it is:
//! THE CLIENT SELECTS TOOLS, IT NEVER DEFINES THEM. Every tool here comes from
//! the deployment's own config - published on-chain by CID, world-readable, and
//! fixed at launch. A request can turn the whole feature off (and by default it
//! IS off, like web search), but it cannot add an entry, change a URL or set a
//! header. The alternative - letting a request declare a tool the enclave then
//! executes - would be an open fetcher running with this deployment's egress
//! identity and its secrets, which is egress laundering with extra steps.
//! Client-declared tools live in the OTHER mode, which /v1 implements (the
//! PASSTHROUGH, see client_tools in lib.rs): the model's call is parsed and
//! handed back as OpenAI `tool_calls`, and the CLIENT executes it. Nothing
//! here ever runs one of those - a request that declares tools gets its own
//! list INSTEAD of this registry, never merged with it.
//!
//! WHERE THE WORK HAPPENS: in the enclave, never in the browser. Same reasoning
//! as search.rs - a browser-side fetch would send the query and the user's IP
//! straight to a third party and leave half the agent loop outside the attested
//! boundary. The user's client talks to this app and nothing else.
//!
//! WHAT CROSSES, stated plainly: the tool endpoint sees the ARGUMENTS the model
//! chose and this deployment's egress IP. It does not see the conversation, the
//! user, or anything the model did not put in the call. That is a real disclosure
//! and it is why the playground's tools switch starts off.
//!
//! REACHABILITY: outbound egress on this fleet is IPv6-ONLY. A tool endpoint
//! whose host publishes no AAAA record cannot be dialled at all - see
//! http::egress_err, which says so in the failure rather than leaving an
//! operator hunting for a bad token.
//!
//! MCP, specifically: the streamable-HTTP transport only (JSON-RPC over POST).
//! stdio is impossible here - a wasm component has no subprocesses - and there
//! is nothing to run one in anyway. The component holds NO state between
//! requests, so a server with `discover` on costs three round trips (initialize,
//! notifications/initialized, tools/list) before the prompt can even be built,
//! every turn. Declare `tools` inline on the entry to skip all three when the
//! server's list is stable.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::bindings::wasi::http::types::Method;
use crate::http::{self, HttpReq};

/// The MCP revision this client speaks. Sent in `initialize` and echoed in the
/// `MCP-Protocol-Version` header afterwards, as that revision requires.
const MCP_VERSION: &str = "2025-06-18";

#[derive(Deserialize, Clone, Default)]
pub struct ToolsConfig {
    /// how many tool calls ONE answer may make before the model is told to
    /// answer from what it has. Each call is a round trip AND a full re-prefill
    /// of the conversation, so this is the main cost knob: a 5-hop turn is five
    /// prefills of a share other tenants are waiting for.
    #[serde(default = "default_max_calls")]
    pub max_calls: usize,
    /// per-call timeout, seconds (connect and first byte). Overridable per tool.
    #[serde(default = "default_timeout_s")]
    pub timeout_s: u64,
    /// hard cap on ONE tool response, bytes, before it is even looked at
    #[serde(default = "default_max_bytes")]
    pub max_bytes: usize,
    /// characters of a tool result actually shown to the model. 6000 chars is
    /// roughly 1.5k tokens; truncating here is what stops one chatty endpoint
    /// from evicting the conversation it was called to help with.
    #[serde(default = "default_max_chars")]
    pub max_chars: usize,
    /// whether a client that says nothing gets tools. False (the default) means
    /// the playground's switch starts off and a /v1 caller opts in, which is the
    /// same stance web search takes: reaching outside is the user's deliberate
    /// act, never a default they inherit.
    #[serde(default)]
    pub default_on: bool,
    /// wall-clock budget for ONE answer's tool loop, seconds. Calls, waits
    /// and the regenerations between them all count against it, and once it
    /// is spent the model is told to finish from what it has - the same
    /// once-then-refuse rule max_calls uses. This is what bounds a loop that
    /// waits: thirty calls that each sleep ten minutes would otherwise be a
    /// five-hour answer. A request may lower it (`loop.max_seconds`), never
    /// raise it.
    #[serde(default = "default_max_seconds")]
    pub max_seconds: u64,
    /// the longest ONE `wait` call may sleep, seconds. A longer ask is
    /// clamped and the result says so; the model calls again to keep
    /// waiting, which is the point: each call is a moment the loop's
    /// remaining budget is restated to it, and a stop reaches the turn.
    #[serde(default = "default_wait_max_s")]
    pub wait_max_s: u64,
    /// how many of an answer's most recent tool results stay in the prompt
    /// in full. Older ones are condensed to their head and tail once the
    /// model has acted on them. Every step of the loop re-prefills the whole
    /// conversation, so a thirty-step run whose every test log stayed whole
    /// would spend most of its time (and its context window) re-reading
    /// failures it already fixed.
    #[serde(default = "default_keep_results")]
    pub keep_results: usize,
    /// SUBAGENTS: how many ONE answer may spawn in total, however they nest.
    /// Zero (the default) means the `spawn_agent` tool does not exist. A
    /// positive number is the deployer's consent and the only switch: the
    /// tool is offered to every loop that could still spawn one, and
    /// withdrawn from a loop that cannot (the count is spent, or it sits at
    /// the depth limit). The count is per ANSWER, not per agent, because
    /// what it bounds is cost - each child is a whole loop of generations
    /// on the same share - and cost is the answer's.
    #[serde(default)]
    pub max_agents: u32,
    /// how deep subagents may nest: 1 = the answer may spawn children that
    /// cannot spawn; 2 = grandchildren; the default 3 allows one level more.
    /// The count above is the real bound; this stops a runaway chain from
    /// spending it one deep call at a time.
    #[serde(default = "default_max_agent_depth")]
    pub max_agent_depth: u32,
    /// a subagent's OWN call budget (it runs its own loop). Absent = the
    /// answer's max_calls. Its clock is never its own: a child gets what is
    /// LEFT of the answer's max_seconds, so no tree outlives the answer.
    #[serde(default)]
    pub agent_max_calls: Option<usize>,
    /// Capabilities this deployment ALREADY has, handed to the model as tools
    /// instead of being decided for it by a pre-pass: `["web_search",
    /// "request", "generate_image", "view_image"]`. Each is backed by its own
    /// config block and never appears without it: web_search and request by
    /// `search` (the block where a deployment consents to the model reaching
    /// the web at all - and request can SEND data out, which deserves that
    /// gate more than reading does), generate_image by `image`, view_image by
    /// `vision_service`. The legacy names fetch_url and post_url still parse -
    /// on-chain config CIDs are immutable - and both resolve to `request`.
    /// `wait` is the exception that needs no block: it sleeps inside the
    /// enclave (see Builtin::Wait) and nothing leaves, so naming it here is
    /// the whole consent.
    ///
    /// The principle is one decider per capability. The router decides before
    /// the model has thought about the question, from four messages of
    /// context, and it gets one guess; as a tool the model asks when it finds
    /// it needs to, with the query (or prompt, or question) it actually wants,
    /// and can ask again after reading what came back. The cost is a
    /// re-prefill per call (see max_calls) against the router's single extra
    /// generation.
    ///
    /// Arming a capability as a tool TURNS THE PRE-PASS FOR IT OFF for that
    /// turn, so a deployment never pays for both: web_search silences the
    /// router's search verdict, generate_image its image verdict, and
    /// view_image stands the delegated-vision pre-pass down (the pictures
    /// leave the prompt and wait for the model to ask).
    #[serde(default)]
    pub builtin: Vec<String>,
    /// The search leg's home since 0.39: provider, key, result and fetch
    /// budgets (see search.rs). It lives HERE because it is an external
    /// capability like every other entry in this block - it backs the
    /// web_search and request builtins, and the pre-pass router falls back to
    /// it on turns where the tools are not armed. The legacy top-level
    /// `search` block still parses (config CIDs are immutable on-chain);
    /// AppConfig::search_cfg resolves the two, this one winning.
    #[serde(default)]
    pub search: Option<crate::search::SearchConfig>,
    /// plain HTTP endpoints, described here in full. This is the fully
    /// general half: ANY API becomes a tool, including ones that produce a
    /// picture for the client (`result: {"image": ...}`) or read the turn's
    /// attached pictures (`"$images"` in the body template) - which is how a
    /// deployment wires image generation and vision since 0.39, against any
    /// endpoint rather than through bespoke config blocks.
    #[serde(default)]
    pub http: Vec<HttpTool>,
    /// MCP servers, whose tools are discovered (or declared inline)
    #[serde(default)]
    pub mcp: Vec<McpServer>,
}

/// What this TURN wires the tool registry to. The search leg is the one
/// capability that stays bespoke (provider abstraction, the citation list,
/// the router fallback all hang off it); everything else external is a plain
/// http entry, and the per-turn facts here gate which of those are offered.
#[derive(Clone, Copy, Default)]
pub struct Builtins<'a> {
    pub search: Option<&'a crate::search::SearchConfig>,
    /// the CLIENT withheld the web for this turn (the playground's search
    /// switch is off). Web-backed builtins are then skipped silently: not
    /// showing the model a tool is a stronger guarantee than asking it not to
    /// use one, and a user's choice is not a misconfiguration to report.
    pub web_withheld: bool,
    /// the tool GROUPS the client switched off this turn (see GROUP_SEARCH):
    /// every http entry and MCP server under one of them is skipped the same
    /// silent way. "search" is expressed through web_withheld, not here.
    pub off: &'a [String],
    /// the conversation carries at least one attached image. Without one, an
    /// http tool that asks for `$images` is silently not offered: a tool with
    /// nothing to look at is prompt noise, not a misconfiguration.
    pub images_present: bool,
    /// the SERVING model reads pictures itself this turn (a vision volume,
    /// prefer_local or named outright), so image-reading tools are silently
    /// stood down: there is nothing to delegate.
    pub images_local: bool,
    /// the most ONE wait call may sleep right now, seconds: the config's
    /// wait_max_s, clamped by what is left of the answer's time budget. Zero
    /// means no wait is possible (the budget is spent), and the call says so
    /// instead of sleeping.
    pub wait_cap_s: u64,
    /// what is left of the answer's wall-clock budget, seconds, so a wait
    /// can tell the model how much room its loop has after this one
    pub turn_left_s: u64,
    /// how many subagents THIS loop could spawn right now: the answer's
    /// remaining count, or zero at the depth limit. Zero withdraws the
    /// spawn_agent tool from the loop's registry.
    pub agent_slots: u32,
    /// the account behind this turn's credential (a verified sign-in token or
    /// a derived API key), or None on an anonymous turn. An http entry names
    /// it with `"$user"` as a whole header value and this app fills it in,
    /// which is how a per-user endpoint (a jot notebook) learns whose notes a
    /// call is about without the model ever choosing. Filled from the request
    /// headers before the body is read, never from the body or the model.
    pub user: Option<&'a str>,
    /// the deployment's max_agents, so a loop with no slots left is told
    /// apart from a deployment that never had the feature (the first is
    /// silent, the second is a note if someone named the tool anyway)
    pub agent_limit: u32,
}

/// The name of the subagent tool, which the answer loop runs itself: a
/// child is a whole loop of generations, and only a leg can generate.
pub const AGENT_TOOL: &str = "spawn_agent";

/// TOOL GROUPS: what a person switches on and off. A group is one tool as the
/// settings panel shows it, made of any number of endpoints (see
/// HttpTool::group). Two groups are the app's own legs rather than config
/// entries, and keep the names their switches always had on the wire:
/// "search" is the search provider, the web_search/request builtins and the
/// pre-turn search leg together; "images" is the image service, the pre-turn
/// image leg and every picture-making or picture-reading endpoint together.
pub const GROUP_SEARCH: &str = "search";
pub const GROUP_IMAGES: &str = "images";

/// A group's label for people: "notes_api" -> "Notes api".
pub fn group_label(name: &str) -> String {
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        let c = if c == '_' || c == '-' { ' ' } else { c };
        if i == 0 { out.extend(c.to_uppercase()); } else { out.push(c); }
    }
    out
}

#[derive(Clone, Copy, PartialEq)]
pub enum Builtin {
    WebSearch,
    Request,
    /// Sleep, in the enclave, then carry on with the same answer. The tool
    /// that makes a loop able to WAIT: a job put in the background on the
    /// machine because it outlasts one command's timeout, a service coming
    /// up, a rate limit. The sleep parks the request in the host's poll loop
    /// (no CPU) and ticks a status line every few seconds, because every hop
    /// between here and the browser cuts a stream that goes quiet for ~180s.
    Wait,
    /// Spawn a subagent: a fresh loop with the same tools and an EMPTY
    /// context, given one task, whose final message comes back as this
    /// call's result. Executed by the answer loop (lib.rs, ToolLoop::
    /// spawn_child), never by tools::call - it is a generation, not a
    /// request. Children may spawn children; the per-answer count and the
    /// depth limit (AgentTree) are what stop that being unbounded.
    Agent,
}

impl Builtin {
    fn parse(name: &str) -> Option<Builtin> {
        match name.trim() {
            "web_search" => Some(Builtin::WebSearch),
            // fetch_url / post_url are the pre-0.38 names for what is now ONE
            // request tool. Config CIDs are immutable on-chain, so the old
            // names must keep resolving forever.
            "request" | "fetch_url" | "post_url" => Some(Builtin::Request),
            "wait" => Some(Builtin::Wait),
            "spawn_agent" => Some(Builtin::Agent),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Builtin::WebSearch => "web_search",
            Builtin::Request => "request",
            Builtin::Wait => "wait",
            Builtin::Agent => AGENT_TOOL,
        }
    }

    fn description(self) -> &'static str {
        match self {
            // This description is the ONLY instruction that reaches an agent
            // client: a request carrying its own system message replaces the
            // deployment's prompt entirely (build_prompt), so the two-attempt
            // rule has to live here to bind on the case that needs it.
            Builtin::WebSearch =>
                "Search the web and get back numbered results with page text. Use it for any \
                 fact about the world you cannot verify from this conversation, including ones \
                 you believe you remember. TWO-ATTEMPT RULE: if you have already tried to \
                 reconstruct the same thing from memory twice - a layout, a table, a spec, a \
                 set of constants, an exact quote - stop and call this instead. A third attempt \
                 from memory is never better than one search. Re-deriving reference data step \
                 by step in your reasoning, or writing \"let me recall\" a second time, is the \
                 signal that you needed to search rather than think harder. Cite what you use \
                 as [1], [2].",
            Builtin::Request =>
                "Send an HTTP request to a URL and return the response text. GET (the default) \
                 reads a page or API - use it to read a web_search result in full, a URL the \
                 user gave you, or a JSON endpoint. POST/PUT/PATCH/DELETE send `body` to an \
                 API or webhook the user pointed you at - tell the user what you sent and \
                 where.",
            Builtin::Wait =>
                "Pause for `seconds`, then continue this same answer. Use it when something \
                 you started needs time before it is worth checking on: a job you put in the \
                 background on the machine (nohup ... &) because it outlasts one command's \
                 timeout, a service that is still coming up, a rate limit you hit. Choose the \
                 length from what you know about the job - one wait of the right size beats \
                 many short polls - and read the result: it says how much of this answer's \
                 time budget is left. Nothing leaves the enclave; the user sees a countdown.",
            Builtin::Agent =>
                "Spawn a subagent: a fresh copy of yourself with the same tools and the same \
                 machine but an EMPTY context, given ONE task, which works it to completion and \
                 returns a written report as this call's result. Use it to keep your own context \
                 clean on a big job - a long investigation, a separate part of the work, a check \
                 you want done from scratch by fresh eyes. Write `task` as a brief to a capable \
                 colleague who knows nothing of this conversation: the goal, the check that says \
                 it is done, where the files are, what has been tried; put the details in \
                 `context` and say in `expect` what the report must contain. Subagents run one \
                 at a time, each with its own call budget inside this answer's remaining time; \
                 an answer may spawn only so many in total, and each may spawn its own. Prefer \
                 doing a few calls yourself over spawning an agent for them.",
        }
    }

    fn schema(self) -> serde_json::Value {
        match self {
            Builtin::WebSearch => serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "what to search for, as you would type it into a search box",
                    }
                },
                "required": ["query"],
            }),
            Builtin::Request => serde_json::json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "absolute http(s) URL" },
                    "method": {
                        "type": "string",
                        "enum": ["GET", "POST", "PUT", "PATCH", "DELETE"],
                        "description": "HTTP method (default GET)",
                    },
                    "body": {
                        "description": "request body, required for POST/PUT/PATCH: a JSON \
                                        object is sent as JSON, a string is sent exactly as \
                                        written",
                    },
                    "content_type": {
                        "type": "string",
                        "description": "MIME type of the body (default application/json)",
                    }
                },
                "required": ["url"],
            }),
            Builtin::Wait => serde_json::json!({
                "type": "object",
                "properties": {
                    "seconds": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "how long to pause, in seconds",
                    },
                    "reason": {
                        "type": "string",
                        "description": "what you are waiting for, in a few words - shown \
                                        to the user beside the countdown",
                    }
                },
                "required": ["seconds"],
            }),
            Builtin::Agent => serde_json::json!({
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "the brief: the goal, the check that says it is done, \
                                        where the files are, what has been tried",
                    },
                    "context": {
                        "type": "string",
                        "description": "everything else the subagent needs to know - it \
                                        cannot see this conversation",
                    },
                    "expect": {
                        "type": "string",
                        "description": "what the report must contain, in a sentence",
                    }
                },
                "required": ["task"],
            }),
        }
    }

    /// Which config block has to be present for this to be offered at all.
    fn available(self, b: &Builtins) -> bool {
        match self {
            Builtin::WebSearch | Builtin::Request => b.search.is_some(),
            Builtin::Wait => true,
            Builtin::Agent => b.agent_slots > 0,
        }
    }

    /// An unavailable builtin that is a CHOICE, not a misconfiguration: the
    /// user's switch withheld it. Skipped without a note.
    fn withheld(self, b: &Builtins) -> bool {
        match self {
            Builtin::WebSearch | Builtin::Request => b.web_withheld,
            Builtin::Wait => false,
            // configured, but this loop may not spawn (count spent, or at
            // the depth limit): a per-loop fact, not a misconfiguration
            Builtin::Agent => b.agent_limit > 0,
        }
    }

    /// What is missing when it is neither available nor deliberately withheld.
    fn missing(self) -> &'static str {
        match self {
            Builtin::WebSearch | Builtin::Request => "`search`",
            Builtin::Wait => "nothing",
            Builtin::Agent => "`max_agents` (a positive count in the `tools` block)",
        }
    }
}

/// What ONE answer's loop may spend, after the request has had its say. The
/// config is the ceiling: a request LOWERS a figure (a client that wants a
/// quick answer, a playground turn that is not a task) and never raises one.
/// `persist` is the `loop` request field: keep working at a verifiable goal
/// until the check passes, rather than answering at the first opportunity.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Budget {
    pub max_calls: usize,
    pub max_seconds: u64,
    pub persist: bool,
    /// subagents the whole answer may spawn (see ToolsConfig::max_agents)
    pub max_agents: u32,
    pub max_agent_depth: u32,
}

impl Budget {
    /// A budget of `n` calls and the default hour, not persisting, no
    /// subagents: the tests' way of saying "a call count and nothing else".
    #[cfg(test)]
    pub fn calls(n: usize) -> Budget {
        Budget {
            max_calls: n,
            max_seconds: default_max_seconds(),
            persist: false,
            max_agents: 0,
            max_agent_depth: default_max_agent_depth(),
        }
    }

    /// The wall-clock figure as the model should read it.
    pub fn time(&self) -> String {
        human_secs(self.max_seconds)
    }
}

/// "45 seconds", "12 minutes", "1 hour 30 minutes": a duration as words, for
/// prompts and results. Seconds only show under two minutes; past that the
/// model is planning in minutes anyway.
pub fn human_secs(s: u64) -> String {
    if s < 120 {
        return format!("{s} second{}", if s == 1 { "" } else { "s" });
    }
    let (h, m) = (s / 3600, (s % 3600) / 60);
    let mut out = String::new();
    if h > 0 {
        out.push_str(&format!("{h} hour{}", if h == 1 { "" } else { "s" }));
    }
    if m > 0 {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&format!("{m} minute{}", if m == 1 { "" } else { "s" }));
    }
    out
}

/// How often a sleeping wait ticks its status line, seconds. Well inside the
/// 180s idle cut every proxy hop applies and the playground's own 300s stall
/// watchdog; a tick is a few dozen bytes.
pub const WAIT_TICK_S: u64 = 5;

/// What a wait call will actually do: `(seconds to sleep, reason, note)`.
/// The seconds are clamped to `cap_s` and the note, when the ask was longer,
/// tells the model so and how to keep waiting. Pure, so it is testable
/// without a clock.
pub fn wait_plan(args: &serde_json::Value, cap_s: u64) -> Result<(u64, String, String), String> {
    let secs = match args.get("seconds") {
        Some(serde_json::Value::Number(n)) => n.as_f64().map(|f| f.round().max(0.0) as u64),
        // a model that writes "30s" or "30" meant thirty
        Some(serde_json::Value::String(s)) => s
            .trim()
            .trim_end_matches(['s', 'S'])
            .trim()
            .parse::<f64>()
            .ok()
            .map(|f| f.round().max(0.0) as u64),
        _ => None,
    }
    .ok_or("wait needs `seconds`, a positive integer")?;
    if secs == 0 {
        return Err("wait needs `seconds` of at least 1".into());
    }
    if cap_s == 0 {
        return Err("no waiting is possible now: this answer's time budget is spent. Do not \
                    call anything else; finish from what you have."
            .into());
    }
    let reason = args
        .get("reason")
        .and_then(|r| r.as_str())
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .unwrap_or("")
        .to_string();
    let note = if secs > cap_s {
        format!(
            " You asked for {}; the most one wait may sleep right now is {}, so it stopped \
             there - call wait again to keep waiting.",
            human_secs(secs),
            human_secs(cap_s)
        )
    } else {
        String::new()
    };
    Ok((secs.min(cap_s), reason, note))
}

fn default_max_calls() -> usize {
    3
}
fn default_max_seconds() -> u64 {
    3600
}
fn default_wait_max_s() -> u64 {
    600
}
fn default_keep_results() -> usize {
    3
}
fn default_max_agent_depth() -> u32 {
    3
}
fn default_timeout_s() -> u64 {
    20
}
fn default_max_bytes() -> usize {
    256 * 1024
}
fn default_max_chars() -> usize {
    6000
}
fn default_true() -> bool {
    true
}

impl ToolsConfig {
    pub fn is_empty(&self) -> bool {
        self.http.is_empty() && self.mcp.is_empty() && self.builtin.is_empty()
    }

    /// What one answer may spend, given the request's `loop` field: absent
    /// or a boolean keeps the config's figures and sets persistence; an
    /// object lowers max_calls and max_seconds (never raises them - the
    /// config is the deployment's ceiling, published on-chain) and persists
    /// unless it says `"persist": false`. Nothing here ever reaches zero: a
    /// loop with no calls would refuse its first one, which is a failed
    /// answer dressed as a budget.
    pub fn budget(&self, req: Option<&serde_json::Value>) -> Budget {
        let mut b = Budget {
            max_calls: self.max_calls.max(1),
            max_seconds: self.max_seconds.max(1),
            persist: false,
            max_agents: self.max_agents,
            max_agent_depth: self.max_agent_depth.max(1),
        };
        match req {
            Some(serde_json::Value::Bool(p)) => b.persist = *p,
            Some(serde_json::Value::Object(o)) => {
                b.persist = o.get("persist").and_then(|v| v.as_bool()).unwrap_or(true);
                if let Some(n) = o.get("max_calls").and_then(|v| v.as_u64()) {
                    b.max_calls = b.max_calls.min(n as usize).max(1);
                }
                if let Some(s) = o.get("max_seconds").and_then(|v| v.as_u64()) {
                    b.max_seconds = b.max_seconds.min(s).max(1);
                }
                // zero is allowed here: "no subagents for this answer" is a
                // choice a client can make, unlike "no calls"
                if let Some(a) = o.get("max_agents").and_then(|v| v.as_u64()) {
                    b.max_agents = b.max_agents.min(a as u32);
                }
                if let Some(d) = o.get("max_agent_depth").and_then(|v| v.as_u64()) {
                    b.max_agent_depth = b.max_agent_depth.min(d as u32).max(1);
                }
            }
            _ => {}
        }
        b
    }

    /// The HTTP tool names a turn would actually be offered - the same name
    /// check and first-wins deduplication `build` applies, without dialling
    /// anything. /models advertises these, and advertising a name that
    /// resolution then drops would put a tool in the UI that cannot be called.
    /// MCP names are deliberately absent: knowing them means a round trip.
    /// The switch http entry `i` sits under: its own `group` when it names
    /// one, "images" when it is about pictures, else the family name its
    /// function name shares with at least one sibling (`notes_list`,
    /// `notes_write`, ... -> "notes": a family of endpoints is one tool, and
    /// a config written before groups existed should not turn into six
    /// switches), else the function name itself.
    pub fn group_of(&self, i: usize) -> String {
        let t = &self.http[i];
        if let Some(g) = t.own_group() {
            return g;
        }
        if let Some(fam) = t.name.split(['_', '-']).next().filter(|f| !f.is_empty() && *f != t.name) {
            let siblings = self.http.iter().enumerate().filter(|(j, o)| {
                *j != i && o.own_group().is_none()
                    && o.name.split(['_', '-']).next() == Some(fam)
            });
            if siblings.count() > 0 {
                return fam.to_string();
            }
        }
        t.name.clone()
    }

    /// The switchable tools this config defines, in the order the settings
    /// panel shows them (see GROUP_SEARCH): name, the functions under it, and
    /// the switch's starting position. "search" and "images" are listed only
    /// when the caller says the deployment has them (they are the app's own
    /// legs, configured outside this block); every other group is the http
    /// entries and MCP servers that share a name.
    pub fn groups(&self, search: Option<bool>, images: Option<bool>) -> Vec<serde_json::Value> {
        let mut out: Vec<serde_json::Value> = Vec::new();
        let mut push = |name: String, kind: &str, tools: Vec<String>, default_on: bool| {
            if let Some(g) = out.iter_mut().find(|g| g["name"] == name) {
                for t in tools {
                    g["tools"].as_array_mut().unwrap().push(serde_json::Value::String(t));
                }
                return;
            }
            out.push(serde_json::json!({
                "name": name.clone(), "label": group_label(&name), "kind": kind,
                "tools": tools, "default_on": default_on,
            }));
        };
        if let Some(on) = search {
            let mut names = Vec::new();
            for b in &self.builtin {
                if let Some(k @ (Builtin::WebSearch | Builtin::Request)) = Builtin::parse(b) {
                    if !names.iter().any(|n| n == k.name()) {
                        names.push(k.name().to_string());
                    }
                }
            }
            push(GROUP_SEARCH.into(), "search", names, on);
        }
        if let Some(on) = images {
            push(GROUP_IMAGES.into(), "images", Vec::new(), on);
        }
        for (i, t) in self.http.iter().enumerate() {
            if check_name(&t.name).is_err() {
                continue;
            }
            let g = self.group_of(i);
            let kind = if g == GROUP_IMAGES { "images" } else { "http" };
            let on = if g == GROUP_IMAGES { images.unwrap_or(self.default_on) } else { self.default_on };
            push(g, kind, vec![t.name.clone()], on);
        }
        for s in &self.mcp {
            push(s.group_name(), "mcp", Vec::new(), self.default_on);
        }
        out
    }

    pub fn http_names(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for b in &self.builtin {
            // parse() folds the legacy fetch_url/post_url aliases into ONE
            // request tool, so the same dedup build() applies must apply here
            if let Some(k) = Builtin::parse(b) {
                if !out.iter().any(|n| n == k.name()) {
                    out.push(k.name().to_string());
                }
            }
        }
        // spawn_agent needs no naming (see max_agents); it is what an answer
        // would be offered, so it is advertised like the rest
        if self.max_agents > 0 && !out.iter().any(|n| n == AGENT_TOOL) {
            out.push(AGENT_TOOL.to_string());
        }
        for t in &self.http {
            if check_name(&t.name).is_ok() && !out.iter().any(|n| n == &t.name) {
                out.push(t.name.clone());
            }
        }
        out
    }
}

/// One HTTP endpoint exposed to the model as a function.
///
/// Arguments reach the request in the way the endpoint expects: `{name}`
/// placeholders in the URL are substituted (percent-encoded), whatever is left
/// becomes the query string on a GET and the JSON body on a POST. A `body`
/// template overrides that for APIs whose shape is not simply the arguments.
#[derive(Deserialize, Clone)]
pub struct HttpTool {
    pub name: String,
    /// what the model is told this does. This is the whole user interface: a
    /// vague description is why a model calls the wrong tool or none at all.
    #[serde(default)]
    pub description: String,
    /// The TOOL this endpoint belongs to, as a person sees it: one notebook
    /// is six endpoints (list, read, write, append, search, delete), and the
    /// settings panel shows one switch for it, not six. Entries that share a
    /// group share that switch. Absent: an endpoint that reads or makes a
    /// picture belongs to "images" (beside the image service, if any), and
    /// anything else is a tool of its own, named by its function name.
    #[serde(default)]
    pub group: Option<String>,
    /// JSON Schema for the arguments (an object schema). Absent = no arguments.
    #[serde(default)]
    pub parameters: Option<serde_json::Value>,
    /// absolute URL. May contain `{arg}` placeholders.
    pub url: String,
    /// GET (default) | POST | PUT | PATCH | DELETE
    #[serde(default)]
    pub method: Option<String>,
    /// extra headers. Reference secrets by name (`"Bearer $TOOL_KEY"`), never
    /// literals - the config is published on-chain by CID.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// JSON body template. `"$arg"` anywhere in it (as a whole string value) is
    /// replaced by that argument's value; absent on a POST sends the arguments
    /// object as-is.
    #[serde(default)]
    pub body: Option<serde_json::Value>,
    /// send leftover arguments as a query string even on a POST
    #[serde(default)]
    pub query: Option<bool>,
    #[serde(default)]
    pub timeout_s: Option<u64>,
    #[serde(default)]
    pub max_bytes: Option<usize>,
    #[serde(default)]
    pub max_chars: Option<usize>,
    /// how the HTTP response becomes a RESULT. Absent = the whole body as
    /// text. This is what lets an arbitrary API produce a picture: a tool
    /// with `result: {"image": "data.0.b64_json"}` delivers the extracted
    /// image to the CLIENT and tells the model a picture was made.
    #[serde(default)]
    pub result: Option<ResultMap>,
    /// FORMATTING AS A PROMPT: an instruction applied to the response by a
    /// short internal pass before the model sees it ("render the results as
    /// a numbered list: [n] Title - URL - snippet"). This is how an arbitrary
    /// API's response gets the shaping the bespoke search leg does in code -
    /// configured, not programmed. Costs one short greedy generation per
    /// call; any failure keeps the raw text.
    #[serde(default)]
    pub format: Option<String>,
    /// ROUTING AS A PROMPT: when this tool should fire on a turn where the
    /// tool LOOP is not armed (most commonly a model without the trained
    /// call format). One classifier pass reads the conversation and these
    /// lines, picks a service or NONE, and the result is folded into the
    /// turn the way a routed search always has been. Ignored on armed turns:
    /// there the description does this job, after the model has thought.
    #[serde(default)]
    pub route: Option<String>,
    /// which parameter the routed line binds to. Optional when the entry has
    /// exactly one required parameter, which is then used.
    #[serde(default)]
    pub route_arg: Option<String>,
    /// CITATIONS, generically: dot paths into the JSON response naming the
    /// hit array and each hit's title/url, so a model-called (or routed)
    /// tool feeds the same numbered source list a routed search does.
    #[serde(default)]
    pub sources: Option<SourcesMap>,
}

/// Where a response's citable hits live: `list` names the array, `title` and
/// `url` name fields WITHIN one hit.
#[derive(Deserialize, Clone)]
pub struct SourcesMap {
    pub list: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

/// Where in a JSON response the result lives, and what it IS. Paths are
/// dot-separated keys and array indexes ("data.0.b64_json").
#[derive(Deserialize, Clone, Default)]
pub struct ResultMap {
    /// the field holding a picture, as base64 or a data URI. The bytes go to
    /// the client through the leg's image delivery, never into the prompt.
    #[serde(default)]
    pub image: Option<String>,
    /// the field holding the text the model should read, extracted instead of
    /// handing it the whole response. A path that does not resolve falls back
    /// to the full body: a readable result beats an error.
    #[serde(default)]
    pub text: Option<String>,
}

impl HttpTool {
    /// The body template asks for the turn's attached pictures (`"$images"`
    /// expands to an array of data URIs, `"$image"` to the first one). Such a
    /// tool is only offered on turns that HAVE pictures, and it is what makes
    /// vision "just a tool": any API that takes images can receive them.
    pub fn wants_images(&self) -> bool {
        fn scan(v: &serde_json::Value) -> bool {
            match v {
                serde_json::Value::String(s) => {
                    matches!(s.trim(), "$images" | "$image" | "${images}" | "${image}")
                }
                serde_json::Value::Array(a) => a.iter().any(scan),
                serde_json::Value::Object(o) => o.values().any(scan),
                _ => false,
            }
        }
        self.body.as_ref().is_some_and(scan)
    }

    /// The response carries a picture for the client (see ResultMap).
    pub fn makes_image(&self) -> bool {
        self.result.as_ref().is_some_and(|r| r.image.is_some())
    }

    /// The switch this endpoint sits under when it says so, or when it is
    /// about pictures; None = decided among its siblings (ToolsConfig::group_of).
    fn own_group(&self) -> Option<String> {
        match self.group.as_deref().map(str::trim) {
            Some(g) if !g.is_empty() => Some(g.to_string()),
            _ if self.makes_image() || self.wants_images() => Some(GROUP_IMAGES.to_string()),
            _ => None,
        }
    }

    /// The parameter a routed line binds to: `route_arg`, or the entry's
    /// sole required parameter. None means `route` cannot be used.
    pub fn route_binding(&self) -> Option<String> {
        if let Some(a) = self.route_arg.as_deref() {
            return Some(a.to_string());
        }
        let req = self.parameters.as_ref()?.get("required")?.as_array()?;
        if req.len() == 1 {
            return req[0].as_str().map(str::to_string);
        }
        None
    }
}

/// One MCP server reachable over the streamable-HTTP transport.
#[derive(Deserialize, Clone)]
pub struct McpServer {
    /// the MCP endpoint, e.g. "https://<id8>.app.enclave.host/mcp"
    pub url: String,
    /// the switch this server's tools sit under (see HttpTool::group);
    /// absent: the server's prefix, else its host
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// keep only these tool names (empty = keep all the server offers)
    #[serde(default)]
    pub allow: Vec<String>,
    /// drop these, applied after `allow`
    #[serde(default)]
    pub deny: Vec<String>,
    /// prepended to every tool name from this server, so two servers offering
    /// `search` can both be attached
    #[serde(default)]
    pub prefix: Option<String>,
    /// declare the tools here and skip discovery entirely (three round trips
    /// per turn saved; the cost is that the list goes stale silently)
    #[serde(default)]
    pub tools: Vec<McpToolDecl>,
    #[serde(default = "default_true")]
    pub discover: bool,
    #[serde(default)]
    pub timeout_s: Option<u64>,
    #[serde(default)]
    pub protocol_version: Option<String>,
}

#[derive(Deserialize, Clone)]
pub struct McpToolDecl {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub parameters: Option<serde_json::Value>,
}

impl McpServer {
    /// The switch this server's tools sit under (see `group`).
    pub fn group_name(&self) -> String {
        if let Some(g) = self.group.as_deref().map(str::trim).filter(|g| !g.is_empty()) {
            return g.to_string();
        }
        let p = self.prefix.as_deref().unwrap_or("").trim_end_matches(['_', '-']);
        if !p.is_empty() {
            return p.to_string();
        }
        host_of(&self.url)
    }
}

/// Where a resolved tool actually goes.
#[derive(Clone)]
pub enum ToolSrc {
    /// a capability the app already has, exposed as a tool
    Builtin(Builtin),
    /// index into ToolsConfig::http
    Http(usize),
    /// index into Registry::mcp, plus the name the SERVER knows it by (which
    /// differs from the exposed name whenever a prefix is set)
    Mcp { server: usize, remote: String },
    /// declared by the REQUEST (the /v1 passthrough): rendered into the
    /// prompt, never executed here - the call goes back to the client
    Client,
}

/// A tool as the model sees it.
#[derive(Clone)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    pub src: ToolSrc,
}

/// One MCP connection for the life of ONE request. The component keeps nothing
/// between requests, so this is opened, used and dropped within a single turn.
pub struct McpSession {
    url: String,
    headers: Vec<(String, String)>,
    session_id: Option<String>,
    version: String,
    timeout_s: u64,
    next_id: u64,
}

/// Everything callable this turn, plus whatever went wrong assembling it.
#[derive(Default)]
pub struct Registry {
    pub tools: Vec<Tool>,
    /// non-fatal problems worth telling the operator about: a server that would
    /// not answer, a name that collided, a schema that was not an object. A
    /// broken tool must never be the reason an answer fails, so these are
    /// reported and the turn proceeds with the tools that DID resolve.
    pub notes: Vec<String>,
    mcp: Vec<McpSession>,
}

impl Registry {
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn find(&self, name: &str) -> Option<&Tool> {
        self.tools.iter().find(|t| t.name == name)
    }

    /// Some armed tool produces a picture for the client. The router's image
    /// verdict stands down when this is true: the model owns the decision.
    pub fn makes_image(&self, cfg: &ToolsConfig) -> bool {
        self.tools.iter().any(|t| match t.src {
            ToolSrc::Http(i) => cfg.http[i].makes_image(),
            _ => false,
        })
    }

    /// The name of the first armed tool that reads the turn's pictures, if
    /// any. The legs use it to stand the vision pre-pass down, stash the
    /// pictures for the tool, and tell the model what to call.
    pub fn image_reader<'a>(&'a self, cfg: &ToolsConfig) -> Option<&'a str> {
        self.image_tool_names(cfg).0.or(self.image_tool_names(cfg).1)
    }

    /// The armed image-taking tools by NATURE: (reader, transformer). A
    /// reader takes pictures and answers in text (view_image); a transformer
    /// takes one and produces another (upscale_image). The stash note names
    /// them for what they do - telling the model to "look" with a tool that
    /// only upscales earns a call that cannot answer the question.
    pub fn image_tool_names<'a>(&'a self, cfg: &ToolsConfig) -> (Option<&'a str>, Option<&'a str>) {
        let pick = |transform: bool| {
            self.tools
                .iter()
                .find(|t| match t.src {
                    ToolSrc::Http(i) => {
                        cfg.http[i].wants_images() && cfg.http[i].makes_image() == transform
                    }
                    _ => false,
                })
                .map(|t| t.name.as_str())
        };
        (pick(false), pick(true))
    }
}

/// Assemble the registry for one request: config entries verbatim, MCP servers
/// discovered (or read from their inline declarations).
///
/// This runs BEFORE the prompt is built, because the schemas have to be in the
/// prompt. That is what makes discovery expensive, and why `discover: false`
/// with inline `tools` exists.
pub fn build(cfg: &ToolsConfig, b: Builtins, on_status: &dyn Fn(&str)) -> Registry {
    let mut reg = Registry::default();
    // Built-ins first, so a config entry can never shadow web_search with
    // something that merely shares its name.
    for want in &cfg.builtin {
        let Some(k) = Builtin::parse(want) else {
            reg.notes.push(format!(
                "builtin '{want}' is not a tool this app has (known: web_search, request, \
                 wait; pictures are http entries, see the config's tools comment)"
            ));
            continue;
        };
        if !k.available(&b) {
            if !k.withheld(&b) {
                let note = format!(
                    "builtin '{}' needs this deployment's {} block, which is not configured",
                    k.name(),
                    k.missing()
                );
                // fetch_url and post_url alias to one request tool; one
                // missing capability is one note, not one per alias
                if !reg.notes.contains(&note) {
                    reg.notes.push(note);
                }
            }
            continue;
        }
        if reg.find(k.name()).is_some() {
            continue;
        }
        reg.tools.push(Tool {
            name: k.name().to_string(),
            description: k.description().to_string(),
            parameters: k.schema(),
            src: ToolSrc::Builtin(k),
        });
    }
    // spawn_agent needs no naming: a positive max_agents IS the deployer's
    // consent, and the tree says per loop whether one may still be spawned
    // (none past the depth limit, none once the count is spent), so a loop
    // that cannot spawn is never shown the tool at all
    let k = Builtin::Agent;
    if k.available(&b) && reg.find(k.name()).is_none() {
        reg.tools.push(Tool {
            name: k.name().to_string(),
            description: k.description().to_string(),
            parameters: k.schema(),
            src: ToolSrc::Builtin(k),
        });
    }
    for (i, t) in cfg.http.iter().enumerate() {
        // a group the person switched off: not offered, not a note
        if b.off.iter().any(|g| *g == cfg.group_of(i)) {
            continue;
        }
        // a misconfigured route IS worth a note: the operator wrote a prompt
        // that can never fire
        if t.route.is_some() && t.route_binding().is_none() {
            reg.notes.push(format!(
                "tool '{}': `route` needs `route_arg` (or exactly one required parameter) \
                 to bind the routed input to",
                t.name
            ));
        }
        // a tool that asks for the turn's pictures only exists when there are
        // pictures to give it, and not when the serving model reads them
        // itself. Silent either way: a per-turn fact, not a misconfiguration.
        // No pictures this turn = no image-taking tools, reader or not. A
        // model that reads pictures ITSELF (images_local) drops only the
        // READERS - delegating the looking would be absurd - but keeps the
        // transformers (upscale: picture in, picture out), which local
        // vision cannot substitute for.
        if t.wants_images() && (!b.images_present || (b.images_local && !t.makes_image())) {
            continue;
        }
        match check_name(&t.name) {
            Err(e) => reg.notes.push(format!("tool '{}' ignored: {e}", t.name)),
            Ok(()) if reg.find(&t.name).is_some() => {
                reg.notes.push(format!("tool '{}' ignored: the name is already taken", t.name));
            }
            Ok(()) => reg.tools.push(Tool {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: object_schema(t.parameters.clone()),
                src: ToolSrc::Http(i),
            }),
        }
    }
    for (i, s) in cfg.mcp.iter().enumerate() {
        if b.off.iter().any(|g| *g == s.group_name()) {
            continue;
        }
        let timeout = s.timeout_s.unwrap_or(cfg.timeout_s);
        let mut sess = McpSession {
            url: s.url.trim().to_string(),
            headers: resolved_headers(&s.headers, &mut reg.notes, &s.url),
            session_id: None,
            version: s.protocol_version.clone().unwrap_or_else(|| MCP_VERSION.into()),
            timeout_s: timeout,
            next_id: 1,
        };
        let declared: Vec<McpToolDecl> = if s.discover && s.tools.is_empty() {
            on_status(&format!("listing tools on {}…", host_of(&s.url)));
            match discover(&mut sess) {
                Ok(list) => list,
                Err(e) => {
                    reg.notes.push(format!("mcp {}: {e}", host_of(&s.url)));
                    Vec::new()
                }
            }
        } else {
            s.tools.clone()
        };
        for d in declared {
            if !s.allow.is_empty() && !s.allow.iter().any(|a| a == &d.name) {
                continue;
            }
            if s.deny.iter().any(|a| a == &d.name) {
                continue;
            }
            let exposed = format!("{}{}", s.prefix.as_deref().unwrap_or(""), d.name);
            if let Err(e) = check_name(&exposed) {
                reg.notes.push(format!("mcp tool '{exposed}' ignored: {e}"));
                continue;
            }
            if reg.find(&exposed).is_some() {
                reg.notes
                    .push(format!("mcp tool '{exposed}' ignored: the name is already taken"));
                continue;
            }
            reg.tools.push(Tool {
                name: exposed,
                description: d.description.clone(),
                parameters: object_schema(d.parameters.clone()),
                src: ToolSrc::Mcp { server: i, remote: d.name.clone() },
            });
        }
        reg.mcp.push(sess);
    }
    reg
}

/// Function names the model can actually reproduce: a name with a space or a
/// quote in it comes back mangled and matches nothing.
fn check_name(n: &str) -> Result<(), String> {
    if n.is_empty() || n.len() > 64 {
        return Err("a name must be 1-64 characters".into());
    }
    if !n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err("a name may only use letters, digits, '_' and '-'".into());
    }
    Ok(())
}

/// Whatever was configured, as an object schema. A model handed a schema that
/// is not an object tends to answer with a bare value the caller cannot bind.
pub fn object_schema(v: Option<serde_json::Value>) -> serde_json::Value {
    match v {
        Some(v) if v.is_object() => v,
        _ => serde_json::json!({ "type": "object", "properties": {} }),
    }
}

fn host_of(url: &str) -> String {
    http::split_url(url).map(|(_, a, _)| a).unwrap_or_else(|_| url.to_string())
}

/// Header values with an unresolved `$SECRET` are DROPPED, not sent. The
/// platform substitutes `$NAME` in config strings at launch; a placeholder that
/// survived means the secret is not set, and posting the literal string
/// "Bearer $TOOL_KEY" earns a 401 that sends an operator hunting for a bad key
/// instead of a missing secret.
/// The reserved `$user` slot: a header whose WHOLE value is `$user` (or
/// `${user}`) carries the account behind this turn's sign-in or derived API
/// key, filled by this app. It is resolved before the secrets pass so it is
/// never mistaken for a missing secret, and a turn with no signed-in caller
/// fails closed: the endpoint is per-user, and reaching it nameless would be
/// exactly the confusion of identities the slot exists to prevent. Not a
/// secret name that could collide: the platform's secret substitution keeps
/// unknown `$names` literal, and `user` is not one the deployer sets.
fn identity_headers(
    h: &BTreeMap<String, String>,
    user: Option<&str>,
    tool: &str,
) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    for (k, v) in h {
        let slot = matches!(v.trim(), "$user" | "${user}");
        match (slot, user) {
            (false, _) => {
                out.insert(k.clone(), v.clone());
            }
            (true, Some(u)) => {
                out.insert(k.clone(), u.to_string());
            }
            (true, None) => {
                return Err(format!(
                    "the {tool} tool acts on behalf of the signed-in user (its '{k}' header is \
                     $user), and this turn has no signed-in caller. It cannot be used on this \
                     turn; tell the user to sign in with Enclave (or call the API with a derived \
                     key) to reach it. Do not retry."
                ))
            }
        }
    }
    Ok(out)
}

fn resolved_headers(
    h: &BTreeMap<String, String>,
    notes: &mut Vec<String>,
    url: &str,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (k, v) in h {
        if let Some(name) = unresolved_in(v) {
            notes.push(format!(
                "{}: header '{k}' references ${name} but no such secret is set on this \
                 deployment - add {name} to the deployment's secrets (console or set_secrets) \
                 and restart it to apply. The header was NOT sent.",
                host_of(url)
            ));
            continue;
        }
        out.push((k.to_ascii_lowercase(), v.clone()));
    }
    out
}

/// The name behind a `$NAME` / `${NAME}` that nothing substituted, if the value
/// still carries one.
fn unresolved_in(s: &str) -> Option<String> {
    let mut rest = s;
    while let Some(i) = rest.find('$') {
        let after = &rest[i + 1..];
        let (name, tail) = match after.strip_prefix('{') {
            Some(b) => match b.find('}') {
                Some(j) => (&b[..j], &b[j + 1..]),
                None => return None,
            },
            None => {
                let end = after
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .unwrap_or(after.len());
                (&after[..end], &after[end..])
            }
        };
        if !name.is_empty() && !name.starts_with(|c: char| c.is_ascii_digit()) {
            return Some(name.to_string());
        }
        rest = tail;
    }
    None
}

// ------------------------------------------------------------- the prompt --

/// The `# Tools` section appended to the system prompt.
///
/// This is the Hermes/Qwen convention - a `<tools>` block of JSON function
/// signatures and a `<tool_call>` block to answer in - because every model in
/// this app's catalog is a qwen chatml model trained on exactly that. It is
/// rendered by hand for the same reason the rest of the prompt is: there is no
/// jinja template in a wasm component, and the format is a TRAINED property, so
/// a family that was taught a different one needs its own arm here rather than
/// a generic guess (see tools_supported).
pub fn system_block(tools: &[Tool], b: Budget) -> String {
    let mut s = signatures(tools);
    s.push_str(&format!(
        "Rules for this app: the call is executed by the server and its result comes back in a \
         <tool_response> block; wait for it rather than inventing one. Call ONLY the functions \
         listed above, by their exact names - nothing else exists, and a call to anything else \
         is shown to the user as a failure. You may make at most {} call{} in one answer, and \
         the whole answer has {} of wall-clock time, so make each one count. When a call fails, \
         say so plainly and answer from what you have.",
        b.max_calls,
        if b.max_calls == 1 { "" } else { "s" },
        b.time(),
    ));
    s.push_str(&finish_rule(tools, b));
    s
}

/// The closing rule of every server-loop block: answer at the first
/// opportunity, or - when the request asked for the LOOP - keep going until
/// the check passes. The persistence text is what turns a model that reports
/// its first failing test run into one that fixes it; without it every
/// reasoning model this app serves stops to "check with the user" after one
/// attempt, which in a loop nobody is watching is the same as giving up.
fn finish_rule(tools: &[Tool], b: Budget) -> String {
    if !b.persist {
        return " When you have enough to answer, stop calling and write the answer.".into();
    }
    let has_wait = tools.iter().any(|t| t.name == "wait");
    let has_agent = tools.iter().any(|t| t.name == AGENT_TOOL);
    format!(
        " WORKING TO A CHECK: the user wants the goal reached, not a first attempt. When the \
         goal comes with a way to verify it (tests, a harness, a build, a command that must \
         succeed), keep going until the check passes: run it, read what failed, change one \
         thing, run it again. Do not stop to ask permission or to report progress - nobody is \
         answering while you work, and the user reads only your final answer. Keep state in \
         files on the machine, never in your head, so a later step can pick up where an \
         earlier one left off.{}{} Stop early only when the check passes, when you are certain \
         it cannot pass with what you have, or when the budget is nearly spent - and then \
         report exactly what passes, what does not, and where the files are.",
        if has_wait {
            " When something you started needs time, call wait rather than polling it in a \
             tight loop of commands."
        } else {
            ""
        },
        if has_agent {
            " On a big job, keep your own context clean with spawn_agent: brief a subagent \
             with one self-contained part of the work and read its report."
        } else {
            ""
        },
    )
}

/// The block for CLIENT-declared tools (the /v1 passthrough). Same trained
/// format, different contract: the model still writes `<tool_call>`, but the
/// call ends the turn and goes back to the client as OpenAI `tool_calls`; the
/// result returns as a `<tool_response>` block in the NEXT request. From where
/// the model sits that is indistinguishable from the server loop, so the rules
/// only drop what is no longer true (the per-answer budget - the client owns
/// the loop).
///
/// `require` is OpenAI's forcing `tool_choice`: Some("") for `"required"`,
/// Some(name) for a named function. It can only be ASKED for - this runtime
/// has no grammar constraint - so it arrives as an instruction, and a model
/// that answers in prose anyway is reported as it stands.
pub fn client_system_block(tools: &[Tool], require: Option<&str>) -> String {
    let mut s = signatures(tools);
    s.push_str(
        "Rules for this app: after you write a call, STOP - it is executed for you and its \
         result arrives in a <tool_response> block in the next turn; never invent one. Call \
         ONLY the functions listed above, by their exact names - nothing else exists. One \
         call at a time. When a result reports an error, say so plainly and answer from what \
         you have. When you have enough to answer, stop calling and write the answer.",
    );
    push_forced(&mut s, require);
    s
}

/// OpenAI's forcing `tool_choice`, as words. Shared by every block that can
/// carry client entries, because only those can be forced.
fn push_forced(s: &mut String, require: Option<&str>) {
    match require {
        Some("") => s.push_str(
            "\n\nFor THIS turn you MUST respond with a tool call, not a prose answer.",
        ),
        Some(name) => s.push_str(&format!(
            "\n\nFor THIS turn you MUST call `{name}`, not answer in prose.",
        )),
        None => {}
    }
}

/// The deployment's own tools offered ALONGSIDE a client's declared ones.
///
/// The client's entries come FIRST and win every name collision, which is what
/// keeps the invariant `ChatReq::client_tools` has always stated: a
/// client-supplied name must never select a server-executed capability that
/// merely shares it. Dropping the server's twin is the only resolution that
/// holds it, since renaming either side would hand the model two entries it
/// has no way to tell apart.
pub fn merge_registries(server: &[Tool], client: &[Tool]) -> Vec<Tool> {
    let mut out: Vec<Tool> = client.to_vec();
    out.extend(
        server
            .iter()
            .filter(|s| !client.iter().any(|c| c.name == s.name))
            .cloned(),
    );
    out
}

/// The block when BOTH lists are live: the client's declared tools and this
/// deployment's own, rendered as one list.
///
/// The model is told nothing about which entry is whose, deliberately. From
/// where it sits every call is the same act - write it, stop, the result comes
/// back in a `<tool_response>` - and which side executes it is this app's
/// business, not something a model should be reasoning about mid-answer. The
/// budget is the one place the split leaks, because it bounds only the server's
/// half; the client owns its own loop and its own limit.
pub fn merged_system_block(tools: &[Tool], b: Budget, require: Option<&str>) -> String {
    let mut s = signatures(tools);
    s.push_str(&format!(
        "Rules for this app: after you write a call, STOP - it is executed for you and its \
         result comes back in a <tool_response> block; never invent one. Call ONLY the \
         functions listed above, by their exact names - nothing else exists, and a call to \
         anything else is shown to the user as a failure. One call at a time, and at most \
         {} of them are run by this server in a single answer, within {} of wall-clock time. \
         When a call fails, say so plainly and answer from what you have.",
        b.max_calls,
        b.time(),
    ));
    s.push_str(&finish_rule(tools, b));
    push_forced(&mut s, require);
    s
}

/// The part both modes share: the signature list, in the format the model was
/// trained on.
fn signatures(tools: &[Tool]) -> String {
    let mut s = String::from(
        "\n\n# Tools\n\nYou may call one or more functions to assist with the user query.\n\n\
         You are provided with function signatures within <tools></tools> XML tags:\n<tools>\n",
    );
    for t in tools {
        let sig = serde_json::json!({
            "type": "function",
            "function": {
                "name": t.name,
                "description": t.description,
                "parameters": t.parameters,
            }
        });
        s.push_str(&sig.to_string());
        s.push('\n');
    }
    s.push_str(
        "</tools>\n\nFor each function call, return a json object with function name and \
         arguments within <tool_call></tool_call> XML tags:\n<tool_call>\n\
         {\"name\": <function-name>, \"arguments\": <args-json-object>}\n</tool_call>\n\n",
    );
    s
}

/// Tool calling is a trained format, not a prompt trick: only the templates
/// whose models were taught this convention get the block. Everything else
/// keeps the pre-pass router, which needs nothing from the model but a line of
/// text.
pub fn template_supported(template: &str) -> bool {
    template == "chatml"
}

/// How a tool result is rendered back into the conversation. Qwen's own
/// template puts tool output in a USER turn wrapped in `<tool_response>`, so
/// this needs no new role and no change to render_template.
pub fn response_turn(name: &str, result: &str) -> String {
    format!("<tool_response>\n{{\"name\": \"{name}\", \"content\": {}}}\n</tool_response>",
            serde_json::Value::String(result.to_string()))
}

// -------------------------------------------------------------- the parser --

#[derive(Debug, PartialEq)]
pub struct ToolCall {
    pub name: String,
    pub args: serde_json::Value,
}

/// The values a model writes when it emits the SHAPE of a call without having
/// decided its content: schema type names, the word placeholder itself, the
/// filler an example would use. Matched whole, never as a substring, because
/// every one of these is also a legitimate thing to search for.
const STUB_VALUES: &[&str] = &[
    "placeholder",
    "placeholder prompt",
    "placeholder text",
    "placeholder query",
    "placeholder description",
    "your prompt here",
    "your query here",
    "your text here",
    "your description here",
    "prompt here",
    "query here",
    "text here",
    "description here",
    "prompt goes here",
    "query goes here",
    "insert prompt here",
    "insert query here",
    "example prompt",
    "example query",
    "sample prompt",
    "sample text",
    "some text",
    "string",
    "todo",
    "tbd",
    "fixme",
    "...",
    "…",
];

/// An argument the model never actually filled in: `(name, value)`.
///
/// This is what a call looks like when the reasoning that was going to author
/// the argument never happened - most sharply after the think budget force-
/// closes a block mid-plan, which drops the model into answer position with a
/// decision half made. Observed live 2026-08-16 on a "write me a self-contained
/// HTML file" turn: the budget ran out mid-sentence, the model's very next act
/// was `generate_image {"prompt": "placeholder"}`, and the user got thirty
/// seconds of GPU and a picture of nothing they asked for. The model's own next
/// block read "I accidentally called generate_image with a placeholder prompt -
/// that was a mistake", which is the tell that nothing about the call was meant.
///
/// Deliberately narrow. It matches a whole trimmed value against a closed list,
/// or a value that is nothing but a bracketed slot (`<prompt>`, `[your query]`)
/// - never a substring, because "what does placeholder mean" is a real question
/// and a real query. The caller is expected to ASK rather than refuse (see
/// ToolLoop::step): a model that meant the literal string sends it again.
pub fn stub_arg(args: &serde_json::Value) -> Option<(String, String)> {
    let obj = args.as_object()?;
    for (k, v) in obj {
        let Some(s) = v.as_str() else { continue };
        if is_stub_value(s) {
            return Some((k.clone(), s.trim().to_string()));
        }
    }
    None
}

fn is_stub_value(s: &str) -> bool {
    let s = s.trim().trim_matches(['"', '\'', '`']).trim();
    if s.is_empty() {
        return false;
    }
    // A bracketed slot is a stub whatever is written inside it, but only when
    // the inside is a short run of words: `{"a": 1}` is a request body and
    // `<html>...</html>` is a document, and neither is a slot.
    let bracketed = [('<', '>'), ('[', ']'), ('{', '}')].iter().any(|&(o, c)| {
        s.starts_with(o)
            && s.ends_with(c)
            && s.len() > 2
            && {
                let inner = &s[1..s.len() - 1];
                inner.len() <= 48
                    && !inner.contains([o, c, '"', ':', '\n', '/'])
                    && inner.chars().any(char::is_alphabetic)
            }
    });
    if bracketed {
        return true;
    }
    // whole-value match, case- and punctuation-insensitive: models write
    // "Placeholder." and "TODO" for the same non-decision. The trailing
    // punctuation comes off SECOND, never first: an ellipsis is a stub in its
    // own right and stripping it leaves nothing to match.
    let norm: String = s.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
    if STUB_VALUES.contains(&norm.as_str()) {
        return true;
    }
    let trimmed = norm.trim_end_matches(['.', ':', '!']);
    !trimmed.is_empty() && STUB_VALUES.contains(&trimmed)
}

/// Pull tool calls out of a reply.
///
/// Tolerant on purpose, because the failure mode is expensive: a call this
/// misses is shown to the user as raw JSON instead of being run. Accepts the
/// trained `<tool_call>{...}</tool_call>` form (including one left unterminated
/// by the stop string), a ```json fence, and a bare object that is nothing but
/// `{"name": ..., "arguments": ...}`. Reasoning inside a <think> block is
/// skipped: a model that talks through calling a tool has not called it.
pub fn parse_calls(text: &str) -> Vec<ToolCall> {
    parse_calls_for(text, &[])
}

/// As `parse_calls`, but with the caller's declared tools on hand so a call
/// that names no function at all can still be identified. Use this on the /v1
/// passthrough; the builtin path has no registry to match against.
pub fn parse_calls_for(text: &str, tools: &[Tool]) -> Vec<ToolCall> {
    let body = after_think(text);
    let mut out = Vec::new();
    // Scan the WHOLE reply, skipping only the calls that sit inside reasoning.
    //
    // `body` (everything past the LAST </think>) is not enough on its own: a
    // model that never writes </tool_call> leaves the stop string unfired, so
    // one reply can carry a complete call, THEN another <think> block, then a
    // second call the token cap cut in half. Anchoring on the last </think>
    // sees only that truncated tail and throws the good call away - observed
    // live 2026-08-14 from qwen3.8 through opencode, where a valid `todowrite`
    // was discarded and the reply was delivered as raw JSON prose.
    for chunk in call_chunks(text) {
        if let Some(c) = one_call(chunk, tools) {
            out.push(c);
        }
    }
    if !out.is_empty() {
        return out;
    }
    // no tagged call: accept a fenced or bare object, but ONLY when it is
    // essentially the whole reply. An answer that QUOTES a call - "you could
    // send {"name": "x", ...}" - is an answer, not a call.
    let t = body.trim();
    let inner = strip_fence(t).unwrap_or(t);
    if inner.starts_with('{') && inner.ends_with('}') {
        if let Some(c) = one_call(inner, tools) {
            return vec![c];
        }
    }
    Vec::new()
}

/// A call that names no function ANYWHERE - `{"arguments": {"content": ...,
/// "filePath": ...}}` and nothing else - observed from the fable 27b writing a
/// file through an agent. The argument keys still identify it whenever exactly
/// one declared tool can accept them: every required key present, and no key
/// the schema does not declare. Ambiguity returns None, because running the
/// wrong tool is worse than showing the block.
fn infer_name(args: &serde_json::Value, tools: &[Tool]) -> Option<String> {
    let obj = args.as_object()?;
    if obj.is_empty() {
        return None;
    }
    let mut fits = tools.iter().filter(|t| {
        let Some(props) = t.parameters.get("properties").and_then(|v| v.as_object()) else {
            return false;
        };
        let required = t.parameters.get("required").and_then(|v| v.as_array());
        obj.keys().all(|k| props.contains_key(k))
            && required.is_none_or(|a| {
                a.iter().filter_map(|v| v.as_str()).all(|r| obj.contains_key(r))
            })
    });
    let first = fits.next()?;
    if fits.next().is_some() {
        return None; // more than one tool fits: do not guess
    }
    Some(first.name.clone())
}

/// The `<tool_call>` bodies in a reply, in order, skipping any that sit INSIDE
/// a reasoning block.
///
/// Think-depth is counted rather than assumed: a call is reasoning if more
/// `<think>` tags opened before it than closed. That is the rule the old
/// `after_think` anchor was approximating, and the approximation broke the
/// moment a model interleaved a real call with more thinking - which qwen3.8
/// does whenever it omits `</tool_call>`, because nothing then stops it.
///
/// A body runs to its `</tool_call>`, or to the next `<think>` when the model
/// never closed the tag, or to the end of the reply. Stopping at `<think>`
/// matters: without it a complete call would swallow the rest of the reply and
/// fail to parse as one object.
fn call_chunks(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    let mut depth = 0i32;
    while pos < text.len() {
        let rest = &text[pos..];
        let open = rest.find("<think>");
        let close = rest.find("</think>");
        let call = rest.find("<tool_call>");
        // whichever marker comes first decides what happens next
        let next = [open, close, call].into_iter().flatten().min();
        let Some(n) = next else { break };
        if Some(n) == open && depth >= 0 {
            depth += 1;
            pos += n + "<think>".len();
        } else if Some(n) == close {
            depth -= 1;
            pos += n + "</think>".len();
        } else {
            let after = pos + n + "<tool_call>".len();
            let tail = &text[after..];
            // A body ends at the first thing that cannot be part of it. All
            // four matter, because this model omits `</tool_call>` and then
            // writes whatever it likes next: a stray `</think>` with no opener,
            // or simply the NEXT call. Ending only at `</tool_call>`/`<think>`
            // let one truncated body swallow the rest of the reply - including
            // the good call after it (measured live 2026-08-14, run 6).
            let end = ["</tool_call>", "<think>", "</think>", "<tool_call>"]
                .iter()
                .filter_map(|m| tail.find(m))
                .min()
                .unwrap_or(tail.len());
            if depth <= 0 {
                out.push(&tail[..end]);
            }
            pos = after + end;
        }
    }
    out
}

/// Everything after a closed `<think>` block, or the whole text when there is
/// no block (or it never closed - a reply still inside its reasoning has not
/// called anything).
fn after_think(text: &str) -> &str {
    match text.rfind("</think>") {
        Some(i) => &text[i + "</think>".len()..],
        None => text,
    }
}

fn strip_fence(t: &str) -> Option<&str> {
    let rest = t.strip_prefix("```")?;
    let rest = rest.strip_prefix("json").unwrap_or(rest);
    let rest = rest.trim_start_matches(['\r', '\n']);
    Some(rest.trim_end().strip_suffix("```").unwrap_or(rest).trim())
}

/// One `{"name": ..., "arguments": {...}}`, from a chunk that may carry
/// whitespace or a fence around it. Arguments that arrived as a JSON STRING
/// (a common near-miss) are re-parsed rather than rejected, and so is a name
/// tucked INSIDE the arguments object - `{"arguments": {"url": ..., "name":
/// "fetch_url"}}` - which then poisons the conversation: the raw call is
/// delivered as assistant text and every retry copies it verbatim. A call that
/// names nothing at all falls back to `infer_name` against the caller's own
/// tools, which is the only evidence left. A wrapper that is simply junk -
/// anything at all before the object - is stepped over rather than refused.
fn one_call(chunk: &str, tools: &[Tool]) -> Option<ToolCall> {
    let t = chunk.trim();
    let t = strip_fence(t).unwrap_or(t);
    if let Some(c) = function_tag_call(t) {
        return Some(c);
    }
    let v: serde_json::Value = serde_json::from_str(t)
        .ok()
        .or_else(|| {
            // a trailing sentence after the object is common; take the balanced
            // prefix that parses
            let end = balanced_end(t)?;
            serde_json::from_str(&t[..end]).ok()
        })
        .or_else(|| {
            // ...and a call the generation budget cut off mid-object, which is
            // the NORMAL ending for a model that never writes </tool_call>:
            // nothing stops it, so the reply runs to max_new and the last thing
            // in it is a call missing its closing braces. Measured live
            // 2026-08-14: a `write` carrying a COMPLETE 29 KB index.html, one
            // `}` short, thrown away in full.
            let repaired = close_truncated(t)?;
            serde_json::from_str(&repaired).ok()
        })
        .or_else(|| {
            // ...and a call whose WRAPPER is junk but whose object is intact:
            // `<function": {"name": ..., "arguments": {...}}` - the functionary
            // tag spelling welded onto the trained form, minus the `{"` that
            // would have opened it. Observed live 2026-09-03 from qwen3.8 on
            // the 27b. Every path above needs the chunk to START with `{`, so
            // one stray leading character threw away a complete, valid call,
            // and `attempted_call` missed it for the same reason - the block
            // reached the user raw instead of being rewritten. Take the first
            // balanced object in the chunk, closing it when the budget cut it
            // off. Junk before the object cannot change what the object says.
            let rest = &t[t.find('{')?..];
            balanced_end(rest)
                .and_then(|end| serde_json::from_str(&rest[..end]).ok())
                .or_else(|| close_truncated(rest).and_then(|r| serde_json::from_str(&r).ok()))
        })?;
    // OpenAI's OWN nesting, `{"function": {"name": ..., "arguments": ...}}`,
    // carries both halves, so unwrap it before reading either.
    let v = match v.get("function") {
        Some(f) if f.is_object() => f.clone(),
        _ => v,
    };
    let mut args = match v.get("arguments").or_else(|| v.get("parameters")) {
        // double-encoded arguments, which this family emits too: the object
        // arrives as a STRING of JSON. Truncation hits that inner document the
        // same way it hits the outer one, so it gets the same repair.
        Some(serde_json::Value::String(s)) => serde_json::from_str(s)
            .ok()
            .or_else(|| close_truncated(s).and_then(|r| serde_json::from_str(&r).ok()))
            .unwrap_or_else(|| serde_json::Value::String(s.clone())),
        Some(a) => a.clone(),
        None => serde_json::json!({}),
    };
    let named = |v: Option<&serde_json::Value>| {
        v.and_then(|n| n.as_str()).map(str::trim).filter(|n| !n.is_empty()).map(str::to_string)
    };
    // `name` is the trained key; `function` is OpenAI's, and qwen3.x reaches
    // for it often enough that leaving it out was a real failure class. Those
    // calls only ran when infer_name happened to pick them out of their
    // argument keys, which is luck: two tools sharing a key shape make it
    // ambiguous and the whole call - a written FILE, in the case that found
    // this - is shown to the user as raw JSON instead. Read the name the model
    // actually wrote before trying to deduce one.
    let name = match named(v.get("name")).or_else(|| named(v.get("function"))) {
        Some(n) => n,
        None => {
            // no top-level name: pull it OUT of the arguments, where a `name`
            // key can only be the function name the model misplaced
            match named(args.as_object().and_then(|o| o.get("name"))) {
                Some(n) => {
                    if let Some(o) = args.as_object_mut() {
                        o.remove("name");
                    }
                    n
                }
                // named nowhere at all: let the argument keys identify it
                None => infer_name(&args, tools)?,
            }
        }
    };
    if name.is_empty() {
        return None;
    }
    Some(ToolCall { name, args })
}

/// The function name out of a `<function...>` tag's interior, in every
/// spelling this family writes it: `=read`, `name = "web_search"`, `="x"`,
/// `"x"`. None when the interior does not look like a tag's at all, which
/// keeps a word that merely starts with "function" (`<functions>`) from
/// becoming a call and leaves the object paths their turn.
fn tag_name(part: &str) -> Option<&str> {
    // an introducer is what proves this was a tag rather than a longer word
    if !part.starts_with(['=', ' ', '\t', '"', '\'']) && !part.starts_with("name") {
        return None;
    }
    let p = part.trim();
    // `<functionname = "x">`: the attribute spelling. Strip `name` only when
    // what follows introduces a VALUE, or a tool really called `named_thing`
    // would lose its first four characters.
    let p = match p.strip_prefix("name") {
        Some(r) if r.trim_start().starts_with(['=', '"', '\'']) => r.trim_start(),
        _ => p,
    };
    let p = p.strip_prefix('=').unwrap_or(p).trim();
    let p = p.trim_matches(['"', '\'']).trim();
    // a name is an identifier; anything else means this was not a tag
    (!p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')))
        .then_some(p)
}

/// The `<function...>` opener family: the functionary/Llama tool syntax, which
/// this family reaches for perhaps one turn in three even when the prompt only
/// ever showed it the Hermes form.
///
/// Neither the tag NOR what follows it is reliably the documented
/// `<function=name>{args}`. Live on 2026-08-14 qwen3.8 wrote `<function=read>,
/// "arguments": {"filePath": ...}}`, and on 2026-09-03 `<functionname =
/// "web_search"> "arguments": {"query": ...}}` - the tag welded onto a fragment
/// of the trained form, with the call never wrapped in an object at all. That
/// second shape is why the tag is read here rather than left to `one_call`'s
/// object paths: the only balanced object in it is the ARGUMENTS value, so
/// recovering an object recovers a call with no name and no arguments.
///
/// Taking the name from the tag in whatever spelling it wears, and the
/// arguments from the first balanced object after it, reads all of them; a
/// call with no object at all is still a call with no arguments rather than a
/// dead block shown to the user as prose.
fn function_tag_call(t: &str) -> Option<ToolCall> {
    let rest = t.strip_prefix("<function")?;
    let (name, after) = rest.split_once('>')?;
    let Some(name) = tag_name(name) else {
        return None;
    };
    let args = after
        .find('{')
        .map(|i| &after[i..])
        .and_then(|s| Some(&s[..balanced_end(s)?]))
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    Some(ToolCall { name: name.to_string(), args })
}

/// Close a JSON object the token budget cut off, or None when nothing is open.
///
/// Only ever APPENDS the closers the text is already short of, so it cannot
/// change the meaning of what the model actually wrote: a truncated call comes
/// back as the call it was going to be, and a merely malformed one still fails.
/// The arguments a truncation drops are the LAST ones, which is why this is
/// worth doing at all - the payload (a file's contents, typically) is complete
/// long before the wrapper is.
fn close_truncated(s: &str) -> Option<String> {
    if !s.trim_start().starts_with('{') {
        return None;
    }
    let mut stack: Vec<char> = Vec::new();
    let mut in_str = false;
    let mut esc = false;
    for c in s.chars() {
        if in_str {
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' => {
                stack.pop();
            }
            _ => {}
        }
    }
    if stack.is_empty() && !in_str {
        return None; // nothing was open: this is malformed, not truncated
    }
    let mut out = String::with_capacity(s.len() + stack.len() + 2);
    out.push_str(s);
    if esc {
        // a dangling backslash would escape the quote we are about to add
        out.push('n');
    }
    if in_str {
        out.push('"');
    }
    while let Some(c) = stack.pop() {
        out.push(c);
    }
    Some(out)
}

/// Index just past the first balanced `{...}`, string-aware.
fn balanced_end(t: &str) -> Option<usize> {
    let b = t.as_bytes();
    if b.first() != Some(&b'{') {
        return None;
    }
    let (mut depth, mut in_str, mut esc) = (0i32, false, false);
    for (i, &c) in b.iter().enumerate() {
        if in_str {
            match c {
                _ if esc => esc = false,
                b'\\' => esc = true,
                b'"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

// ------------------------------------------------------------- the calling --

pub struct ToolResult {
    pub text: String,
    pub is_error: bool,
    pub ms: u64,
    /// (title, url) for a web_search call. The playground already renders a
    /// numbered source list under a reply; a search the MODEL asked for
    /// deserves the same one, or its [1] and [2] point at nothing.
    pub sources: Vec<(String, String)>,
    /// the picture a generate_image call produced. The model only reads the
    /// text beside it; the bytes ride out to the client through the leg's
    /// existing image delivery.
    pub image: Option<crate::image::GeneratedImage>,
}

/// Run one call. Never returns Err: a failure IS a result, handed back to the
/// model so it can try something else or tell the user plainly. A tool that is
/// down should cost an answer its accuracy, not its existence.
///
/// `images` is the current conversation's attached pictures (for view_image)
/// and `on_status` keeps the client stream warm through the slow builtins - an
/// image generation queued behind other tenants is minutes, and every
/// server-side wait on a response path must tick.
pub fn call(
    reg: &mut Registry,
    cfg: &ToolsConfig,
    b: Builtins,
    name: &str,
    args: &serde_json::Value,
    images: &[Vec<u8>],
    now_ms: impl Fn() -> u64,
    on_status: &dyn Fn(&str),
) -> ToolResult {
    let t0 = now_ms();
    let src = match reg.find(name) {
        Some(t) => t.src.clone(),
        None => {
            // A CONFIGURED image-taking entry that was gated off this turn
            // deserves the real reason: "no tool named X" reads as a
            // deployment gap, and the model then tells the user the
            // capability does not exist (seen live: an upscale ask on a
            // turn whose history carried no picture).
            let text = if cfg.http.iter().any(|t| t.name == name && t.wants_images()) {
                format!(
                    "the {name} tool exists on this deployment but takes a picture, and this \
                     turn's conversation carries none - it is only offered when one is \
                     present. Ask the user to attach (or re-send) the image they mean; do \
                     not retry without a picture."
                )
            } else {
                let known: Vec<&str> = reg.tools.iter().map(|t| t.name.as_str()).collect();
                format!(
                    "there is no tool named '{name}' on this deployment. Available: {}",
                    if known.is_empty() { "(none)".into() } else { known.join(", ") }
                )
            };
            return ToolResult {
                text,
                is_error: true,
                ms: now_ms().saturating_sub(t0),
                sources: Vec::new(),
                image: None,
            };
        }
    };
    let max_chars = match &src {
        ToolSrc::Http(i) => cfg.http[*i].max_chars.unwrap_or(cfg.max_chars),
        _ => cfg.max_chars,
    };
    let mut sources = Vec::new();
    let mut image = None;
    let r = match src {
        ToolSrc::Builtin(k) => call_builtin(k, &b, args, &mut sources, on_status),
        ToolSrc::Http(i) => {
            call_http(&cfg.http[i], cfg, args, images, &mut sources, &mut image, on_status, b.user)
        }
        ToolSrc::Mcp { server, remote } => call_mcp(&mut reg.mcp[server], &remote, args),
        // never built into a Registry - the passthrough renders client tools
        // into the prompt and hands the call back, so reaching this arm is a
        // wiring bug, and the failure must say which side executes
        ToolSrc::Client => Err("client-declared tools are executed by the client, not here".into()),
    };
    finish_call(r, max_chars, sources, image, t0, now_ms)
}

/// Run ONE http entry outside a registry: the routed pre-pass path, which
/// already knows exactly which tool it wants and must not pay MCP discovery
/// for the privilege.
pub fn call_http_entry(
    cfg: &ToolsConfig,
    i: usize,
    args: &serde_json::Value,
    images: &[Vec<u8>],
    now_ms: impl Fn() -> u64,
    on_status: &dyn Fn(&str),
) -> ToolResult {
    let t0 = now_ms();
    let t = &cfg.http[i];
    let mut sources = Vec::new();
    let mut image = None;
    // the routed pre-pass carries no caller: an entry that asks for $user
    // fails closed there, which is the right answer for a route with no one
    // behind it
    let r = call_http(t, cfg, args, images, &mut sources, &mut image, on_status, None);
    finish_call(r, t.max_chars.unwrap_or(cfg.max_chars), sources, image, t0, now_ms)
}

/// The shared tail of a call: truncation, error hygiene, the clock.
fn finish_call(
    r: Result<String, String>,
    max_chars: usize,
    mut sources: Vec<(String, String)>,
    mut image: Option<crate::image::GeneratedImage>,
    t0: u64,
    now_ms: impl Fn() -> u64,
) -> ToolResult {
    let (text, is_error) = match r {
        Ok(t) => (truncate(&t, max_chars), false),
        Err(e) => (e, true),
    };
    if is_error {
        sources.clear();
        image = None;
    }
    if let Some(img) = &mut image {
        img.ms = now_ms().saturating_sub(t0);
    }
    ToolResult { text, is_error, ms: now_ms().saturating_sub(t0), sources, image }
}

/// The app's own capabilities, called the way any other tool is.
///
/// web_search renders EXACTLY what the pre-pass renders (search::render_context),
/// so the numbering the model cites is the numbering it has always cited and
/// the answer path needs no second convention.
fn call_builtin(
    k: Builtin,
    b: &Builtins,
    args: &serde_json::Value,
    sources: &mut Vec<(String, String)>,
    on_status: &dyn Fn(&str),
) -> Result<String, String> {
    match k {
        // reached only by the /tools probe: the answer loop intercepts the
        // name before dispatch (ToolLoop::step), because a child is a whole
        // loop of generations and only a leg can generate
        Builtin::Agent => Err(
            "spawn_agent is run by the answer loop, not by a probe: it starts a whole \
             subagent loop, which only a chat turn can host"
                .into(),
        ),
        Builtin::Wait => {
            let (secs, reason, note) = wait_plan(args, b.wait_cap_s)?;
            // Sleep in ticks, each one a status line: the guest cannot say
            // anything while parked, so the tick IS the keepalive, and the
            // countdown is what the user sees in place of a frozen screen.
            let mut left = secs;
            while left > 0 {
                // the stop button, seen from here: a tick that could not be
                // written means nobody is waiting, so neither does this
                if crate::client_gone() {
                    return Err(format!(
                        "the client disconnected {} into the wait",
                        human_secs(secs - left)
                    ));
                }
                on_status(&format!(
                    "waiting {}{} · {}s left",
                    human_secs(secs),
                    if reason.is_empty() { String::new() } else { format!(": {reason}") },
                    left
                ));
                let slice = left.min(WAIT_TICK_S);
                crate::sleep_ms(slice * 1000);
                left -= slice;
            }
            let budget_left = b.turn_left_s.saturating_sub(secs);
            Ok(format!(
                "Waited {}{}.{} This answer has about {} of its time budget left.",
                human_secs(secs),
                if reason.is_empty() { String::new() } else { format!(" ({reason})") },
                note,
                human_secs(budget_left),
            ))
        }
        Builtin::WebSearch => {
            let scfg = b.search.ok_or("web search is not configured on this deployment")?;
            let q = args
                .get("query")
                .and_then(|q| q.as_str())
                .map(str::trim)
                .filter(|q| !q.is_empty())
                .ok_or("web_search needs a non-empty `query` string")?;
            let hits = crate::search::search(scfg, q)?;
            if hits.is_empty() {
                // NOT an error: "nothing found" is a real answer, and the model
                // should say so rather than call again with the same words
                return Ok(format!("No web results for '{q}'."));
            }
            *sources = hits.iter().map(|h| (h.title.clone(), h.url.clone())).collect();
            Ok(crate::search::render_context(q, &hits))
        }
        Builtin::Request => {
            let scfg = b.search.ok_or("outbound requests are not configured on this deployment")?;
            let u = args
                .get("url")
                .and_then(|u| u.as_str())
                .map(str::trim)
                .filter(|u| !u.is_empty())
                .ok_or("request needs a non-empty `url` string")?;
            if !u.starts_with("http://") && !u.starts_with("https://") {
                return Err(format!("'{u}' is not an absolute http(s) URL"));
            }
            let method = args
                .get("method")
                .and_then(|m| m.as_str())
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .unwrap_or("GET")
                .to_ascii_uppercase();
            let text = match method.as_str() {
                "GET" => crate::search::fetch_page(scfg, u)?,
                "POST" | "PUT" | "PATCH" | "DELETE" => {
                    let m = match method.as_str() {
                        "POST" => Method::Post,
                        "PUT" => Method::Put,
                        "PATCH" => Method::Patch,
                        _ => Method::Delete,
                    };
                    // a write needs a body; DELETE conventionally goes without
                    let (body, ctype) = request_payload(args, method != "DELETE")?;
                    crate::search::send_request(
                        scfg,
                        m,
                        u,
                        body.as_deref().map(str::as_bytes),
                        &ctype,
                    )?
                }
                other => {
                    return Err(format!(
                        "unsupported method '{other}' - use GET, POST, PUT, PATCH or DELETE"
                    ))
                }
            };
            // the target joins the source list for the same reason a fetched
            // page does, plus one of its own on a write: the user gets told
            // where their data went without having to trust the prose
            sources.push((u.to_string(), u.to_string()));
            Ok(text)
        }
    }
}

/// The body and content type a write-method request call actually sends. A
/// JSON object or array is serialized and sent as JSON; a string goes as-is,
/// defaulting to application/json because that is what the APIs a model
/// reaches for speak. `content_type` overrides the default either way.
fn request_payload(
    args: &serde_json::Value,
    required: bool,
) -> Result<(Option<String>, String), String> {
    let body = match args.get("body") {
        None | Some(serde_json::Value::Null) if required => {
            return Err(
                "this method needs a `body` (a JSON object, or a string sent as-is)".into()
            )
        }
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(v) => Some(v.to_string()),
    };
    let ctype = args
        .get("content_type")
        .and_then(|c| c.as_str())
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .unwrap_or("application/json")
        .to_string();
    Ok((body, ctype))
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}\n[truncated at {max} characters]")
}

fn call_http(
    t: &HttpTool,
    cfg: &ToolsConfig,
    args: &serde_json::Value,
    images: &[Vec<u8>],
    sources: &mut Vec<(String, String)>,
    image_out: &mut Option<crate::image::GeneratedImage>,
    on_status: &dyn Fn(&str),
    user: Option<&str>,
) -> Result<String, String> {
    let empty = serde_json::Map::new();
    let obj = args.as_object().unwrap_or(&empty);
    let method = match t.method.as_deref().unwrap_or("GET").to_ascii_uppercase().as_str() {
        "GET" => Method::Get,
        "POST" => Method::Post,
        "PUT" => Method::Put,
        "PATCH" => Method::Patch,
        "DELETE" => Method::Delete,
        other => return Err(format!("tool '{}' has an unsupported method '{other}'", t.name)),
    };
    let is_get = matches!(method, Method::Get | Method::Delete);
    // {arg} in the URL is consumed by the path; whatever is left goes to the
    // query string (GET) or the body (POST)
    let (mut url, used) = substitute(&t.url, obj);
    let leftover: Vec<(&String, &serde_json::Value)> =
        obj.iter().filter(|(k, _)| !used.contains(&k.as_str())).collect();
    let as_query = t.query.unwrap_or(is_get);
    if as_query && !leftover.is_empty() {
        let mut q = String::new();
        for (k, v) in &leftover {
            q.push(if q.is_empty() { '?' } else { '&' });
            if url.contains('?') && q.len() == 1 {
                q.pop();
                q.push('&');
            }
            q.push_str(&pct(k));
            q.push('=');
            q.push_str(&pct(&scalar(v)));
        }
        url.push_str(&q);
    }
    let body: Option<Vec<u8>> = if is_get {
        None
    } else {
        let payload = match &t.body {
            // the turn's pictures ride the template as $images / $image -
            // bytes the model could never put in its arguments itself.
            // Pruning after the fill is what makes an OPTIONAL parameter
            // expressible in a template at all: a hole the model left
            // unfilled is dropped rather than sent as the literal string
            // "$factor" for the endpoint to choke on.
            Some(tpl) => prune_unfilled(fill_template(tpl, &with_images(obj, images)), t),
            None if as_query => serde_json::json!({}),
            None => args.clone(),
        };
        Some(payload.to_string().into_bytes())
    };

    let mut req = HttpReq::get(&url);
    req.method = method;
    req.timeout_s = t.timeout_s.unwrap_or(cfg.timeout_s);
    req.max_bytes = req_max(t, cfg);
    req.body = body.as_deref();
    req = req.header("accept", b"application/json, text/plain;q=0.9, */*;q=0.8");
    if body.is_some() {
        req = req.header("content-type", b"application/json");
    }
    let mut notes = Vec::new();
    let headers = identity_headers(&t.headers, user, &t.name)?;
    for (k, v) in resolved_headers(&headers, &mut notes, &t.url) {
        req = req.header(&k, v.as_bytes());
    }
    if let Some(n) = notes.first() {
        return Err(n.clone());
    }

    // ticked, because a tool is allowed to be slow (an image generation
    // queued behind other tenants is minutes) and the client stream must see
    // SOMETHING inside every idle-timeout window between here and the browser
    let name = t.name.clone();
    let r = http::request_with_tick(req, 15, &mut |secs| {
        on_status(&format!("waiting on {name}… {secs}s"));
        // the stop button, mid-request: the wait ends and nothing is read
        !crate::client_gone()
    })?;
    let text = String::from_utf8_lossy(&r.body).trim().to_string();
    if r.status >= 400 {
        let hint: String = text.chars().take(400).collect();
        return Err(format!("tool '{}' returned HTTP {}: {hint}", t.name, r.status));
    }
    if r.truncated {
        return Ok(format!("{text}\n[response was cut off at {} bytes]", req_max(t, cfg)));
    }
    extract_sources(t, &text, sources);
    map_result(t, text, args, image_out)
}

/// The hits a sources map names, as (title, url) rows for the citation list.
/// Tolerant like everything on this path: a path that misses simply yields
/// no sources, never an error.
fn extract_sources(t: &HttpTool, text: &str, sources: &mut Vec<(String, String)>) {
    let Some(sm) = &t.sources else { return };
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text) else { return };
    let Some(arr) = json_path(&parsed, &sm.list).and_then(|v| v.as_array()) else { return };
    for hit in arr {
        let field = |p: &Option<String>| -> Option<String> {
            p.as_deref()
                .and_then(|p| json_path(hit, p))
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };
        let title = field(&sm.title);
        let url = field(&sm.url);
        if title.is_none() && url.is_none() {
            continue;
        }
        let u = url.clone().unwrap_or_default();
        sources.push((title.or(url).unwrap_or_default(), u));
    }
}

/// A response cap that fits what the tool RETURNS: a picture arrives as
/// megabytes of base64 inside JSON, so an image-producing tool that sets no
/// cap of its own gets an image-sized default instead of the text one.
fn req_max(t: &HttpTool, cfg: &ToolsConfig) -> usize {
    match t.max_bytes {
        Some(n) => n,
        None if t.makes_image() => (12 * 1024 * 1024).max(cfg.max_bytes),
        None => cfg.max_bytes,
    }
}

/// The substitution map for a BODY template: the model's arguments plus the
/// turn's pictures under the reserved names `images` (array of data URIs) and
/// `image` (the first one). Reserved means reserved: an argument that happens
/// to share the name is shadowed, because bytes the model cannot produce must
/// win over text it can.
fn with_images(
    obj: &serde_json::Map<String, serde_json::Value>,
    images: &[Vec<u8>],
) -> serde_json::Map<String, serde_json::Value> {
    let mut out = obj.clone();
    if !images.is_empty() {
        let uris: Vec<serde_json::Value> = images
            .iter()
            .map(|b| serde_json::Value::String(crate::vision::to_data_uri(b)))
            .collect();
        out.insert("image".into(), uris[0].clone());
        out.insert("images".into(), serde_json::Value::Array(uris));
    }
    out
}

/// The response, shaped by the tool's ResultMap: an extracted picture goes to
/// the client and the model gets a note; an extracted text field spares the
/// model the envelope; no map (or a text path that misses) hands over the
/// body as-is.
fn map_result(
    t: &HttpTool,
    text: String,
    args: &serde_json::Value,
    image_out: &mut Option<crate::image::GeneratedImage>,
) -> Result<String, String> {
    let Some(rm) = &t.result else { return Ok(text) };
    let parsed: Option<serde_json::Value> = serde_json::from_str(&text).ok();
    if let Some(path) = &rm.image {
        let raw = parsed
            .as_ref()
            .and_then(|j| json_path(j, path))
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                let hint: String = text.chars().take(200).collect();
                format!(
                    "tool '{}' answered without an image at result.image path '{path}': {hint}",
                    t.name
                )
            })?;
        let (mime, b64) = image_payload(raw);
        let prompt = args
            .get("prompt")
            .and_then(|p| p.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| {
                // no prompt argument (an upscale-style tool): echo the args
                // minus any image payloads - kilobytes of base64 are not a
                // "request" the model should read back
                match args {
                    serde_json::Value::Object(o) => {
                        let slim: serde_json::Map<String, serde_json::Value> = o
                            .iter()
                            .filter(|(k, _)| k.as_str() != "image" && k.as_str() != "images")
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect();
                        truncate(&serde_json::Value::Object(slim).to_string(), 200)
                    }
                    other => truncate(&other.to_string(), 200),
                }
            });
        *image_out = Some(crate::image::GeneratedImage {
            b64,
            mime,
            prompt: prompt.clone(),
            model: None,
            seed: None,
            ms: 0, // stamped by call(), which owns the clock
        });
        return Ok(format!(
            "The call succeeded: an image has been generated and is displayed to the user \
             directly above your reply (request: \"{prompt}\"). You cannot see it. \
             Acknowledge it briefly and naturally, and offer to adjust it; do not describe \
             details you cannot verify, and do not call the tool again unless the user wants \
             a different picture."
        ));
    }
    if let Some(path) = &rm.text {
        if let Some(v) = parsed.as_ref().and_then(|j| json_path(j, path)) {
            return Ok(match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            });
        }
    }
    Ok(text)
}

/// Walk a dot path ("data.0.b64_json") through keys and array indexes.
fn json_path<'a>(v: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut cur = v;
    for seg in path.split('.') {
        cur = match seg.parse::<usize>() {
            Ok(i) => cur.get(i)?,
            Err(_) => cur.get(seg)?,
        };
    }
    Some(cur)
}

/// (mime, base64) from a field that may be raw base64 or a full data URI.
fn image_payload(raw: &str) -> (String, String) {
    if let Some(rest) = raw.strip_prefix("data:") {
        if let Some((meta, b64)) = rest.split_once(',') {
            let mime = meta.split(';').next().unwrap_or("").trim();
            let mime = if mime.is_empty() { "image/png" } else { mime };
            return (mime.to_string(), b64.to_string());
        }
    }
    ("image/png".to_string(), raw.to_string())
}

/// A scalar argument as a bare string: JSON strings lose their quotes, numbers
/// and booleans print themselves, anything structured stays JSON.
fn scalar(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Replace `{name}` in a URL with the matching argument, percent-encoded.
/// Returns the names it consumed so they are not ALSO sent as query params.
fn substitute<'a>(
    url: &str,
    obj: &'a serde_json::Map<String, serde_json::Value>,
) -> (String, Vec<&'a str>) {
    let mut out = String::with_capacity(url.len());
    let mut used = Vec::new();
    let mut rest = url;
    while let Some(i) = rest.find('{') {
        let Some(j) = rest[i..].find('}') else { break };
        let key = &rest[i + 1..i + j];
        match obj.get_key_value(key) {
            Some((k, v)) => {
                out.push_str(&rest[..i]);
                out.push_str(&pct(&scalar(v)));
                used.push(k.as_str());
            }
            None => out.push_str(&rest[..i + j + 1]),
        }
        rest = &rest[i + j + 1..];
    }
    out.push_str(rest);
    (out, used)
}

/// Drop template holes the fill left behind, so a declared-but-omitted
/// argument means "send nothing" instead of sending the literal "$name".
/// Scoped tightly: only a WHOLE string value (fill_template's own rule)
/// naming one of the tool's DECLARED parameters (or the reserved image
/// slots, for a template used on an imageless routed turn) is a hole -
/// literal text containing a `$` still travels untouched.
fn prune_unfilled(v: serde_json::Value, t: &HttpTool) -> serde_json::Value {
    fn hole(v: &serde_json::Value, t: &HttpTool) -> bool {
        let Some(name) = v.as_str().and_then(|s| s.strip_prefix('$')) else { return false };
        let name = name.strip_prefix('{').and_then(|x| x.strip_suffix('}')).unwrap_or(name);
        if matches!(name, "image" | "images") {
            return true;
        }
        t.parameters
            .as_ref()
            .and_then(|p| p.get("properties"))
            .and_then(|p| p.as_object())
            .is_some_and(|props| props.contains_key(name))
    }
    match v {
        serde_json::Value::Object(o) => serde_json::Value::Object(
            o.into_iter()
                .filter(|(_, val)| !hole(val, t))
                .map(|(k, val)| (k, prune_unfilled(val, t)))
                .collect(),
        ),
        serde_json::Value::Array(a) => serde_json::Value::Array(
            a.into_iter().filter(|x| !hole(x, t)).map(|x| prune_unfilled(x, t)).collect(),
        ),
        other => other,
    }
}

/// Fill `"$arg"` holes in a body template. Only a WHOLE string value is
/// replaced, so a template can carry literal text containing a `$` safely.
fn fill_template(
    tpl: &serde_json::Value,
    obj: &serde_json::Map<String, serde_json::Value>,
) -> serde_json::Value {
    match tpl {
        serde_json::Value::String(s) => {
            let name = s.strip_prefix('$').map(|r| {
                r.strip_prefix('{').and_then(|x| x.strip_suffix('}')).unwrap_or(r).to_string()
            });
            match name.and_then(|n| obj.get(&n).cloned()) {
                Some(v) => v,
                None => tpl.clone(),
            }
        }
        serde_json::Value::Array(a) => {
            serde_json::Value::Array(a.iter().map(|x| fill_template(x, obj)).collect())
        }
        serde_json::Value::Object(o) => serde_json::Value::Object(
            o.iter().map(|(k, v)| (k.clone(), fill_template(v, obj))).collect(),
        ),
        other => other.clone(),
    }
}

fn pct(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ------------------------------------------------------------------- MCP --

fn discover(sess: &mut McpSession) -> Result<Vec<McpToolDecl>, String> {
    let init = rpc(
        sess,
        "initialize",
        Some(serde_json::json!({
            "protocolVersion": sess.version,
            "capabilities": {},
            "clientInfo": { "name": "eyesoff-ai", "version": env!("CARGO_PKG_VERSION") },
        })),
    )?;
    // the server may answer with a revision of its own choosing; speak its
    // language from here on rather than insisting on ours
    if let Some(v) = init.get("protocolVersion").and_then(|v| v.as_str()) {
        sess.version = v.to_string();
    }
    notify(sess, "notifications/initialized")?;
    let list = rpc(sess, "tools/list", None)?;
    let arr = list
        .get("tools")
        .and_then(|t| t.as_array())
        .ok_or("tools/list returned no tools array")?;
    Ok(arr
        .iter()
        .filter_map(|t| {
            Some(McpToolDecl {
                name: t.get("name")?.as_str()?.to_string(),
                description: t
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string(),
                parameters: t.get("inputSchema").cloned(),
            })
        })
        .collect())
}

fn call_mcp(
    sess: &mut McpSession,
    remote: &str,
    args: &serde_json::Value,
) -> Result<String, String> {
    // a turn that skipped discovery (inline tools) never handshook
    if sess.session_id.is_none() && sess.next_id == 1 {
        let init = rpc(
            sess,
            "initialize",
            Some(serde_json::json!({
                "protocolVersion": sess.version,
                "capabilities": {},
                "clientInfo": { "name": "eyesoff-ai", "version": env!("CARGO_PKG_VERSION") },
            })),
        )?;
        if let Some(v) = init.get("protocolVersion").and_then(|v| v.as_str()) {
            sess.version = v.to_string();
        }
        notify(sess, "notifications/initialized")?;
    }
    let r = rpc(
        sess,
        "tools/call",
        Some(serde_json::json!({ "name": remote, "arguments": args })),
    )?;
    let text = render_mcp_content(&r);
    if r.get("isError").and_then(|e| e.as_bool()).unwrap_or(false) {
        return Err(if text.is_empty() {
            format!("mcp tool '{remote}' reported an error")
        } else {
            text
        });
    }
    Ok(text)
}

/// An MCP result as text. Text parts join; structured output falls back to its
/// JSON; anything binary is named rather than dumped, because a base64 image in
/// the prompt is thousands of tokens of nothing the model can read.
fn render_mcp_content(r: &serde_json::Value) -> String {
    let mut parts = Vec::new();
    if let Some(a) = r.get("content").and_then(|c| c.as_array()) {
        for c in a {
            match c.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    if let Some(t) = c.get("text").and_then(|t| t.as_str()) {
                        parts.push(t.to_string());
                    }
                }
                Some(other) => parts.push(format!("[{other} content omitted]")),
                None => {}
            }
        }
    }
    if parts.is_empty() {
        if let Some(s) = r.get("structuredContent") {
            return s.to_string();
        }
    }
    parts.join("\n")
}

fn notify(sess: &mut McpSession, method: &str) -> Result<(), String> {
    let body = serde_json::json!({ "jsonrpc": "2.0", "method": method }).to_string();
    let r = post(sess, body.as_bytes())?;
    if r.status >= 400 {
        return Err(format!("{method} was refused: HTTP {}", r.status));
    }
    Ok(())
}

fn rpc(
    sess: &mut McpSession,
    method: &str,
    params: Option<serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let id = sess.next_id;
    sess.next_id += 1;
    let mut msg = serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method });
    if let Some(p) = params {
        msg["params"] = p;
    }
    let body = msg.to_string();
    let r = post(sess, body.as_bytes())?;
    if r.status >= 400 {
        let hint: String = String::from_utf8_lossy(&r.body).chars().take(300).collect();
        return Err(format!("{method} failed: HTTP {} {hint}", r.status));
    }
    let v = decode_rpc(&r.body, id)
        .ok_or_else(|| format!("{method}: no JSON-RPC response in the reply"))?;
    if let Some(e) = v.get("error") {
        let m = e.get("message").and_then(|m| m.as_str()).unwrap_or("unspecified");
        return Err(format!("{method} failed: {m}"));
    }
    Ok(v.get("result").cloned().unwrap_or(serde_json::Value::Null))
}

fn post(sess: &mut McpSession, body: &[u8]) -> Result<http::Response, String> {
    let url = sess.url.clone();
    let mut req = HttpReq::post(&url, body)
        .timeout(sess.timeout_s)
        .header("content-type", b"application/json")
        .header("accept", b"application/json, text/event-stream");
    if let Some(s) = &sess.session_id {
        req = req.header("mcp-session-id", s.as_bytes());
    }
    // the revision that introduced this transport requires the header on every
    // request AFTER initialize - which includes the initialized NOTIFICATION,
    // the first thing sent once the handshake has an answer
    if sess.next_id >= 2 {
        req = req.header("mcp-protocol-version", sess.version.as_bytes());
    }
    for (k, v) in &sess.headers {
        req = req.header(k, v.as_bytes());
    }
    let r = http::request(req)?;
    // the server assigns a session on initialize and expects it echoed on
    // everything after
    if let Some(s) = r.header("mcp-session-id") {
        sess.session_id = Some(s.to_string());
    }
    Ok(r)
}

/// A JSON-RPC response out of either framing: a bare JSON body, or an SSE
/// stream whose `data:` lines carry the message (the streamable-HTTP transport
/// lets the server pick, per request).
fn decode_rpc(body: &[u8], id: u64) -> Option<serde_json::Value> {
    let s = String::from_utf8_lossy(body);
    let t = s.trim();
    if t.starts_with('{') {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(t) {
            return matches_id(v, id);
        }
    }
    // SSE: frames separated by a blank line, payload in one or more data: lines
    for frame in t.split("\n\n") {
        let mut data = String::new();
        for line in frame.lines() {
            if let Some(rest) = line.trim_end_matches('\r').strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest.trim_start());
            }
        }
        if data.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&data) {
            if let Some(v) = matches_id(v, id) {
                return Some(v);
            }
        }
    }
    None
}

/// A response for OUR request. The transport is allowed to interleave the
/// server's own requests and notifications on the same stream, and answering
/// from one of those would be answering the wrong question.
fn matches_id(v: serde_json::Value, id: u64) -> Option<serde_json::Value> {
    match v.get("id").and_then(|i| i.as_u64()) {
        Some(got) if got == id => Some(v),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> Tool {
        Tool {
            name: name.into(),
            description: "does a thing".into(),
            parameters: serde_json::json!({"type":"object","properties":{"q":{"type":"string"}}}),
            src: ToolSrc::Http(0),
        }
    }

    #[test]
    fn endpoints_group_into_one_switch_each() {
        let cfg: ToolsConfig = serde_json::from_value(serde_json::json!({
            "builtin": ["web_search", "request"],
            "http": [
                { "name": "notes_list", "url": "http://j/api/notes" },
                { "name": "notes_write", "url": "http://j/api/notes/{name}", "method": "PUT" },
                { "name": "run_vm_command", "url": "http://vm/exec", "method": "POST" },
                { "name": "generate_image", "url": "http://img/v1", "method": "POST",
                  "body": { "prompt": "$prompt" }, "result": { "image": "data.0.b64_json" } }
            ]
        })).unwrap();
        let g = cfg.groups(Some(false), Some(true));
        let names: Vec<&str> = g.iter().map(|x| x["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["search", "images", "notes", "run_vm_command"],
                   "a family of endpoints is one switch; a lone one is its own");
        assert_eq!(g[2]["tools"], serde_json::json!(["notes_list", "notes_write"]));
        assert_eq!(g[2]["label"], "Notes");
        assert_eq!(g[1]["tools"], serde_json::json!(["generate_image"]), "a picture-maker joins images");
        assert_eq!(g[0]["tools"], serde_json::json!(["web_search", "request"]));
        // a switched-off group is not in the registry, silently
        let off = vec!["notes".to_string()];
        let b = Builtins { search: None, web_withheld: true, off: &off, ..Default::default() };
        let reg = build(&cfg, b, &|_| {});
        let offered: Vec<&str> = reg.tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(offered, ["run_vm_command", "generate_image"]);
        assert!(reg.notes.is_empty(), "{:?}", reg.notes);
    }

    #[test]
    fn tagged_calls_parse() {
        let c = parse_calls("<tool_call>\n{\"name\": \"get_weather\", \"arguments\": {\"city\": \"Oslo\"}}\n</tool_call>");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].name, "get_weather");
        assert_eq!(c[0].args["city"], "Oslo");
    }

    #[test]
    fn unterminated_call_still_parses() {
        // the stop string eats </tool_call>, which must not cost us the call
        let c = parse_calls("<tool_call>\n{\"name\": \"a\", \"arguments\": {}}");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].name, "a");
    }

    #[test]
    fn several_calls_in_one_reply() {
        let c = parse_calls(
            "<tool_call>{\"name\":\"a\",\"arguments\":{}}</tool_call>\n\
             <tool_call>{\"name\":\"b\",\"arguments\":{\"x\":1}}</tool_call>",
        );
        assert_eq!(c.len(), 2);
        assert_eq!(c[1].name, "b");
        assert_eq!(c[1].args["x"], 1);
    }

    #[test]
    fn reasoning_about_a_call_is_not_a_call() {
        // inside a think block the model is deciding, not calling
        let c = parse_calls("<think>\nI could use <tool_call>{\"name\":\"a\",\"arguments\":{}}</tool_call>\n</think>\nHere you go.");
        assert!(c.is_empty(), "{c:?}");
        // ...but a call AFTER the block closes is real
        let c = parse_calls("<think>\nplan\n</think>\n<tool_call>{\"name\":\"a\",\"arguments\":{}}</tool_call>");
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn quoting_a_call_in_prose_is_not_a_call() {
        let t = "You could send {\"name\": \"a\", \"arguments\": {}} to that endpoint.";
        assert!(parse_calls(t).is_empty());
    }

    #[test]
    fn bare_and_fenced_objects_parse() {
        let c = parse_calls("{\"name\": \"a\", \"arguments\": {\"x\": 2}}");
        assert_eq!(c.len(), 1);
        let c = parse_calls("```json\n{\"name\": \"a\", \"arguments\": {}}\n```");
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn stringified_arguments_are_reparsed() {
        let c = parse_calls("<tool_call>{\"name\":\"a\",\"arguments\":\"{\\\"x\\\":1}\"}</tool_call>");
        assert_eq!(c[0].args["x"], 1);
    }

    #[test]
    fn name_inside_arguments_is_rescued() {
        // seen live (fable-fusion-27b, 2026-08-05): the name tucked into the
        // arguments object and no top-level name at all. The stop string had
        // also eaten </tool_call>.
        let c = parse_calls(
            "<tool_call>\n{\"arguments\":{\"url\":\"https://enclave.host/develop#api\",\"name\":\"fetch_url\"}}",
        );
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].name, "fetch_url");
        assert_eq!(c[0].args["url"], "https://enclave.host/develop#api");
        // the function name is not an argument
        assert!(c[0].args.get("name").is_none(), "{:?}", c[0].args);
    }

    /// The 2026-09-03 shape: the functionary tag spelling welded onto the
    /// trained form, so the block opens with `<` instead of `{` and carries a
    /// stray quote at the end. The object between them is COMPLETE, and every
    /// path here used to need the chunk to start with `{`, so the whole call
    /// was thrown away and delivered to the user as raw text.
    #[test]
    fn a_junk_wrapper_does_not_cost_the_call() {
        let raw = "<tool_call>\n<function\": {\"name\": \"notes_write\", \"arguments\": \
                   {\"name\": \"memory/preferences.md\", \"content\": \"- Name: Steven\"}}\"";
        let c = parse_calls(raw);
        assert_eq!(c.len(), 1, "{c:?}");
        assert_eq!(c[0].name, "notes_write");
        // the note's own `name` argument survives: the top-level name was
        // found first, so the misplaced-name rescue never ran
        assert_eq!(c[0].args["name"], "memory/preferences.md");
        assert_eq!(c[0].args["content"], "- Name: Steven");
        // the same wrapper around a truncated object still lands the call
        let cut = "<tool_call>\n<function\": {\"name\": \"notes_write\", \"arguments\": \
                   {\"name\": \"a.md\", \"content\": \"half";
        let c = parse_calls(cut);
        assert_eq!(c.len(), 1, "{c:?}");
        assert_eq!(c[0].args["content"], "half");
        // ...but junk with no object in it is still not a call
        assert!(parse_calls("<tool_call>\n<function\": notes_write").is_empty());
    }

    fn client_tool(name: &str, props: &[&str], required: &[&str]) -> Tool {
        let properties: serde_json::Map<String, serde_json::Value> =
            props.iter().map(|p| ((*p).to_string(), serde_json::json!({"type": "string"}))).collect();
        Tool {
            name: name.to_string(),
            description: String::new(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": properties,
                "required": required,
            }),
            src: ToolSrc::Client,
        }
    }

    #[test]
    fn call_with_no_name_at_all_is_identified_by_its_arguments() {
        // seen live (fable-fusion-27b, 2026-08-08, opencode writing a file):
        // a complete, valid tool_call block carrying ONLY `arguments`
        let reg = [
            client_tool("write", &["filePath", "content"], &["filePath", "content"]),
            client_tool("bash", &["command", "workdir"], &["command"]),
        ];
        let raw = "<tool_call>\n{\"arguments\":{\"content\":\"print(1)\\n\",\
                   \"filePath\":\"/home/steven/pacman/pacman.py\"}}\n</tool_call>";
        // without the registry there is nothing to match against
        assert!(parse_calls(raw).is_empty());
        let c = parse_calls_for(raw, &reg);
        assert_eq!(c.len(), 1, "{c:?}");
        assert_eq!(c[0].name, "write");
        assert_eq!(c[0].args["filePath"], "/home/steven/pacman/pacman.py");
    }

    #[test]
    fn an_ambiguous_nameless_call_is_not_guessed() {
        let reg = [
            client_tool("read", &["path"], &["path"]),
            client_tool("stat", &["path"], &["path"]),
        ];
        let raw = "<tool_call>{\"arguments\":{\"path\":\"/etc/hosts\"}}</tool_call>";
        assert!(parse_calls_for(raw, &reg).is_empty());
    }

    #[test]
    fn a_nameless_call_with_undeclared_keys_is_not_guessed() {
        let reg = [client_tool("write", &["filePath", "content"], &["filePath"])];
        let raw = "<tool_call>{\"arguments\":{\"filePath\":\"/a\",\"mode\":\"755\"}}</tool_call>";
        assert!(parse_calls_for(raw, &reg).is_empty(), "`mode` is not in the schema");
    }

    #[test]
    fn a_real_name_argument_is_kept() {
        // the rescue must not touch a call whose TOOL takes a `name` parameter
        let c = parse_calls("<tool_call>{\"name\":\"lookup\",\"arguments\":{\"name\":\"steve\"}}</tool_call>");
        assert_eq!(c[0].name, "lookup");
        assert_eq!(c[0].args["name"], "steve");
    }

    #[test]
    fn url_placeholders_are_substituted_and_encoded() {
        let mut obj = serde_json::Map::new();
        obj.insert("id".into(), serde_json::json!("a b/c"));
        obj.insert("q".into(), serde_json::json!("x"));
        let (url, used) = substitute("https://h/items/{id}/detail", &obj);
        assert_eq!(url, "https://h/items/a%20b%2Fc/detail");
        assert_eq!(used, vec!["id"]);
        // an unknown placeholder is left alone rather than blanked
        let (url, used) = substitute("https://h/{nope}", &obj);
        assert_eq!(url, "https://h/{nope}");
        assert!(used.is_empty());
    }

    #[test]
    fn body_templates_fill_from_arguments() {
        let mut obj = serde_json::Map::new();
        obj.insert("city".into(), serde_json::json!("Oslo"));
        let tpl = serde_json::json!({"query": "$city", "opts": {"n": 3}, "lit": "$5 each"});
        let out = fill_template(&tpl, &obj);
        assert_eq!(out["query"], "Oslo");
        assert_eq!(out["opts"]["n"], 3);
        // "$5 each" is not an identifier, so it stays literal
        assert_eq!(out["lit"], "$5 each");
    }

    #[test]
    fn omitted_optional_args_are_pruned_from_body_templates() {
        // the upscale-tool shape: image reserved, factor optional
        let t: HttpTool = serde_json::from_value(serde_json::json!({
            "name": "upscale_image",
            "url": "https://h/v1/images/upscale",
            "method": "POST",
            "parameters": {
                "type": "object",
                "properties": { "factor": { "type": "integer", "enum": [1, 2, 4] } },
            },
            "body": { "image": "$image", "factor": "$factor", "lit": "$5 each", "keep": "$UNDECLARED" },
        }))
        .unwrap();
        // factor omitted by the model, no image on the turn either
        let obj = serde_json::Map::new();
        let filled = prune_unfilled(fill_template(t.body.as_ref().unwrap(), &obj), &t);
        assert!(filled.get("factor").is_none(), "{filled}");
        assert!(filled.get("image").is_none(), "{filled}");
        assert_eq!(filled["lit"], "$5 each"); // not an identifier hole
        assert_eq!(filled["keep"], "$UNDECLARED"); // not a declared parameter
        // factor provided: filled and kept
        let mut obj = serde_json::Map::new();
        obj.insert("factor".into(), serde_json::json!(2));
        let filled = prune_unfilled(fill_template(t.body.as_ref().unwrap(), &obj), &t);
        assert_eq!(filled["factor"], 2);
    }

    #[test]
    fn unresolved_secrets_are_not_sent() {
        let mut h = BTreeMap::new();
        h.insert("authorization".into(), "Bearer $TOOL_KEY".into());
        h.insert("x-fixed".into(), "plain".into());
        let mut notes = Vec::new();
        let out = resolved_headers(&h, &mut notes, "https://h/x");
        assert_eq!(out, vec![("x-fixed".to_string(), "plain".to_string())]);
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("TOOL_KEY"), "{}", notes[0]);
        // a value with no placeholder is untouched
        assert_eq!(unresolved_in("Bearer sk-live-123"), None);
        assert_eq!(unresolved_in("${A}"), Some("A".into()));
    }

    #[test]
    fn names_the_model_cannot_reproduce_are_refused() {
        assert!(check_name("get_weather").is_ok());
        assert!(check_name("a-b1").is_ok());
        assert!(check_name("has space").is_err());
        assert!(check_name("").is_err());
    }

    #[test]
    fn registry_drops_duplicates_and_reports_them() {
        let cfg: ToolsConfig = serde_json::from_value(serde_json::json!({
            "http": [
                {"name": "a", "url": "https://h/1"},
                {"name": "a", "url": "https://h/2"},
                {"name": "bad name", "url": "https://h/3"},
            ]
        }))
        .unwrap();
        let reg = build(&cfg, Builtins::default(), &|_| {});
        assert_eq!(reg.tools.len(), 1);
        assert_eq!(reg.notes.len(), 2);
    }

    /// /models advertises names; resolution drops some. They have to agree,
    /// or the playground offers a tool that cannot be called.
    #[test]
    fn advertised_names_match_the_ones_that_resolve() {
        let cfg: ToolsConfig = serde_json::from_value(serde_json::json!({
            "http": [
                {"name": "a", "url": "https://h/1"},
                {"name": "a", "url": "https://h/2"},
                {"name": "bad name", "url": "https://h/3"},
                {"name": "b", "url": "https://h/4"},
            ]
        }))
        .unwrap();
        let reg = build(&cfg, Builtins::default(), &|_| {});
        let resolved: Vec<String> = reg.tools.iter().map(|t| t.name.clone()).collect();
        assert_eq!(cfg.http_names(), resolved);
        assert_eq!(resolved, vec!["a".to_string(), "b".to_string()]);
    }

    /// Built-ins only appear when the capability behind them is configured.
    /// A model told it can search on a deployment with no provider would call
    /// a tool that can only ever fail.
    #[test]
    fn builtins_need_the_capability_they_are_backed_by() {
        let cfg: ToolsConfig = serde_json::from_value(serde_json::json!({
            "builtin": ["web_search", "fetch_url", "post_url", "teleport"]
        }))
        .unwrap();
        // no capability blocks: nothing is offered, and every reason is
        // reported - once per capability, not once per alias
        let reg = build(&cfg, Builtins::default(), &|_| {});
        assert!(reg.is_empty());
        assert_eq!(reg.notes.len(), 3, "{:?}", reg.notes);
        assert!(reg.notes.iter().any(|n| n.contains("teleport")), "{:?}", reg.notes);
        assert!(reg.notes.iter().any(|n| n.contains("`search` block")), "{:?}", reg.notes);

        // with a search block, the real ones resolve (fetch_url and post_url
        // folding into ONE request tool) and the invented one still does not
        let scfg: crate::search::SearchConfig =
            serde_json::from_value(serde_json::json!({ "provider": "exa" })).unwrap();
        let b = Builtins { search: Some(&scfg), ..Default::default() };
        let reg = build(&cfg, b, &|_| {});
        let names: Vec<&str> = reg.tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["web_search", "request"]);
        assert_eq!(reg.notes.len(), 1);
        // and /models advertises exactly those
        assert_eq!(cfg.http_names(), vec!["web_search".to_string(), "request".to_string()]);
    }

    /// The generic powers that let ANY API be an image or vision tool: a
    /// $images body template gates the entry on the turn's pictures, and a
    /// result map types what comes back.
    #[test]
    fn image_reading_and_making_ride_generic_entries() {
        let cfg: ToolsConfig = serde_json::from_value(serde_json::json!({
            "http": [
                {
                    "name": "draw",
                    "url": "https://img.example/v1/images/generations",
                    "method": "POST",
                    "body": { "prompt": "$prompt", "n": 1 },
                    "result": { "image": "data.0.b64_json" }
                },
                {
                    "name": "look",
                    "url": "https://eyes.example/v1/vision",
                    "method": "POST",
                    "body": { "question": "$question", "images": "$images" },
                    "result": { "text": "answer" }
                }
            ]
        }))
        .unwrap();
        assert!(cfg.http[0].makes_image() && !cfg.http[0].wants_images());
        assert!(cfg.http[1].wants_images() && !cfg.http[1].makes_image());
        // image attached: both offered, and the registry knows their roles
        let b = Builtins { images_present: true, ..Default::default() };
        let reg = build(&cfg, b, &|_| {});
        let names: Vec<&str> = reg.tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["draw", "look"]);
        assert!(reg.makes_image(&cfg));
        assert_eq!(reg.image_reader(&cfg), Some("look"));
        // no picture this turn: the reader vanishes silently
        let reg = build(&cfg, Builtins::default(), &|_| {});
        let names: Vec<&str> = reg.tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["draw"]);
        assert!(reg.notes.is_empty(), "{:?}", reg.notes);
        assert_eq!(reg.image_reader(&cfg), None);
        // the serving model reads pictures itself: same silent stand-down
        let b = Builtins { images_present: true, images_local: true, ..Default::default() };
        let reg = build(&cfg, b, &|_| {});
        let names: Vec<&str> = reg.tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["draw"]);
    }

    /// The response side of the generic entries: extraction by dot path, an
    /// image result leaving as bytes-for-the-client plus a note the model can
    /// act on, and tolerant fallbacks.
    #[test]
    fn result_maps_shape_what_comes_back() {
        let draw: HttpTool = serde_json::from_value(serde_json::json!({
            "name": "draw", "url": "https://img.example/g", "method": "POST",
            "result": { "image": "data.0.b64_json" }
        }))
        .unwrap();
        let mut img = None;
        let out = map_result(
            &draw,
            serde_json::json!({"data": [{"b64_json": "QUJD"}]}).to_string(),
            &serde_json::json!({"prompt": "a fox"}),
            &mut img,
        )
        .unwrap();
        assert!(out.contains("a fox"), "{out}");
        let g = img.expect("image extracted");
        assert_eq!(g.b64, "QUJD");
        assert_eq!(g.mime, "image/png");
        // a data URI keeps its own mime
        let mut img = None;
        map_result(
            &draw,
            serde_json::json!({"data": [{"b64_json": "data:image/webp;base64,QUJD"}]}).to_string(),
            &serde_json::json!({}),
            &mut img,
        )
        .unwrap();
        let g = img.expect("image extracted");
        assert_eq!((g.mime.as_str(), g.b64.as_str()), ("image/webp", "QUJD"));
        // a response with no image at the path is an error naming the path
        let mut img = None;
        let e = map_result(&draw, "{\"error\": \"busy\"}".into(), &serde_json::json!({}), &mut img)
            .unwrap_err();
        assert!(e.contains("data.0.b64_json"), "{e}");
        assert!(img.is_none());
        // a text path extracts the field; a missing one falls back to the body
        let look: HttpTool = serde_json::from_value(serde_json::json!({
            "name": "look", "url": "https://eyes.example/v", "method": "POST",
            "result": { "text": "answer" }
        }))
        .unwrap();
        let mut img = None;
        let out = map_result(
            &look,
            serde_json::json!({"answer": "a receipt", "tokens": 512}).to_string(),
            &serde_json::json!({}),
            &mut img,
        )
        .unwrap();
        assert_eq!(out, "a receipt");
        let whole = serde_json::json!({"other": 1}).to_string();
        let out = map_result(&look, whole.clone(), &serde_json::json!({}), &mut img).unwrap();
        assert_eq!(out, whole);
    }

    /// The turn's pictures ride the body template as reserved names, and they
    /// shadow any argument the model wrote under those names.
    #[test]
    fn images_are_injected_into_body_templates() {
        let mut obj = serde_json::Map::new();
        obj.insert("question".into(), serde_json::json!("what is this?"));
        obj.insert("images".into(), serde_json::json!("model-written nonsense"));
        let png = vec![0x89, b'P', b'N', b'G', 0];
        let filled = fill_template(
            &serde_json::json!({ "q": "$question", "imgs": "$images", "one": "$image" }),
            &with_images(&obj, &[png]),
        );
        assert_eq!(filled["q"], "what is this?");
        let uri = filled["imgs"][0].as_str().unwrap();
        assert!(uri.starts_with("data:image/png;base64,"), "{uri}");
        assert_eq!(filled["one"].as_str().unwrap(), uri);
        // no pictures: the model's own argument is left alone
        let filled = fill_template(
            &serde_json::json!({ "imgs": "$images" }),
            &with_images(&obj, &[]),
        );
        assert_eq!(filled["imgs"], "model-written nonsense");
    }

    #[test]
    fn request_payloads_take_both_shapes() {
        // a JSON object is serialized and defaults to application/json
        let (b, ct) = request_payload(&serde_json::json!({"body": {"x": 1}}), true).unwrap();
        assert_eq!(b.as_deref(), Some("{\"x\":1}"));
        assert_eq!(ct, "application/json");
        // a string goes as-is, and content_type overrides the default
        let (b, ct) = request_payload(
            &serde_json::json!({
                "body": "a=1&b=2", "content_type": "application/x-www-form-urlencoded"
            }),
            true,
        )
        .unwrap();
        assert_eq!(b.as_deref(), Some("a=1&b=2"));
        assert_eq!(ct, "application/x-www-form-urlencoded");
        // a missing body on a write method is an error that names the field
        assert!(request_payload(&serde_json::json!({}), true).unwrap_err().contains("body"));
        assert!(request_payload(&serde_json::json!({"body": null}), true).is_err());
        // ...but a DELETE goes without one
        let (b, _) = request_payload(&serde_json::json!({}), false).unwrap();
        assert!(b.is_none());
    }

    /// The user's search switch governs the tool, and turning it off is not a
    /// misconfiguration: the tool disappears without a word of complaint.
    #[test]
    fn withholding_the_web_removes_the_tool_silently() {
        let cfg: ToolsConfig =
            serde_json::from_value(serde_json::json!({ "builtin": ["web_search"] })).unwrap();
        let b = Builtins { search: None, web_withheld: true, ..Default::default() };
        let reg = build(&cfg, b, &|_| {});
        assert!(reg.is_empty());
        assert!(reg.notes.is_empty(), "{:?}", reg.notes);
        // whereas a deployment that simply forgot the search block IS told
        let reg = build(&cfg, Builtins::default(), &|_| {});
        assert_eq!(reg.notes.len(), 1);
    }

    /// `tool`, but with the source that decides who executes it.
    fn from(name: &str, src: ToolSrc) -> Tool {
        Tool { src, ..tool(name) }
    }

    /// Run 6, live: a truncated nameless call with double-encoded arguments,
    /// then a STRAY `</think>` with no opener, then the real `write`. A body
    /// must end at the next marker of any kind or the first one swallows the
    /// reply and the good call is never seen. The nameless one is recovered
    /// too, from its argument keys, once its inner JSON is closed.
    #[test]
    fn a_truncated_body_does_not_swallow_the_call_after_it() {
        let mut todowrite = tool("todowrite");
        todowrite.parameters =
            serde_json::json!({"type":"object","properties":{"todos":{"type":"array"}}});
        let reg = [todowrite, tool("write")];
        let reply = "</think>\n\n<tool_call>\n\
                     {\"arguments\":\"{\\\"todos\\\":[{\\\"content\\\":\\\"plan the maze\
                     \n</think>\n\n<tool_call>\n\
                     {\"name\": \"write\", \"arguments\": {\"filePath\": \"/a.html\", \
                     \"content\": \"<!DOCTYPE html>\\n</html>\\n\"}";
        let calls = parse_calls_for(reply, &reg);
        let names: Vec<&str> = calls.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"write"), "the write must survive: {names:?}");
        let w = calls.iter().find(|c| c.name == "write").unwrap();
        assert_eq!(w.args["content"], "<!DOCTYPE html>\n</html>\n");
    }

    /// A model that omits `</tool_call>` never fires the stop string, so one
    /// reply carries a COMPLETE call, another think block, and then a second
    /// call the token cap cut in half. BOTH come back: the complete one because
    /// a truncated tail is not evidence it never happened, and the cut one
    /// because close_truncated can finish it. Observed live 2026-08-14 from
    /// qwen3.8 through opencode, where anchoring on the last </think> threw the
    /// complete call away and the cut one would not parse.
    #[test]
    fn both_calls_survive_a_reply_that_never_closed_its_tag() {
        let reg = [tool("todowrite"), tool("write")];
        let reply = "<think>\nplanning\n</think>\n\
                     <tool_call>\n{\"function\": \"todowrite\", \"arguments\": {\"q\": \"a\"}}\n\
                     <think>\nnow the file, let me count the maze rows\n</think>\n\
                     <tool_call>\n{\"function\": \"write\", \"arguments\": {\"q\": \"<!DOCTYPE";
        let calls = parse_calls_for(reply, &reg);
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!(calls[0].name, "todowrite");
        assert_eq!(calls[0].args["q"], "a");
        assert_eq!(calls[1].name, "write");
        assert_eq!(calls[1].args["q"], "<!DOCTYPE");
    }

    /// A model that never writes `</tool_call>` never fires the stop string, so
    /// its reply ends when the budget does - mid-object, one or two closers
    /// short, with the actual payload already complete. Measured live
    /// 2026-08-14: a `write` carrying a finished 29 KB index.html, exactly one
    /// `}` short, discarded in full. Closing what is open recovers the file.
    #[test]
    fn a_call_the_budget_cut_off_is_still_run() {
        let reg = [tool("write")];
        let cut = "<tool_call>\n{\"name\": \"write\", \"arguments\": \
                   {\"filePath\": \"/a.html\", \"content\": \"<html>done</html>\\n\"}";
        let c = parse_calls_for(cut, &reg);
        assert_eq!(c.len(), 1, "{c:?}");
        assert_eq!(c[0].name, "write");
        assert_eq!(c[0].args["content"], "<html>done</html>\n");

        // cut INSIDE the string: the partial value survives, the call still runs
        let mid = "<tool_call>\n{\"name\": \"write\", \"arguments\": \
                   {\"filePath\": \"/a.html\", \"content\": \"<html>half";
        let c = parse_calls_for(mid, &reg);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].args["content"], "<html>half");

        // genuinely malformed is still refused: nothing was left open
        assert!(close_truncated("{\"a\": 1}").is_none());
        assert!(close_truncated("not json").is_none());
    }

    /// The third spelling this model produces, captured live 2026-08-14: the
    /// functionary `<function=name>` tag welded onto a fragment of the trained
    /// form. Both that hybrid and the documented `<function=name>{args}` have
    /// to read, because which one arrives is a coin toss.
    #[test]
    fn the_function_tag_spelling_is_still_a_call() {
        let reg = [tool("read")];
        let hybrid = "<tool_call>\n<function=read>, \"arguments\": \
                      {\"filePath\": \"/x/game\"}}";
        let c = parse_calls_for(hybrid, &reg);
        assert_eq!(c.len(), 1, "{c:?}");
        assert_eq!(c[0].name, "read");
        assert_eq!(c[0].args["filePath"], "/x/game");

        let documented = "<tool_call>\n<function=read>{\"filePath\": \"/y\"}</function>\n</tool_call>";
        let c = parse_calls_for(documented, &reg);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].name, "read");
        assert_eq!(c[0].args["filePath"], "/y");
    }

    /// The 2026-09-03 screenshot: `<functionname = "web_search">`, the tag
    /// with its `name` attribute welded on, and the call never wrapped in an
    /// object at all - so `"arguments"` sits outside every brace and the only
    /// balanced object IS the arguments value. Recovering an object cannot
    /// help here (it yields no name and no arguments), so the tag has to be
    /// read in whatever spelling the model reached for.
    #[test]
    fn the_function_tag_is_read_in_every_spelling() {
        let c = parse_calls(
            "<tool_call>\n<functionname = \"web_search\"> \"arguments\": \
             {\"query\": \"why do dogs have fur evolution biology purpose\"}}",
        );
        assert_eq!(c.len(), 1, "{c:?}");
        assert_eq!(c[0].name, "web_search");
        assert_eq!(c[0].args["query"], "why do dogs have fur evolution biology purpose");

        for raw in [
            "<tool_call><function name=\"read\">{\"p\": \"/a\"}</tool_call>",
            "<tool_call><function=read>{\"p\": \"/a\"}</tool_call>",
            "<tool_call><function \"read\">{\"p\": \"/a\"}</tool_call>",
            "<tool_call><functionname=\"read\">{\"p\": \"/a\"}</tool_call>",
        ] {
            let c = parse_calls(raw);
            assert_eq!(c.len(), 1, "{raw}");
            assert_eq!(c[0].name, "read", "{raw}");
            assert_eq!(c[0].args["p"], "/a", "{raw}");
        }
        // a tool whose name starts with "name" keeps all of it
        let c = parse_calls("<tool_call><function=named_thing>{}</tool_call>");
        assert_eq!(c[0].name, "named_thing");
        // ...and a word that merely starts with "function" is not a tag, so
        // nothing is invented from it
        assert!(parse_calls("<tool_call><functions>{\"a\":1}</tool_call>").is_empty());
    }

    /// The rule the scan actually enforces: a call written INSIDE reasoning is
    /// a model talking about calling something, not a call.
    #[test]
    fn a_call_inside_reasoning_is_still_not_a_call() {
        let reg = [tool("write")];
        let inside = "<think>\nI could write \
                      <tool_call>\n{\"name\": \"write\", \"arguments\": {\"q\": \"x\"}}\n</tool_call>\n\
                      but first let me check\n</think>\n\nLet me check the folder.";
        assert!(parse_calls_for(inside, &reg).is_empty());
    }

    /// The name under OpenAI's key rather than the trained one. Observed from
    /// qwen3.8 through opencode, where a `write` carrying a whole file was
    /// shown to the user as raw JSON because nothing read `"function"` and the
    /// argument keys were not unique enough for infer_name to rescue it.
    #[test]
    fn a_call_named_under_function_is_still_a_call() {
        let mut read = tool("read");
        read.parameters =
            serde_json::json!({"type":"object","properties":{"filePath":{"type":"string"}}});
        let mut write = tool("write");
        write.parameters = serde_json::json!({
            "type":"object",
            "properties":{"filePath":{"type":"string"},"content":{"type":"string"}}
        });
        let reg = [read, write];

        // the flat spelling this model reaches for
        let flat = "<tool_call>\n{\"function\": \"write\", \"arguments\": \
                    {\"filePath\": \"/a.html\", \"content\": \"<!DOCTYPE html>\"}}\n</tool_call>";
        let c = parse_calls_for(flat, &reg);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].name, "write");
        assert_eq!(c[0].args["content"], "<!DOCTYPE html>");

        // and OpenAI's own nesting, which carries the arguments with it
        let nested = "<tool_call>\n{\"function\": {\"name\": \"read\", \"arguments\": \
                      {\"filePath\": \"/a.html\"}}}\n</tool_call>";
        let c = parse_calls_for(nested, &reg);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].name, "read");
        assert_eq!(c[0].args["filePath"], "/a.html");

        // the trained spelling still wins when both are somehow present
        let both = "<tool_call>\n{\"name\": \"read\", \"function\": \"write\", \
                    \"arguments\": {\"filePath\": \"/a.html\"}}\n</tool_call>";
        assert_eq!(parse_calls_for(both, &reg)[0].name, "read");
    }

    /// The deployment's tools join the client's rather than replacing them, so
    /// an agent that brought its own file tools can still reach web_search.
    #[test]
    fn the_merge_offers_both_lists_at_once() {
        let server = [from("web_search", ToolSrc::Http(0)), from("request", ToolSrc::Http(1))];
        let client = [from("read", ToolSrc::Client), from("write", ToolSrc::Client)];
        let all = merge_registries(&server, &client);
        let names: Vec<&str> = all.iter().map(|t| t.name.as_str()).collect();
        // the client's lead: they are the caller's own job, ours supplement it
        assert_eq!(names, ["read", "write", "web_search", "request"]);
        // and one block carries them all, in the trained format
        let block = merged_system_block(&all, Budget::calls(32), None);
        for n in names {
            assert!(block.contains(&format!("\"name\":\"{n}\"")), "{n} missing from {block}");
        }
        // one list, not two: the tag also appears in the prose above it, so
        // count the opening tag as it is actually emitted, on its own line
        assert_eq!(block.matches("\n<tools>\n").count(), 1);
    }

    /// The invariant client_tools has always stated: a client-supplied NAME can
    /// never select a server-executed capability. The client's entry wins and
    /// the server's twin leaves the list, so there is nothing to select.
    #[test]
    fn a_name_collision_is_won_by_the_client() {
        let server = [from("web_search", ToolSrc::Http(0))];
        let client = [from("web_search", ToolSrc::Client)];
        let all = merge_registries(&server, &client);
        assert_eq!(all.len(), 1);
        assert!(matches!(all[0].src, ToolSrc::Client));
    }

    /// The detector's whole job is to be narrow: an unfilled slot is caught,
    /// and a real value that merely READS like one is not, because the cost of
    /// a false positive is a wasted generation on every turn that asks about
    /// any of these words.
    #[test]
    fn an_unfilled_argument_is_told_from_a_real_one() {
        let a = |v: serde_json::Value| stub_arg(&v).map(|(k, val)| format!("{k}={val}"));

        // the live 2026-08-16 call
        assert_eq!(
            a(serde_json::json!({"prompt": "placeholder", "size": "1024x1024"})).as_deref(),
            Some("prompt=placeholder")
        );
        // the other spellings of not having decided
        for v in [
            "Placeholder.", "TODO", "tbd", "...", "…", "string", "your prompt here",
            "  Some text  ", "<prompt>", "[your query]", "{description}",
        ] {
            assert!(
                stub_arg(&serde_json::json!({ "prompt": v })).is_some(),
                "{v:?} is a slot the model never filled"
            );
        }

        // REAL values, including every one that contains a stub word
        for v in [
            "a tall white multi-stage rocket on a tropical island launch pad",
            "what does placeholder mean in typography",
            "placeholder text generators",
            "rust string vs &str",
            "TODO comments in the rust standard library",
            "{\"id\": 7}",                       // a request body, not a slot
            "<html><body>hi</body></html>",      // a document, not a slot
            "<a very long bracketed run of words that is clearly real content>",
            "/v1/models",
        ] {
            assert_eq!(
                stub_arg(&serde_json::json!({ "query": v })),
                None,
                "{v:?} is a value the model chose"
            );
        }

        // non-strings and absent arguments are nothing to do with this
        assert_eq!(stub_arg(&serde_json::json!({"n": 1, "on": true})), None);
        assert_eq!(stub_arg(&serde_json::json!({})), None);
        assert_eq!(stub_arg(&serde_json::json!("placeholder")), None);
    }

    /// A routed line needs a parameter to bind to: route_arg, or the sole
    /// required one; ambiguity is a config note, not a guess.
    #[test]
    fn route_binding_resolves_or_refuses() {
        let t = |v: serde_json::Value| -> HttpTool { serde_json::from_value(v).unwrap() };
        let explicit = t(serde_json::json!({
            "name": "a", "url": "https://h/x", "route": "when x", "route_arg": "q",
            "parameters": {"type": "object", "required": ["q", "n"]}
        }));
        assert_eq!(explicit.route_binding().as_deref(), Some("q"));
        let sole = t(serde_json::json!({
            "name": "b", "url": "https://h/x", "route": "when y",
            "parameters": {"type": "object", "required": ["query"]}
        }));
        assert_eq!(sole.route_binding().as_deref(), Some("query"));
        let ambiguous = t(serde_json::json!({
            "name": "c", "url": "https://h/x", "route": "when z",
            "parameters": {"type": "object", "required": ["q", "n"]}
        }));
        assert_eq!(ambiguous.route_binding(), None);
        // ...and build says so instead of arming a route that can never fire
        let cfg: ToolsConfig = serde_json::from_value(serde_json::json!({
            "http": [{ "name": "c", "url": "https://h/x", "route": "when z",
                       "parameters": {"type": "object", "required": ["q", "n"]} }]
        }))
        .unwrap();
        let reg = build(&cfg, Builtins::default(), &|_| {});
        assert!(reg.notes.iter().any(|n| n.contains("route_arg")), "{:?}", reg.notes);
    }

    /// Citations from any API: a sources map pulls (title, url) rows out of
    /// the response, tolerantly.
    #[test]
    fn sources_maps_extract_hits() {
        let t: HttpTool = serde_json::from_value(serde_json::json!({
            "name": "s", "url": "https://h/x",
            "sources": { "list": "results", "title": "meta.name", "url": "link" }
        }))
        .unwrap();
        let body = serde_json::json!({ "results": [
            { "meta": { "name": "First" }, "link": "https://a" },
            { "meta": {}, "link": "https://b" },
            { "meta": { "name": "" } }
        ]})
        .to_string();
        let mut sources = Vec::new();
        extract_sources(&t, &body, &mut sources);
        assert_eq!(
            sources,
            vec![("First".to_string(), "https://a".to_string()),
                 ("https://b".to_string(), "https://b".to_string())]
        );
        // a path that misses, or a body that is not JSON, yields nothing
        let mut sources = Vec::new();
        extract_sources(&t, "{\"other\": 1}", &mut sources);
        extract_sources(&t, "plain text", &mut sources);
        assert!(sources.is_empty());
    }

    /// The pre-0.38 config names keep resolving - config CIDs are immutable
    /// on-chain - and both land on the one request tool.
    #[test]
    fn legacy_builtin_names_alias_to_request() {
        assert!(matches!(Builtin::parse("fetch_url"), Some(Builtin::Request)));
        assert!(matches!(Builtin::parse("post_url"), Some(Builtin::Request)));
        assert!(matches!(Builtin::parse("request"), Some(Builtin::Request)));
    }

    /// A built-in cannot be shadowed by an http entry that borrows its name:
    /// the model would call `web_search` and reach something else entirely.
    #[test]
    fn a_config_entry_cannot_shadow_a_builtin() {
        let cfg: ToolsConfig = serde_json::from_value(serde_json::json!({
            "builtin": ["web_search"],
            "http": [{ "name": "web_search", "url": "https://elsewhere/x" }]
        }))
        .unwrap();
        let scfg: crate::search::SearchConfig =
            serde_json::from_value(serde_json::json!({ "provider": "exa" })).unwrap();
        let b = Builtins { search: Some(&scfg), ..Default::default() };
        let reg = build(&cfg, b, &|_| {});
        assert_eq!(reg.tools.len(), 1);
        assert!(matches!(reg.tools[0].src, ToolSrc::Builtin(_)));
        assert_eq!(reg.notes.len(), 1);
    }

    #[test]
    fn the_system_block_carries_the_signatures() {
        let s = system_block(&[tool("get_weather")], Budget::calls(3));
        assert!(s.contains("<tools>"), "{s}");
        assert!(s.contains("\"name\":\"get_weather\""), "{s}");
        assert!(s.contains("at most 3 calls"), "{s}");
        // and the singular reads properly
        assert!(system_block(&[tool("a")], Budget::calls(1)).contains("at most 1 call in"));
    }

    #[test]
    fn json_rpc_decodes_both_framings() {
        let plain = br#"{"jsonrpc":"2.0","id":7,"result":{"ok":true}}"#;
        assert_eq!(decode_rpc(plain, 7).unwrap()["result"]["ok"], true);
        // wrong id is not our answer
        assert!(decode_rpc(plain, 8).is_none());
        let sse = b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"ok\":true}}\n\n";
        assert_eq!(decode_rpc(sse, 7).unwrap()["result"]["ok"], true);
        // a server notification sharing the stream is skipped
        let mixed = b"data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":1}\n\n";
        assert_eq!(decode_rpc(mixed, 7).unwrap()["result"], 1);
    }

    #[test]
    fn mcp_content_renders_text_and_names_the_rest() {
        let r = serde_json::json!({"content":[{"type":"text","text":"hello"},{"type":"image","data":"…"}]});
        assert_eq!(render_mcp_content(&r), "hello\n[image content omitted]");
        let r = serde_json::json!({"content":[],"structuredContent":{"n":1}});
        assert_eq!(render_mcp_content(&r), "{\"n\":1}");
    }

    #[test]
    fn results_come_back_as_a_tool_response_turn() {
        let t = response_turn("a", "it \"worked\"");
        assert!(t.starts_with("<tool_response>"), "{t}");
        assert!(t.ends_with("</tool_response>"), "{t}");
        // the result is JSON-escaped, so a quote in it cannot break the frame
        assert!(t.contains(r#""it \"worked\"""#), "{t}");
    }

    #[test]
    fn long_results_are_truncated_visibly() {
        let s = "x".repeat(100);
        let out = truncate(&s, 10);
        assert!(out.starts_with(&"x".repeat(10)));
        assert!(out.contains("truncated at 10"));
        assert_eq!(truncate("short", 10), "short");
    }

    /// `wait` is the one builtin no config block backs: named, it is offered
    /// whatever else the deployment has, and it is never withheld by the web
    /// switch because nothing about it leaves the enclave.
    #[test]
    fn wait_needs_no_block_and_no_web() {
        let cfg: ToolsConfig =
            serde_json::from_value(serde_json::json!({ "builtin": ["wait", "web_search"] }))
                .unwrap();
        let b = Builtins { search: None, web_withheld: true, ..Default::default() };
        let reg = build(&cfg, b, &|_| {});
        assert!(reg.find("wait").is_some(), "{:?}", reg.notes);
        assert!(reg.find("web_search").is_none());
        assert!(reg.notes.is_empty(), "{:?}", reg.notes);
        assert_eq!(cfg.http_names(), vec!["wait".to_string(), "web_search".to_string()]);
        // the budget fields have their defaults without being written
        assert_eq!(cfg.max_seconds, 3600);
        assert_eq!(cfg.wait_max_s, 600);
        assert_eq!(cfg.keep_results, 3);
    }

    /// The plan is where a wait's arguments are judged: the number can arrive
    /// as a string, a long ask is clamped and told so, and a spent budget is
    /// an answer rather than a sleep.
    #[test]
    fn a_wait_is_clamped_and_told_so() {
        let (s, reason, note) = wait_plan(&serde_json::json!({ "seconds": 30, "reason": "build" }), 600).unwrap();
        assert_eq!((s, reason.as_str(), note.as_str()), (30, "build", ""));
        let (s, _, note) = wait_plan(&serde_json::json!({ "seconds": "45s" }), 600).unwrap();
        assert_eq!(s, 45);
        assert!(note.is_empty());
        // longer than the cap: sleeps the cap, says what it did not do
        let (s, _, note) = wait_plan(&serde_json::json!({ "seconds": 900 }), 600).unwrap();
        assert_eq!(s, 600);
        assert!(note.contains("15 minutes"), "{note}");
        assert!(note.contains("10 minutes"), "{note}");
        assert!(note.contains("call wait again"), "{note}");
        // nothing to wait with
        let e = wait_plan(&serde_json::json!({ "seconds": 5 }), 0).unwrap_err();
        assert!(e.contains("time budget is spent"), "{e}");
        // and nothing to wait for
        assert!(wait_plan(&serde_json::json!({}), 600).is_err());
        assert!(wait_plan(&serde_json::json!({ "seconds": 0 }), 600).is_err());
        assert!(wait_plan(&serde_json::json!({ "seconds": "soon" }), 600).is_err());
    }

    /// A request lowers the deployment's budget and never raises it, and the
    /// two shapes of `loop` mean what they say.
    #[test]
    fn a_request_can_only_lower_the_budget() {
        let cfg: ToolsConfig = serde_json::from_value(serde_json::json!({
            "max_calls": 32, "max_seconds": 1800
        }))
        .unwrap();
        let b = cfg.budget(None);
        assert_eq!(b, Budget { max_calls: 32, max_seconds: 1800, persist: false, max_agents: 0, max_agent_depth: 3 });
        assert!(cfg.budget(Some(&serde_json::json!(true))).persist);
        assert!(!cfg.budget(Some(&serde_json::json!(false))).persist);
        let b = cfg.budget(Some(&serde_json::json!({ "max_calls": 8, "max_seconds": 600 })));
        assert_eq!(b, Budget { max_calls: 8, max_seconds: 600, persist: true, max_agents: 0, max_agent_depth: 3 });
        let b = cfg.budget(Some(&serde_json::json!({ "max_calls": 999, "max_seconds": 99999, "persist": false })));
        assert_eq!(b, Budget { max_calls: 32, max_seconds: 1800, persist: false, max_agents: 0, max_agent_depth: 3 });
        // zero is not a budget: the loop would refuse its first call
        let b = cfg.budget(Some(&serde_json::json!({ "max_calls": 0, "max_seconds": 0 })));
        assert_eq!((b.max_calls, b.max_seconds), (1, 1));
        assert_eq!(human_secs(45), "45 seconds");
        assert_eq!(human_secs(600), "10 minutes");
        assert_eq!(human_secs(5400), "1 hour 30 minutes");
        assert_eq!(human_secs(7200), "2 hours");
    }

    /// spawn_agent exists exactly when the tree says a loop may spawn:
    /// max_agents is the switch, slots are the per-loop fact, and a loop
    /// with none left is silent about it rather than noted as misconfigured.
    #[test]
    fn the_spawn_tool_follows_the_slots() {
        let off: ToolsConfig = serde_json::from_value(serde_json::json!({
            "http": [{ "name": "t", "url": "https://h/x" }]
        }))
        .unwrap();
        let reg = build(&off, Builtins::default(), &|_| {});
        assert!(reg.find(AGENT_TOOL).is_none());
        assert!(!off.http_names().iter().any(|n| n == AGENT_TOOL));
        let on: ToolsConfig = serde_json::from_value(serde_json::json!({
            "max_agents": 4, "http": [{ "name": "t", "url": "https://h/x" }]
        }))
        .unwrap();
        assert_eq!(on.max_agent_depth, 3);
        // advertised, and offered to a loop with slots
        assert!(on.http_names().iter().any(|n| n == AGENT_TOOL));
        let b = Builtins { agent_slots: 4, agent_limit: 4, ..Default::default() };
        let reg = build(&on, b, &|_| {});
        assert!(reg.find(AGENT_TOOL).is_some(), "{:?}", reg.notes);
        assert!(reg.notes.is_empty(), "{:?}", reg.notes);
        // no slots (count spent, or at the depth limit): withdrawn, no note
        let b = Builtins { agent_slots: 0, agent_limit: 4, ..Default::default() };
        let reg = build(&on, b, &|_| {});
        assert!(reg.find(AGENT_TOOL).is_none());
        assert!(reg.notes.is_empty(), "{:?}", reg.notes);
        // named by hand at a deployment without the feature: that IS a note
        let named: ToolsConfig = serde_json::from_value(serde_json::json!({
            "builtin": ["spawn_agent"], "http": [{ "name": "t", "url": "https://h/x" }]
        }))
        .unwrap();
        let reg = build(&named, Builtins::default(), &|_| {});
        assert!(reg.find(AGENT_TOOL).is_none());
        assert!(reg.notes.iter().any(|n| n.contains("max_agents")), "{:?}", reg.notes);
        // the budget carries the limits, and a request may only lower them
        let b = on.budget(Some(&serde_json::json!({ "max_agents": 1, "max_agent_depth": 9 })));
        assert_eq!((b.max_agents, b.max_agent_depth), (1, 3));
        let b = on.budget(Some(&serde_json::json!({ "max_agents": 0 })));
        assert_eq!(b.max_agents, 0);
        // the persisting rules point at it when it is there
        let with = vec![tool("run_tests"), tool(AGENT_TOOL)];
        let rules = system_block(&with, Budget { max_calls: 8, max_seconds: 600, persist: true, max_agents: 4, max_agent_depth: 3 });
        assert!(rules.contains("spawn_agent"), "{rules}");
    }

    /// The rules quote the budget and, only when asked to persist, tell the
    /// model to work to the check - naming wait only when it has one.
    #[test]
    fn the_rules_follow_the_budget() {
        let list = vec![tool("run_tests")];
        let quick = system_block(&list, Budget { max_calls: 32, max_seconds: 1800, persist: false, max_agents: 0, max_agent_depth: 3 });
        assert!(quick.contains("at most 32 calls"), "{quick}");
        assert!(quick.contains("30 minutes of wall-clock time"), "{quick}");
        assert!(quick.contains("stop calling and write the answer"), "{quick}");
        assert!(!quick.contains("WORKING TO A CHECK"), "{quick}");
        let persist = system_block(&list, Budget { max_calls: 32, max_seconds: 1800, persist: true, max_agents: 0, max_agent_depth: 3 });
        assert!(persist.contains("WORKING TO A CHECK"), "{persist}");
        assert!(persist.contains("keep going until the check passes"), "{persist}");
        assert!(!persist.contains("call wait"), "{persist}");
        let with_wait = vec![tool("run_tests"), tool("wait")];
        let persist = system_block(&with_wait, Budget { max_calls: 32, max_seconds: 1800, persist: true, max_agents: 0, max_agent_depth: 3 });
        assert!(persist.contains("call wait rather than polling"), "{persist}");
        // the merged block (client tools beside ours) carries the same rule
        let merged = merged_system_block(&with_wait, Budget { max_calls: 4, max_seconds: 120, persist: true, max_agents: 0, max_agent_depth: 3 }, None);
        assert!(merged.contains("at most 4 of them"), "{merged}");
        assert!(merged.contains("within 2 minutes"), "{merged}");
        assert!(merged.contains("WORKING TO A CHECK"), "{merged}");
    }
}

#[cfg(test)]
mod identity_tests {
    use super::*;

    #[test]
    fn user_slot_fills_or_fails_closed() {
        let mut h = BTreeMap::new();
        h.insert("x-api-key".to_string(), "$JOT_API_KEY".to_string());
        h.insert("x-user".to_string(), "$user".to_string());
        let out = identity_headers(&h, Some("0xabc"), "notes_list").unwrap();
        assert_eq!(out["x-user"], "0xabc");
        assert_eq!(out["x-api-key"], "$JOT_API_KEY", "secrets are the other pass's job");
        let e = identity_headers(&h, None, "notes_list").unwrap_err();
        assert!(e.contains("no signed-in caller") && e.contains("notes_list"), "{e}");
        // a header that merely mentions the word is not the slot
        let mut plain = BTreeMap::new();
        plain.insert("x-note".to_string(), "for $user only".to_string());
        assert_eq!(identity_headers(&plain, None, "t").unwrap()["x-note"], "for $user only");
    }
}
