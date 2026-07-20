//! **Per-client tool renderings** — how a real chat client reshapes an MCP tool
//! surface before it reaches the model.
//!
//! # The problem this module is honest about
//!
//! `jig context` renders the provider API request `jig bench` sends. That number
//! is exact for *that* request, but it is not universal: a chat client sits
//! between the MCP server and the provider, and may rewrite tool names, wrap
//! descriptions, or inject framing of its own. A developer whose tools are
//! consumed through Claude Code or VS Code is paying that client's rendering,
//! not Jig's.
//!
//! # The rule: no invented renderings
//!
//! Every variant here is derived from a **citable public source** — official
//! documentation or open-source code — recorded in [`ClientSpec::citation`] and
//! printed by `jig context --client list`. Where no such source exists, the
//! client is listed as [`Evidence::Unknown`] and **no renderer is implemented**.
//! An honest gap beats a fabricated variant: a plausible-looking guess would be
//! indistinguishable from a measurement, which is the one failure mode this
//! tool cannot afford.
//!
//! Two clients are deliberately *not* implemented for exactly that reason —
//! Claude Desktop and Cursor. Their documentation describes configuration and
//! UI, never the tool rendering, and both are closed-source. The only public
//! claims about them (for instance a widely-repeated Cursor tool-count limit)
//! trace back to community forum posts, which do not establish behaviour.
//!
//! # What a variant does and does not claim
//!
//! Each implemented renderer transforms only what its source establishes —
//! in every verified case so far, the **tool name**. Descriptions and input
//! schemas are passed through untouched because that is what the sources show;
//! none of them is evidence about *system-prompt framing*, per-client tool
//! instructions, or other context the client may add. A variant's token delta
//! is therefore a **lower bound** on the difference from the raw API request,
//! and [`ClientRendering::caveat`] says so in the output.

use serde_json::Value;

/// How well a client's rendering is established by public evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evidence {
    /// Official documentation or open-source code establishes the
    /// transformation exactly.
    Verified,
    /// Public evidence establishes part of the transformation (typically the
    /// tool-name format) but not the whole framing. What is missing is named
    /// explicitly in [`ClientSpec::unestablished`].
    Approximated,
    /// No public source establishes the rendering. **Not implemented** — Jig
    /// reports the gap rather than guessing.
    Unknown,
}

impl Evidence {
    /// A short tag for tables and machine output.
    pub fn tag(self) -> &'static str {
        match self {
            Evidence::Verified => "verified",
            Evidence::Approximated => "approximated",
            Evidence::Unknown => "unknown",
        }
    }

    /// Whether a renderer exists for this level of evidence.
    pub fn is_implemented(self) -> bool {
        !matches!(self, Evidence::Unknown)
    }
}

/// How a client derives the tool name the model sees, from the MCP tool name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NameScheme {
    /// The MCP name verbatim — what a raw provider API request carries.
    Passthrough,
    /// Claude Code: `mcp__<server>__<tool>`.
    ClaudeCode,
    /// VS Code / Copilot: `mcp_<sanitized-server>_<tool>`, with the prefix
    /// capped at 18 characters and the whole id capped at 64.
    VsCode,
}

/// VS Code caps the `mcp_<server>_` prefix at this many characters.
/// (`McpToolName.MaxPrefixLen` in `mcpTypes.ts`.)
const VSCODE_MAX_PREFIX_LEN: usize = 18;
/// VS Code caps the whole generated tool id at this many characters.
/// (`McpToolName.MaxLength` in `mcpTypes.ts`.)
const VSCODE_MAX_LENGTH: usize = 64;
/// The literal `McpToolName.Prefix`.
const VSCODE_PREFIX: &str = "mcp_";

/// A client Jig knows about, and what public evidence says about its rendering.
#[derive(Debug, Clone, Copy)]
pub struct ClientSpec {
    /// The `--client` value.
    pub id: &'static str,
    /// Human-facing name.
    pub label: &'static str,
    /// How well the rendering is established.
    pub evidence: Evidence,
    /// The source establishing it (or, for [`Evidence::Unknown`], the sources
    /// checked that failed to establish it).
    pub citation: &'static str,
    /// One line on what the rendering does.
    pub summary: &'static str,
    /// What public evidence does **not** establish. `None` only for the raw API
    /// rendering, which is Jig's own request and needs no external evidence.
    pub unestablished: Option<&'static str>,
    /// The name transformation, when implemented.
    name_scheme: Option<NameScheme>,
}

/// Every client `jig context --client list` reports, implemented or not.
///
/// Order is deliberate: the default first, then implemented variants, then the
/// honest gaps.
pub const CLIENTS: &[ClientSpec] = &[
    ClientSpec {
        id: "api",
        label: "Raw provider API",
        evidence: Evidence::Verified,
        citation: "jig's own request — the body `jig bench` sends, rendered by \
                   jig_core::bench::render_request_parts",
        summary: "tool names, descriptions and schemas exactly as the server sent them",
        unestablished: None,
        name_scheme: Some(NameScheme::Passthrough),
    },
    ClientSpec {
        id: "claude-code",
        label: "Claude Code",
        evidence: Evidence::Approximated,
        citation: "https://code.claude.com/docs/en/hooks — \"MCP tools follow the naming \
                   pattern `mcp__<server>__<tool>`\"",
        summary: "prefixes each tool name with `mcp__<server>__`",
        unestablished: Some(
            "whether the description or input schema is rewritten, and what system-prompt \
             framing is added — Claude Code is closed-source, so only the name format is citable",
        ),
        name_scheme: Some(NameScheme::ClaudeCode),
    },
    ClientSpec {
        id: "vscode",
        label: "VS Code / GitHub Copilot",
        evidence: Evidence::Verified,
        citation: "microsoft/vscode, src/vs/workbench/contrib/mcp/common/mcpTypes.ts \
                   (McpToolName.Prefix='mcp_', MaxPrefixLen=18, MaxLength=64) and \
                   mcpLanguageModelToolContribution.ts (modelDescription = definition.description, \
                   inputSchema = definition.inputSchema)",
        summary: "prefixes names as `mcp_<server>_`, prefix capped at 18 chars and id at 64; \
                  description and schema pass through unmodified",
        unestablished: Some(
            "the final serialization into the provider HTTP payload happens in the Copilot \
             extension layer, which is not open-source — the name transform and the \
             description/schema pass-through are verified, the last hop is not",
        ),
        name_scheme: Some(NameScheme::VsCode),
    },
    ClientSpec {
        id: "openai-agents",
        label: "OpenAI Agents SDK (Python)",
        evidence: Evidence::Verified,
        citation: "openai/openai-agents-python, src/agents/mcp/util.py — \
                   `include_server_in_tool_names` defaults to False, so `tool_public_name` is \
                   the raw MCP tool name; description and schema are passed through",
        summary: "no transformation by default — identical to the raw API rendering",
        unestablished: Some(
            "the opt-in `include_server_in_tool_names=True` path yields `mcp_<server>__<tool>`; \
             jig renders the documented default, not the opt-in",
        ),
        name_scheme: Some(NameScheme::Passthrough),
    },
    ClientSpec {
        id: "claude-desktop",
        label: "Claude Desktop",
        evidence: Evidence::Unknown,
        citation: "checked https://modelcontextprotocol.io/docs/develop/connect-local-servers \
                   and Anthropic's Claude Desktop documentation: both cover configuration, \
                   server startup and the approval UI, and state nothing about tool naming or \
                   rendering. Closed-source.",
        summary: "not established — no renderer implemented",
        unestablished: Some("the entire rendering"),
        name_scheme: None,
    },
    ClientSpec {
        id: "cursor",
        label: "Cursor",
        evidence: Evidence::Unknown,
        citation: "checked https://cursor.com/docs/context/mcp and \
                   https://cursor.com/docs/agent/tools: neither documents a name transformation \
                   nor a tool-definition limit. The widely-repeated \"40 tool limit\" appears \
                   only in community forum threads, which do not establish behaviour. \
                   Closed-source.",
        summary: "not established — no renderer implemented",
        unestablished: Some("the entire rendering"),
        name_scheme: None,
    },
];

/// The default `--client` value: today's behaviour, the raw provider API request.
pub const DEFAULT_CLIENT: &str = "api";

/// Look up a client spec by its `--client` id (case-insensitive).
pub fn spec(id: &str) -> Option<&'static ClientSpec> {
    let needle = id.trim().to_ascii_lowercase();
    CLIENTS.iter().find(|c| c.id == needle)
}

/// The ids of every client Jig knows about, for help text and errors.
pub fn known_clients() -> Vec<&'static str> {
    CLIENTS.iter().map(|c| c.id).collect()
}

/// Why a `--client` value could not be rendered.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The id is not in [`CLIENTS`].
    #[error("unknown client '{client}' (known: {known}); `jig context --client list` describes each one")]
    Unknown {
        /// The unrecognized id.
        client: String,
        /// Comma-separated known ids.
        known: String,
    },
    /// The client is known, but no public source establishes its rendering, so
    /// Jig refuses to invent one.
    #[error(
        "no rendering is implemented for '{client}': {reason}. \
         Jig will not guess a rendering it cannot cite — run `jig context --client list` for \
         what was checked, or use --client api for the raw provider request"
    )]
    NotEstablished {
        /// The requested id.
        client: &'static str,
        /// What was checked and why it is not enough.
        reason: &'static str,
    },
}

/// A tool name as one client renders it, alongside the MCP name it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedName {
    /// The name the MCP server declared.
    pub mcp: String,
    /// The name this client presents to the model.
    pub rendered: String,
}

impl RenderedName {
    /// Whether this client changed the name at all.
    pub fn is_changed(&self) -> bool {
        self.mcp != self.rendered
    }
}

/// One client's rendering of a tool surface: the transformed names plus the
/// provenance needed to report it honestly.
#[derive(Debug, Clone)]
pub struct ClientRendering {
    /// The spec this rendering came from.
    pub spec: &'static ClientSpec,
    /// The server name the prefix was derived from (empty for passthrough
    /// schemes, which do not use it).
    pub server_name: String,
    /// Per-tool names, in the order the tools were given.
    pub names: Vec<RenderedName>,
}

impl ClientRendering {
    /// Whether this rendering leaves the tool surface byte-identical to the raw
    /// API request.
    pub fn is_identity(&self) -> bool {
        self.names.iter().all(|n| !n.is_changed())
    }

    /// The honesty caveat printed with any non-`api` rendering: a token delta
    /// covers only what the citation establishes, so it is a lower bound.
    pub fn caveat(&self) -> Option<&'static str> {
        self.spec.unestablished
    }
}

/// Render `tools`' names as `client` would present them, given the MCP server's
/// own name (used for the prefix schemes).
///
/// # Errors
///
/// [`ClientError::Unknown`] for an unrecognized id, or
/// [`ClientError::NotEstablished`] for a client whose rendering no public source
/// establishes — Jig refuses to guess rather than emitting a plausible fiction.
pub fn render_names(
    client: &str,
    server_name: &str,
    tool_names: &[String],
) -> Result<ClientRendering, ClientError> {
    let spec = spec(client).ok_or_else(|| ClientError::Unknown {
        client: client.to_string(),
        known: known_clients().join(", "),
    })?;

    let scheme = spec.name_scheme.ok_or(ClientError::NotEstablished {
        client: spec.id,
        reason: spec.citation,
    })?;

    let names = tool_names
        .iter()
        .map(|mcp| RenderedName {
            mcp: mcp.clone(),
            rendered: apply_scheme(scheme, server_name, mcp),
        })
        .collect();

    Ok(ClientRendering {
        spec,
        server_name: server_name.to_string(),
        names,
    })
}

/// Apply one name scheme. Pure and total — never panics on any input.
fn apply_scheme(scheme: NameScheme, server: &str, tool: &str) -> String {
    match scheme {
        NameScheme::Passthrough => tool.to_string(),
        NameScheme::ClaudeCode => {
            // Documented form: `mcp__<server>__<tool>`, with any character
            // outside `A-Za-z0-9_-` replaced by `_`.
            format!("mcp__{}__{}", sanitize_claude_code(server), tool)
        }
        NameScheme::VsCode => {
            // `McpPrefixGenerator.take()`: lowercase, non-`[a-z0-9_.-]` runs
            // collapse to `_`, the server segment is cut so the whole prefix
            // fits MaxPrefixLen, then a `_` suffix; the id is `.`->`_` and
            // truncated to MaxLength.
            let safe = sanitize_vscode(server);
            // Prefix budget: "mcp_" + <server> + "_" must be <= 18.
            let max_server = VSCODE_MAX_PREFIX_LEN
                .saturating_sub(VSCODE_PREFIX.len())
                .saturating_sub(1);
            let server_part: String = safe.chars().take(max_server).collect();
            let id = format!("{VSCODE_PREFIX}{server_part}_{}", tool.replace('.', "_"));
            id.chars().take(VSCODE_MAX_LENGTH).collect()
        }
    }
}

/// Claude Code's documented sanitization: any character outside `A-Za-z0-9_-`
/// becomes `_`.
fn sanitize_claude_code(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// VS Code's `safeName`: lowercase, then every run of characters outside
/// `[a-z0-9_.-]` collapses to a single `_`.
fn sanitize_vscode(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut in_run = false;
    for c in lower.chars() {
        let ok = c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.' || c == '-';
        if ok {
            out.push(c);
            in_run = false;
        } else if !in_run {
            out.push('_');
            in_run = true;
        }
    }
    out
}

/// Rewrite the `tools` array of an already-rendered provider request body so
/// each tool carries its client-rendered name.
///
/// Only the name is touched, because only the name is what the citations
/// establish. Both provider dialects are handled: Anthropic's flat
/// `{name, description, input_schema}` and OpenAI's
/// `{type: function, function: {name, ...}}`.
///
/// Index-aligned with `rendering.names`, which is built from the same tool slice
/// in the same order.
pub fn apply_to_request_body(body: &mut Value, rendering: &ClientRendering) {
    let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };
    for (entry, name) in tools.iter_mut().zip(&rendering.names) {
        // OpenAI nests the tool under `function`; Anthropic is flat.
        let target = match entry.get_mut("function") {
            Some(f) if f.is_object() => f,
            _ => entry,
        };
        if let Some(obj) = target.as_object_mut() {
            obj.insert("name".to_string(), Value::String(name.rendered.clone()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn api_is_the_default_and_the_identity() {
        assert_eq!(DEFAULT_CLIENT, "api");
        let r = render_names("api", "my-server", &names(&["echo", "make_reservation"])).unwrap();
        assert!(r.is_identity());
        assert_eq!(r.names[0].rendered, "echo");
        assert!(r.caveat().is_none(), "the raw API request needs no caveat");
    }

    #[test]
    fn claude_code_prefixes_with_server_and_double_underscores() {
        let r = render_names("claude-code", "filesystem", &names(&["read_file"])).unwrap();
        assert_eq!(r.names[0].rendered, "mcp__filesystem__read_file");
        assert!(!r.is_identity());
        // Approximated, not verified: the framing beyond the name is uncited.
        assert_eq!(r.spec.evidence, Evidence::Approximated);
        assert!(r.caveat().is_some());
    }

    #[test]
    fn claude_code_sanitizes_characters_outside_the_documented_set() {
        let r = render_names("claude-code", "my server.v2", &names(&["t"])).unwrap();
        assert_eq!(r.names[0].rendered, "mcp__my_server_v2__t");
    }

    #[test]
    fn vscode_caps_the_prefix_at_eighteen_characters() {
        let r = render_names(
            "vscode",
            "an-extremely-long-server-name",
            &names(&["do_thing"]),
        )
        .unwrap();
        let rendered = &r.names[0].rendered;
        // "mcp_" + 13 server chars + "_" == exactly MaxPrefixLen.
        let prefix_len = rendered.len() - "do_thing".len();
        assert_eq!(prefix_len, VSCODE_MAX_PREFIX_LEN, "got {rendered}");
        assert_eq!(rendered, "mcp_an-extremely-_do_thing");
    }

    #[test]
    fn vscode_truncates_the_whole_id_at_sixty_four() {
        let long_tool = "t".repeat(200);
        let r = render_names("vscode", "srv", &names(&[&long_tool])).unwrap();
        assert_eq!(r.names[0].rendered.chars().count(), VSCODE_MAX_LENGTH);
    }

    #[test]
    fn vscode_lowercases_and_collapses_unsafe_runs() {
        let r = render_names("vscode", "My Server!!Name", &names(&["a.b"])).unwrap();
        // "My Server!!Name" -> "my_server_name" (the `!!` run collapses to one
        // `_`), cut to the 13-character server budget (18 - "mcp_" - "_") as
        // "my_server_nam", then the tool's `.` becomes `_`.
        assert_eq!(r.names[0].rendered, "mcp_my_server_nam_a_b");
    }

    #[test]
    fn openai_agents_default_is_the_identity_and_says_so() {
        let r = render_names("openai-agents", "srv", &names(&["echo"])).unwrap();
        assert!(r.is_identity());
        assert_eq!(r.spec.evidence, Evidence::Verified);
        // Verified *and* identical is a real finding, not a missing feature.
        assert!(r.caveat().unwrap().contains("include_server_in_tool_names"));
    }

    #[test]
    fn unknown_clients_are_refused_not_guessed() {
        for id in ["claude-desktop", "cursor"] {
            let spec = spec(id).unwrap();
            assert_eq!(spec.evidence, Evidence::Unknown);
            assert!(!spec.evidence.is_implemented());
            let err = render_names(id, "srv", &names(&["echo"])).unwrap_err();
            assert!(
                matches!(err, ClientError::NotEstablished { .. }),
                "a client with no citable rendering must be refused: {err}"
            );
            // The refusal carries the evidence trail, not a bare "unsupported".
            assert!(err.to_string().contains("checked"));
        }
    }

    #[test]
    fn an_unrecognized_id_lists_the_known_ones() {
        let err = render_names("emacs", "srv", &names(&["echo"])).unwrap_err();
        assert!(matches!(err, ClientError::Unknown { .. }));
        let msg = err.to_string();
        assert!(msg.contains("api") && msg.contains("vscode"));
    }

    #[test]
    fn every_spec_is_internally_consistent() {
        for c in CLIENTS {
            assert!(!c.citation.is_empty(), "{} has no citation", c.id);
            assert_eq!(
                c.evidence.is_implemented(),
                c.name_scheme.is_some(),
                "{}: evidence and implementation status disagree",
                c.id
            );
            // Only the raw API rendering may omit a caveat.
            assert_eq!(
                c.unestablished.is_none(),
                c.id == "api",
                "{}: caveat presence is wrong",
                c.id
            );
        }
    }

    #[test]
    fn body_rewrite_handles_both_provider_dialects() {
        let r = render_names("claude-code", "srv", &names(&["echo", "other"])).unwrap();

        // Anthropic: flat tool objects.
        let mut anthropic = json!({
            "tools": [
                { "name": "echo", "description": "d", "input_schema": {} },
                { "name": "other", "input_schema": {} }
            ]
        });
        apply_to_request_body(&mut anthropic, &r);
        assert_eq!(anthropic["tools"][0]["name"], "mcp__srv__echo");
        assert_eq!(anthropic["tools"][1]["name"], "mcp__srv__other");
        // Everything else is untouched — only the name is cited.
        assert_eq!(anthropic["tools"][0]["description"], "d");

        // OpenAI: nested under `function`.
        let mut openai = json!({
            "tools": [
                { "type": "function", "function": { "name": "echo", "parameters": {} } },
                { "type": "function", "function": { "name": "other", "parameters": {} } }
            ]
        });
        apply_to_request_body(&mut openai, &r);
        assert_eq!(openai["tools"][0]["function"]["name"], "mcp__srv__echo");
        assert_eq!(openai["tools"][1]["function"]["name"], "mcp__srv__other");
    }

    #[test]
    fn body_rewrite_is_total_over_odd_bodies() {
        let r = render_names("claude-code", "srv", &names(&["echo"])).unwrap();
        // No tools key, wrong type, and a non-object entry: all no-ops.
        for mut body in [
            json!({}),
            json!({ "tools": "nope" }),
            json!({ "tools": [1] }),
        ] {
            apply_to_request_body(&mut body, &r);
        }
    }

    #[test]
    fn name_schemes_are_total_over_empty_and_unicode_input() {
        for server in ["", "日本語", "----", "a"] {
            for client in ["api", "claude-code", "vscode", "openai-agents"] {
                let r = render_names(client, server, &names(&["", "t"])).unwrap();
                assert_eq!(r.names.len(), 2);
            }
        }
    }
}
