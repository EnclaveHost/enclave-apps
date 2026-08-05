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
    /// Capabilities this deployment ALREADY has, handed to the model as tools
    /// instead of being decided for it by a pre-pass: `["web_search",
    /// "request", "generate_image", "view_image"]`. Each is backed by its own
    /// config block and never appears without it: web_search and request by
    /// `search` (the block where a deployment consents to the model reaching
    /// the web at all - and request can SEND data out, which deserves that
    /// gate more than reading does), generate_image by `image`, view_image by
    /// `vision_service`. The legacy names fetch_url and post_url still parse -
    /// on-chain config CIDs are immutable - and both resolve to `request`.
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
    /// plain HTTP endpoints, described here in full
    #[serde(default)]
    pub http: Vec<HttpTool>,
    /// MCP servers, whose tools are discovered (or declared inline)
    #[serde(default)]
    pub mcp: Vec<McpServer>,
}

/// The capabilities a built-in tool is wired to. Passed in rather than read
/// from config here, because they belong to the app, not to the tools block:
/// `web_search` IS the search leg, with the deployment's provider and key;
/// `generate_image` IS the image leg; `view_image` IS the vision delegation.
#[derive(Clone, Copy, Default)]
pub struct Builtins<'a> {
    pub search: Option<&'a crate::search::SearchConfig>,
    /// the CLIENT withheld the web for this turn (the playground's search
    /// switch is off). Web-backed builtins are then skipped silently: not
    /// showing the model a tool is a stronger guarantee than asking it not to
    /// use one, and a user's choice is not a misconfiguration to report.
    pub web_withheld: bool,
    /// the image-generation leg, when configured AND live for this turn: the
    /// user's image switch governs the tool exactly as it governs the router
    pub image: Option<&'a crate::image::ImageConfig>,
    /// image is configured but the user's switch is off this turn - skipped
    /// silently, same stance as web_withheld
    pub image_withheld: bool,
    /// the vision delegation leg, when configured and this turn DELEGATES
    /// (a serving model reading the picture itself keeps it local; there is
    /// nothing for a tool to do)
    pub vision: Option<&'a crate::vision::VisionConfig>,
    /// vision_service is configured but this turn reads the picture locally -
    /// skipped silently, the capability is not missing
    pub vision_withheld: bool,
    /// the conversation carries at least one attached image. Without one,
    /// view_image is silently not offered: a tool with nothing to look at is
    /// prompt noise, not a misconfiguration.
    pub images_present: bool,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Builtin {
    WebSearch,
    Request,
    GenerateImage,
    ViewImage,
}

impl Builtin {
    fn parse(name: &str) -> Option<Builtin> {
        match name.trim() {
            "web_search" => Some(Builtin::WebSearch),
            // fetch_url / post_url are the pre-0.38 names for what is now ONE
            // request tool. Config CIDs are immutable on-chain, so the old
            // names must keep resolving forever.
            "request" | "fetch_url" | "post_url" => Some(Builtin::Request),
            "generate_image" => Some(Builtin::GenerateImage),
            "view_image" => Some(Builtin::ViewImage),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Builtin::WebSearch => "web_search",
            Builtin::Request => "request",
            Builtin::GenerateImage => "generate_image",
            Builtin::ViewImage => "view_image",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Builtin::WebSearch =>
                "Search the web and get back numbered results with page text. Use it for any \
                 fact about the world you cannot verify from this conversation, including ones \
                 you believe you remember. Cite what you use as [1], [2].",
            Builtin::Request =>
                "Send an HTTP request to a URL and return the response text. GET (the default) \
                 reads a page or API - use it to read a web_search result in full, a URL the \
                 user gave you, or a JSON endpoint. POST/PUT/PATCH/DELETE send `body` to an \
                 API or webhook the user pointed you at - tell the user what you sent and \
                 where.",
            Builtin::GenerateImage =>
                "Generate an image from a text prompt. The picture is shown to the user \
                 directly, above your reply; you never see it, so acknowledge it briefly \
                 rather than describing it. Write the prompt as a full visual description of \
                 the desired picture.",
            Builtin::ViewImage =>
                "Look at the image(s) the user attached and answer one question about them. \
                 The vision model sees ONLY the image and your question, not the conversation, \
                 so make the question self-contained and ask for exact transcription when \
                 text, numbers or labels matter.",
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
            Builtin::GenerateImage => serde_json::json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "full visual description of the image to generate",
                    }
                },
                "required": ["prompt"],
            }),
            Builtin::ViewImage => serde_json::json!({
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "one self-contained question about the attached image(s)",
                    }
                },
                "required": ["question"],
            }),
        }
    }

    /// Which config block has to be present for this to be offered at all.
    fn available(self, b: &Builtins) -> bool {
        match self {
            Builtin::WebSearch | Builtin::Request => b.search.is_some(),
            Builtin::GenerateImage => b.image.is_some(),
            Builtin::ViewImage => b.vision.is_some() && b.images_present,
        }
    }

    /// An unavailable builtin that is a CHOICE, not a misconfiguration: the
    /// user's switch withheld it, or there is simply no image this turn.
    /// Skipped without a note.
    fn withheld(self, b: &Builtins) -> bool {
        match self {
            Builtin::WebSearch | Builtin::Request => b.web_withheld,
            Builtin::GenerateImage => b.image_withheld,
            Builtin::ViewImage => {
                b.vision_withheld || (b.vision.is_some() && !b.images_present)
            }
        }
    }

    /// What is missing when it is neither available nor deliberately withheld.
    fn missing(self) -> &'static str {
        match self {
            Builtin::WebSearch | Builtin::Request => "`search`",
            Builtin::GenerateImage => "`image`",
            Builtin::ViewImage => "`vision_service`",
        }
    }
}

fn default_max_calls() -> usize {
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

    /// The HTTP tool names a turn would actually be offered - the same name
    /// check and first-wins deduplication `build` applies, without dialling
    /// anything. /models advertises these, and advertising a name that
    /// resolution then drops would put a tool in the UI that cannot be called.
    /// MCP names are deliberately absent: knowing them means a round trip.
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
}

/// One MCP server reachable over the streamable-HTTP transport.
#[derive(Deserialize, Clone)]
pub struct McpServer {
    /// the MCP endpoint, e.g. "https://<id8>.app.enclave.host/mcp"
    pub url: String,
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
                 generate_image, view_image)"
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
    for (i, t) in cfg.http.iter().enumerate() {
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
pub fn system_block(tools: &[Tool], max_calls: usize) -> String {
    let mut s = signatures(tools);
    s.push_str(&format!(
        "Rules for this app: the call is executed by the server and its result comes back in a \
         <tool_response> block; wait for it rather than inventing one. Call ONLY the functions \
         listed above, by their exact names - nothing else exists, and a call to anything else \
         is shown to the user as a failure. You may make at most {max_calls} call{} in one \
         answer, so make each one count. When a call fails, say so plainly and answer from what \
         you have. When you have enough to answer, stop calling and write the answer.",
        if max_calls == 1 { "" } else { "s" }
    ));
    s
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
    match require {
        Some("") => s.push_str(
            "\n\nFor THIS turn you MUST respond with a tool call, not a prose answer.",
        ),
        Some(name) => s.push_str(&format!(
            "\n\nFor THIS turn you MUST call `{name}`, not answer in prose.",
        )),
        None => {}
    }
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

/// Pull tool calls out of a reply.
///
/// Tolerant on purpose, because the failure mode is expensive: a call this
/// misses is shown to the user as raw JSON instead of being run. Accepts the
/// trained `<tool_call>{...}</tool_call>` form (including one left unterminated
/// by the stop string), a ```json fence, and a bare object that is nothing but
/// `{"name": ..., "arguments": ...}`. Reasoning inside a <think> block is
/// skipped: a model that talks through calling a tool has not called it.
pub fn parse_calls(text: &str) -> Vec<ToolCall> {
    let body = after_think(text);
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(i) = rest.find("<tool_call>") {
        let after = &rest[i + "<tool_call>".len()..];
        let (chunk, tail) = match after.find("</tool_call>") {
            Some(j) => (&after[..j], &after[j + "</tool_call>".len()..]),
            None => (after, ""),
        };
        if let Some(c) = one_call(chunk) {
            out.push(c);
        }
        rest = tail;
        if tail.is_empty() {
            break;
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
        if let Some(c) = one_call(inner) {
            return vec![c];
        }
    }
    Vec::new()
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
/// delivered as assistant text and every retry copies it verbatim.
fn one_call(chunk: &str) -> Option<ToolCall> {
    let t = chunk.trim();
    let t = strip_fence(t).unwrap_or(t);
    let v: serde_json::Value = serde_json::from_str(t).ok().or_else(|| {
        // a trailing sentence after the object is common; take the balanced
        // prefix that parses
        let end = balanced_end(t)?;
        serde_json::from_str(&t[..end]).ok()
    })?;
    let mut args = match v.get("arguments").or_else(|| v.get("parameters")) {
        Some(serde_json::Value::String(s)) => {
            serde_json::from_str(s).unwrap_or(serde_json::Value::String(s.clone()))
        }
        Some(a) => a.clone(),
        None => serde_json::json!({}),
    };
    let name = match v.get("name").and_then(|n| n.as_str()) {
        Some(n) => n.trim().to_string(),
        // no top-level name: pull it OUT of the arguments, where a `name` key
        // can only be the function name the model misplaced
        None => {
            let o = args.as_object_mut()?;
            let n = o.get("name")?.as_str()?.trim().to_string();
            o.remove("name");
            n
        }
    };
    if name.is_empty() {
        return None;
    }
    Some(ToolCall { name, args })
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
            let known: Vec<&str> = reg.tools.iter().map(|t| t.name.as_str()).collect();
            return ToolResult {
                text: format!(
                    "there is no tool named '{name}' on this deployment. Available: {}",
                    if known.is_empty() { "(none)".into() } else { known.join(", ") }
                ),
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
        ToolSrc::Builtin(k) => {
            call_builtin(k, &b, args, images, &mut sources, &mut image, &now_ms, on_status)
        }
        ToolSrc::Http(i) => call_http(&cfg.http[i], cfg, args),
        ToolSrc::Mcp { server, remote } => call_mcp(&mut reg.mcp[server], &remote, args),
        // never built into a Registry - the passthrough renders client tools
        // into the prompt and hands the call back, so reaching this arm is a
        // wiring bug, and the failure must say which side executes
        ToolSrc::Client => Err("client-declared tools are executed by the client, not here".into()),
    };
    let (text, is_error) = match r {
        Ok(t) => (truncate(&t, max_chars), false),
        Err(e) => (e, true),
    };
    if is_error {
        sources.clear();
        image = None;
    }
    ToolResult { text, is_error, ms: now_ms().saturating_sub(t0), sources, image }
}

/// The app's own capabilities, called the way any other tool is.
///
/// web_search renders EXACTLY what the pre-pass renders (search::render_context),
/// so the numbering the model cites is the numbering it has always cited and
/// the answer path needs no second convention. generate_image and view_image
/// run the same legs the router and the vision pre-pass run, for the same
/// reason: one implementation per capability, whoever decided to use it.
#[allow(clippy::too_many_arguments)]
fn call_builtin(
    k: Builtin,
    b: &Builtins,
    args: &serde_json::Value,
    images: &[Vec<u8>],
    sources: &mut Vec<(String, String)>,
    image_out: &mut Option<crate::image::GeneratedImage>,
    now_ms: &impl Fn() -> u64,
    on_status: &dyn Fn(&str),
) -> Result<String, String> {
    match k {
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
        Builtin::GenerateImage => {
            let icfg = b.image.ok_or("image generation is not configured on this deployment")?;
            let prompt = args
                .get("prompt")
                .and_then(|p| p.as_str())
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .ok_or("generate_image needs a non-empty `prompt` string")?;
            let img = crate::image::generate(icfg, prompt, now_ms, on_status)?;
            let text = format!(
                "An image has been generated from the prompt \"{}\" and is displayed to the \
                 user directly above your reply. You cannot see it. Acknowledge it briefly \
                 and naturally, and offer to adjust it; do not describe details you cannot \
                 verify, and do not call generate_image again unless the user wants a \
                 different picture.",
                img.prompt
            );
            *image_out = Some(img);
            Ok(text)
        }
        Builtin::ViewImage => {
            let vcfg = b.vision.ok_or("vision delegation is not configured on this deployment")?;
            if images.is_empty() {
                return Err("there is no attached image in this conversation to look at".into());
            }
            let q = args
                .get("question")
                .and_then(|q| q.as_str())
                .map(str::trim)
                .filter(|q| !q.is_empty())
                .ok_or("view_image needs a non-empty `question` string")?;
            let a = crate::vision::describe(vcfg, images, q, None, now_ms, on_status)?;
            Ok(a.text)
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

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}\n[truncated at {max} characters]")
}

fn call_http(t: &HttpTool, cfg: &ToolsConfig, args: &serde_json::Value) -> Result<String, String> {
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
            Some(tpl) => fill_template(tpl, obj),
            None if as_query => serde_json::json!({}),
            None => args.clone(),
        };
        Some(payload.to_string().into_bytes())
    };

    let mut req = HttpReq::get(&url);
    req.method = method;
    req.timeout_s = t.timeout_s.unwrap_or(cfg.timeout_s);
    req.max_bytes = t.max_bytes.unwrap_or(cfg.max_bytes);
    req.body = body.as_deref();
    req = req.header("accept", b"application/json, text/plain;q=0.9, */*;q=0.8");
    if body.is_some() {
        req = req.header("content-type", b"application/json");
    }
    let mut notes = Vec::new();
    for (k, v) in resolved_headers(&t.headers, &mut notes, &t.url) {
        req = req.header(&k, v.as_bytes());
    }
    if let Some(n) = notes.first() {
        return Err(n.clone());
    }

    let r = http::request(req)?;
    let text = String::from_utf8_lossy(&r.body).trim().to_string();
    if r.status >= 400 {
        let hint: String = text.chars().take(400).collect();
        return Err(format!("tool '{}' returned HTTP {}: {hint}", t.name, r.status));
    }
    if r.truncated {
        return Ok(format!("{text}\n[response was cut off at {} bytes]", req_max(t, cfg)));
    }
    Ok(text)
}

fn req_max(t: &HttpTool, cfg: &ToolsConfig) -> usize {
    t.max_bytes.unwrap_or(cfg.max_bytes)
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
            "clientInfo": { "name": "llm-chat", "version": env!("CARGO_PKG_VERSION") },
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
                "clientInfo": { "name": "llm-chat", "version": env!("CARGO_PKG_VERSION") },
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

    /// generate_image and view_image ride their own config blocks, and
    /// view_image only exists on a turn that actually carries a picture.
    #[test]
    fn image_and_vision_builtins_follow_their_legs() {
        let cfg: ToolsConfig = serde_json::from_value(serde_json::json!({
            "builtin": ["generate_image", "view_image"]
        }))
        .unwrap();
        let icfg: crate::image::ImageConfig =
            serde_json::from_value(serde_json::json!({ "endpoint": "https://img.example" }))
                .unwrap();
        let vcfg: crate::vision::VisionConfig =
            serde_json::from_value(serde_json::json!({ "endpoint": "https://eyes.example" }))
                .unwrap();
        // both configured, image attached: both offered
        let b = Builtins {
            image: Some(&icfg),
            vision: Some(&vcfg),
            images_present: true,
            ..Default::default()
        };
        let reg = build(&cfg, b, &|_| {});
        let names: Vec<&str> = reg.tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["generate_image", "view_image"]);
        assert!(reg.notes.is_empty(), "{:?}", reg.notes);
        // no picture this turn: view_image vanishes silently
        let b = Builtins {
            image: Some(&icfg),
            vision: Some(&vcfg),
            images_present: false,
            ..Default::default()
        };
        let reg = build(&cfg, b, &|_| {});
        let names: Vec<&str> = reg.tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["generate_image"]);
        assert!(reg.notes.is_empty(), "{:?}", reg.notes);
        // the user's image switch withholds generate_image silently too
        let b = Builtins {
            image: None,
            image_withheld: true,
            vision: Some(&vcfg),
            images_present: true,
            ..Default::default()
        };
        let reg = build(&cfg, b, &|_| {});
        let names: Vec<&str> = reg.tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["view_image"]);
        assert!(reg.notes.is_empty(), "{:?}", reg.notes);
        // whereas blocks that are simply absent ARE reported
        let reg = build(&cfg, Builtins::default(), &|_| {});
        assert!(reg.is_empty());
        assert_eq!(reg.notes.len(), 2, "{:?}", reg.notes);
        assert!(reg.notes.iter().any(|n| n.contains("`image` block")), "{:?}", reg.notes);
        assert!(
            reg.notes.iter().any(|n| n.contains("`vision_service` block")),
            "{:?}",
            reg.notes
        );
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
        let s = system_block(&[tool("get_weather")], 3);
        assert!(s.contains("<tools>"), "{s}");
        assert!(s.contains("\"name\":\"get_weather\""), "{s}");
        assert!(s.contains("at most 3 calls"), "{s}");
        // and the singular reads properly
        assert!(system_block(&[tool("a")], 1).contains("at most 1 call in"));
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
}
