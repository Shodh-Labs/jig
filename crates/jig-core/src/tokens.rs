//! The **token-budget engine**: what a server's tool surface costs in context
//! tokens, per tool and per model, before the user types a word.
//!
//! # Accuracy honesty
//!
//! Every count carries an [`Exactness`] flag. Two provider families are handled
//! very differently:
//!
//! * **OpenAI — exact.** The `tiktoken` BPE tokenizers are the *actual*
//!   tokenizers OpenAI ships. `o200k_base` (gpt-4o / o-series lineage) and
//!   `cl100k_base` (gpt-4 / 3.5 lineage) produce exact counts, labelled
//!   [`Exactness::Exact`].
//!
//! * **Anthropic — labelled approximation (+ optional exact).** Anthropic does
//!   not publish a local tokenizer for Claude 3+. By default Jig approximates a
//!   Claude count with the `o200k_base` tokenizer and labels it
//!   `~approx` everywhere ([`Exactness::Approximate`], carrying the method
//!   string). With `--exact-anthropic` and `ANTHROPIC_API_KEY` set, the CLI
//!   calls Anthropic's `count_tokens` endpoint for an exact **grand total**
//!   (the endpoint reports a request-level total, not a per-tool breakdown, so
//!   the per-tool rows stay `~approx` while the total is labelled exact).
//!
//! # What gets counted — the canonical rendering
//!
//! See [`CANONICAL_RENDERING_DOC`]. In short: for each tool Jig counts the
//! compact JSON `{name, description?, input_schema}` a client transmits, plus
//! the server's `instructions` string if present. The rendering step is a
//! single function ([`canonical_tool_json`]) so client-specific variants can be
//! swapped in later.
//!
//! # Extending the model registry
//!
//! Adding a model is one entry in the private `REGISTRY` table in this
//! module. Each entry names the tiktoken
//! encoding to use, whether the count is an Anthropic approximation, and (for
//! `--exact-anthropic`) the concrete Anthropic API model id to bill the
//! `count_tokens` call against.

#[cfg(feature = "exact-anthropic")]
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::protocol::Tool;

/// A precise, human-readable definition of Jig's V1 canonical rendering.
///
/// Emitted verbatim in `--json` output and referenced in the README so the
/// number is never a black box.
pub const CANONICAL_RENDERING_DOC: &str = "V1 canonical rendering: for each tool, a compact \
    (no insignificant whitespace) JSON object with keys {name, description, input_schema} — \
    description is omitted when the tool declares none — serialized with lexicographically \
    sorted keys for determinism. A tool's token count is the token count of that JSON string. \
    The server's `instructions` string, if present, is counted verbatim. The grand total is \
    the sum of every per-tool count plus the instructions count.";

/// How a token count was produced — the exactness contract attached to every
/// number Jig reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exactness {
    /// An exact count from the provider's real tokenizer (or official API).
    Exact,
    /// A labelled approximation. `method` documents exactly how it was derived.
    Approximate {
        /// Human-readable description of the approximation method.
        method: String,
    },
}

impl Exactness {
    /// Whether this count is exact.
    pub fn is_exact(&self) -> bool {
        matches!(self, Exactness::Exact)
    }

    /// The approximation method, if this count is approximate.
    pub fn method(&self) -> Option<&str> {
        match self {
            Exactness::Approximate { method } => Some(method),
            Exactness::Exact => None,
        }
    }

    /// A short tag suitable for inline annotation: `exact` or `~approx`.
    pub fn tag(&self) -> &'static str {
        if self.is_exact() {
            "exact"
        } else {
            "~approx"
        }
    }
}

/// Errors from the token layer.
#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    /// The requested model id is not in the registry.
    #[error("unknown model '{model}' (known models: {known})")]
    UnknownModel {
        /// The unrecognized model id.
        model: String,
        /// Comma-separated list of known model ids.
        known: String,
    },
    /// The tiktoken tokenizer could not be constructed.
    #[error("failed to build tokenizer: {0}")]
    Tokenizer(String),
    /// The `--exact-anthropic` path failed (network, auth, mapping, etc.).
    #[error("{0}")]
    Exact(String),
}

/// A tiktoken encoding backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    /// gpt-4o / o-series lineage.
    O200kBase,
    /// gpt-4 / 3.5 lineage.
    Cl100kBase,
}

impl Encoding {
    fn label(self) -> &'static str {
        match self {
            Encoding::O200kBase => "o200k_base",
            Encoding::Cl100kBase => "cl100k_base",
        }
    }
}

/// One registry entry. Adding a model is a single row here.
struct ModelSpec {
    /// The canonical model id Jig exposes on `--model`.
    id: &'static str,
    /// Aliases users may type instead of `id`.
    aliases: &'static [&'static str],
    /// Which tiktoken encoding to count with.
    encoding: Encoding,
    /// True when the local count is an *approximation* of the provider's real
    /// tokenizer (Anthropic), false when it is exact (OpenAI).
    anthropic: bool,
    /// For `--exact-anthropic`: the concrete Anthropic API model id to send to
    /// the `count_tokens` endpoint. `None` for non-Anthropic models.
    api_model: Option<&'static str>,
    /// Which provider dialect `jig bench` speaks to for this model.
    bench_provider: crate::bench::Provider,
    /// The concrete API model string `jig bench` sends on the wire. Hardcoded
    /// mappings age, so `jig bench --api-model <string>` overrides this.
    bench_api_model: &'static str,
}

/// The model registry. One entry per supported model — extend here.
const REGISTRY: &[ModelSpec] = &[
    ModelSpec {
        id: "gpt-4o",
        aliases: &["gpt4o", "4o", "o200k"],
        encoding: Encoding::O200kBase,
        anthropic: false,
        api_model: None,
        bench_provider: crate::bench::Provider::OpenAI,
        bench_api_model: "gpt-4o",
    },
    ModelSpec {
        id: "gpt-4",
        aliases: &["gpt4", "gpt-3.5", "cl100k"],
        encoding: Encoding::Cl100kBase,
        anthropic: false,
        api_model: None,
        bench_provider: crate::bench::Provider::OpenAI,
        bench_api_model: "gpt-4",
    },
    ModelSpec {
        id: "claude-sonnet",
        aliases: &["claude", "sonnet", "claude-3-5-sonnet"],
        encoding: Encoding::O200kBase,
        anthropic: true,
        api_model: Some("claude-sonnet-5"),
        bench_provider: crate::bench::Provider::Anthropic,
        bench_api_model: "claude-sonnet-5",
    },
    ModelSpec {
        id: "claude-opus",
        aliases: &["opus"],
        encoding: Encoding::O200kBase,
        anthropic: true,
        api_model: Some("claude-opus-4-8"),
        bench_provider: crate::bench::Provider::Anthropic,
        bench_api_model: "claude-opus-4-8",
    },
];

/// Resolve a model id/alias into its `jig bench` mapping: the canonical id, the
/// provider dialect, and the concrete API model string. `None` if unknown.
///
/// This shares the one registry so budget and bench never drift on which model
/// ids exist or which provider a `claude-*`/`gpt-*` id belongs to.
pub fn bench_model_spec(
    model_id: &str,
) -> Option<(&'static str, crate::bench::Provider, &'static str)> {
    resolve(model_id).map(|s| (s.id, s.bench_provider, s.bench_api_model))
}

fn unknown_model(id: &str) -> TokenError {
    TokenError::UnknownModel {
        model: id.to_string(),
        known: known_models().join(", "),
    }
}

fn resolve(id: &str) -> Option<&'static ModelSpec> {
    let needle = id.trim().to_ascii_lowercase();
    REGISTRY.iter().find(|spec| {
        spec.id.eq_ignore_ascii_case(&needle)
            || spec.aliases.iter().any(|a| a.eq_ignore_ascii_case(&needle))
    })
}

/// The canonical ids of every model in the registry, for help text and errors.
pub fn known_models() -> Vec<&'static str> {
    REGISTRY.iter().map(|s| s.id).collect()
}

/// Whether `id` resolves to an Anthropic (approximate) model.
pub fn is_anthropic_model(id: &str) -> bool {
    resolve(id).map(|s| s.anthropic).unwrap_or(false)
}

/// A ready-to-use counter for one model: the tiktoken BPE is built once and
/// reused across every tool.
pub struct ModelCounter {
    model_id: String,
    encoding_label: &'static str,
    anthropic: bool,
    bpe: tiktoken_rs::CoreBPE,
}

impl ModelCounter {
    /// Build a counter for `model_id`, resolving aliases.
    pub fn new(model_id: &str) -> Result<Self, TokenError> {
        let spec = resolve(model_id).ok_or_else(|| unknown_model(model_id))?;
        let bpe = match spec.encoding {
            Encoding::O200kBase => tiktoken_rs::o200k_base(),
            Encoding::Cl100kBase => tiktoken_rs::cl100k_base(),
        }
        .map_err(|e| TokenError::Tokenizer(e.to_string()))?;
        Ok(ModelCounter {
            model_id: spec.id.to_string(),
            encoding_label: spec.encoding.label(),
            anthropic: spec.anthropic,
            bpe,
        })
    }

    /// Count the tokens in `text` (ordinary encoding — no special-token
    /// handling, so arbitrary tool JSON can never trip a special sequence).
    pub fn count(&self, text: &str) -> usize {
        self.bpe.encode_ordinary(text).len()
    }

    /// The canonical model id (post-alias-resolution).
    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    /// The tiktoken encoding label backing this counter (e.g. `o200k_base`),
    /// for provenance in machine output.
    pub fn encoding_label(&self) -> &'static str {
        self.encoding_label
    }

    /// The exactness of this model's per-tool counts.
    pub fn exactness(&self) -> Exactness {
        if self.anthropic {
            Exactness::Approximate {
                method: format!(
                    "counted with the {} tokenizer as a proxy (Claude's tokenizer is not \
                     publicly available); use --exact-anthropic for the exact total",
                    self.encoding_label
                ),
            }
        } else {
            Exactness::Exact
        }
    }

    /// The model header line, e.g. `gpt-4o (o200k_base, exact)` or
    /// `claude-sonnet (~approx via o200k_base)`.
    pub fn header(&self) -> String {
        if self.anthropic {
            format!("{} (~approx via {})", self.model_id, self.encoding_label)
        } else {
            format!("{} ({}, exact)", self.model_id, self.encoding_label)
        }
    }
}

/// V1 canonical rendering of a single tool — see [`CANONICAL_RENDERING_DOC`].
///
/// This is the one place the "what gets counted" question is answered. Keep it
/// pluggable: a future milestone may add client-specific rendering variants.
/// # Why `annotations` is absent
///
/// A tool may carry an [`annotations`] object. It is **not** counted, because it
/// is never sent to the model: annotations are client-side metadata (display
/// name, permission gating), and neither the Anthropic nor the OpenAI tool
/// object has a member for them. Adding the typed field to [`Tool`] therefore
/// left this rendering byte-identical, and every previously published figure —
/// including the bundled census — remains directly comparable.
///
/// [`annotations`]: crate::protocol::Tool::annotations
pub fn canonical_tool_json(tool: &Tool) -> String {
    let mut map = Map::new();
    map.insert("name".to_string(), json!(tool.name));
    if let Some(desc) = &tool.description {
        map.insert("description".to_string(), json!(desc));
    }
    map.insert("input_schema".to_string(), tool.input_schema.clone());
    // serde_json's default Map is a BTreeMap → keys serialize in sorted order,
    // so the rendering (and its token count) is deterministic regardless of the
    // order the server sent fields in.
    serde_json::to_string(&Value::Object(map)).unwrap_or_default()
}

/// The token budget for one tool under one model.
#[derive(Debug, Clone)]
pub struct ToolBudget {
    /// The callable tool name (the identifier for `jig call --tool`).
    pub name: String,
    /// The optional human-facing title.
    pub title: Option<String>,
    /// Tokens for this tool's canonical rendering.
    pub tokens: usize,
    /// The exact canonical JSON that was counted.
    pub canonical: String,
}

/// The full token budget for one model over a server's tool surface.
#[derive(Debug, Clone)]
pub struct ModelBudget {
    /// Canonical model id.
    pub model_id: String,
    /// Header line describing the model + tokenizer.
    pub header: String,
    /// Exactness of the per-tool (and instructions) counts.
    pub per_tool_exactness: Exactness,
    /// Exactness of the grand [`total`](ModelBudget::total). Usually equal to
    /// `per_tool_exactness`, but `--exact-anthropic` can make the total exact
    /// while the per-tool rows remain approximate.
    pub total_exactness: Exactness,
    /// Per-tool budgets, in server order (sort at render time).
    pub tools: Vec<ToolBudget>,
    /// Tokens for the server `instructions` string, if present.
    pub instructions_tokens: Option<usize>,
    /// Grand total tokens.
    pub total: usize,
}

/// Compute a model's budget entirely locally (exact for OpenAI, `~approx` for
/// Anthropic). The grand total is exactly the sum of per-tool counts plus the
/// instructions count.
pub fn budget_local(
    model_id: &str,
    tools: &[Tool],
    instructions: Option<&str>,
) -> Result<ModelBudget, TokenError> {
    let counter = ModelCounter::new(model_id)?;
    let exactness = counter.exactness();

    let mut tool_budgets = Vec::with_capacity(tools.len());
    let mut total = 0usize;
    for tool in tools {
        let canonical = canonical_tool_json(tool);
        let tokens = counter.count(&canonical);
        total += tokens;
        tool_budgets.push(ToolBudget {
            name: tool.name.clone(),
            title: tool.title.clone(),
            tokens,
            canonical,
        });
    }

    let instructions_tokens = instructions.map(|s| counter.count(s));
    if let Some(it) = instructions_tokens {
        total += it;
    }

    Ok(ModelBudget {
        model_id: counter.model_id().to_string(),
        header: counter.header(),
        per_tool_exactness: exactness.clone(),
        total_exactness: exactness,
        tools: tool_budgets,
        instructions_tokens,
        total,
    })
}

/// Response shape of `POST /v1/messages/count_tokens`.
#[cfg(feature = "exact-anthropic")]
#[derive(Deserialize)]
struct CountTokensResponse {
    input_tokens: usize,
}

/// Call Anthropic's `count_tokens` endpoint for an **exact grand total** of the
/// tools array (+ instructions), in tokens.
///
/// The endpoint reports a single request-level total, not a per-tool
/// breakdown, so this returns only the total. Two calls are made: a baseline
/// (minimal message, no tools) and the full request; the difference isolates
/// the tools + instructions contribution from the message framing.
///
/// The API key is read from the caller and sent only in the `x-api-key`
/// header — it is never logged, echoed, or placed in an error message.
#[cfg(feature = "exact-anthropic")]
pub async fn anthropic_exact_total(
    model_id: &str,
    tools: &[Tool],
    instructions: Option<&str>,
    api_key: &str,
) -> Result<usize, TokenError> {
    let spec = resolve(model_id).ok_or_else(|| unknown_model(model_id))?;
    let api_model = spec.api_model.ok_or_else(|| {
        TokenError::Exact(format!(
            "model '{}' has no Anthropic API mapping for --exact-anthropic",
            model_id
        ))
    })?;

    let client = reqwest::Client::new();
    let baseline = count_tokens_call(&client, api_model, &[], None, api_key).await?;
    let full = count_tokens_call(&client, api_model, tools, instructions, api_key).await?;
    Ok(full.saturating_sub(baseline))
}

#[cfg(feature = "exact-anthropic")]
async fn count_tokens_call(
    client: &reqwest::Client,
    api_model: &str,
    tools: &[Tool],
    instructions: Option<&str>,
    api_key: &str,
) -> Result<usize, TokenError> {
    let tools_json: Vec<Value> = tools
        .iter()
        .map(|t| {
            let mut m = Map::new();
            m.insert("name".to_string(), json!(t.name));
            if let Some(d) = &t.description {
                m.insert("description".to_string(), json!(d));
            }
            m.insert("input_schema".to_string(), t.input_schema.clone());
            Value::Object(m)
        })
        .collect();

    let mut body = json!({
        "model": api_model,
        "messages": [{ "role": "user", "content": "." }],
    });
    if !tools_json.is_empty() {
        body["tools"] = Value::Array(tools_json);
    }
    if let Some(s) = instructions {
        body["system"] = json!(s);
    }

    let resp = client
        .post("https://api.anthropic.com/v1/messages/count_tokens")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| TokenError::Exact(format!("count_tokens request failed: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        let detail: String = resp
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect();
        return Err(TokenError::Exact(format!(
            "count_tokens returned HTTP {status}: {detail}"
        )));
    }

    let parsed: CountTokensResponse = resp
        .json()
        .await
        .map_err(|e| TokenError::Exact(format!("invalid count_tokens response: {e}")))?;
    Ok(parsed.input_tokens)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str, desc: Option<&str>, schema: Value) -> Tool {
        serde_json::from_value(json!({
            "name": name,
            "description": desc,
            "inputSchema": schema,
        }))
        .unwrap()
    }

    #[test]
    fn resolves_ids_and_aliases_case_insensitively() {
        assert!(resolve("gpt-4o").is_some());
        assert!(resolve("GPT-4O").is_some());
        assert!(resolve("4o").is_some());
        assert!(resolve("claude").is_some());
        assert!(resolve("sonnet").is_some());
        assert!(resolve("nope").is_none());
    }

    #[test]
    fn openai_is_exact_anthropic_is_approx() {
        assert!(ModelCounter::new("gpt-4o").unwrap().exactness().is_exact());
        assert!(ModelCounter::new("gpt-4").unwrap().exactness().is_exact());
        assert!(!ModelCounter::new("claude-sonnet")
            .unwrap()
            .exactness()
            .is_exact());
        assert!(!is_anthropic_model("gpt-4o"));
        assert!(is_anthropic_model("claude-sonnet"));
    }

    #[test]
    fn headers_are_labelled() {
        assert_eq!(
            ModelCounter::new("gpt-4o").unwrap().header(),
            "gpt-4o (o200k_base, exact)"
        );
        assert_eq!(
            ModelCounter::new("claude-sonnet").unwrap().header(),
            "claude-sonnet (~approx via o200k_base)"
        );
    }

    #[test]
    fn known_string_token_counts_match_tiktoken() {
        // Fixtures: exact expected counts for the two OpenAI encodings.
        let o200k = ModelCounter::new("gpt-4o").unwrap();
        let cl100k = ModelCounter::new("gpt-4").unwrap();
        assert_eq!(o200k.count(""), 0);
        assert_eq!(cl100k.count(""), 0);
        // "hello world" is two tokens in both cl100k_base and o200k_base.
        assert_eq!(cl100k.count("hello world"), 2);
        assert_eq!(o200k.count("hello world"), 2);
        // "tiktoken is great!" is the canonical 6-token cl100k_base example.
        assert_eq!(cl100k.count("tiktoken is great!"), 6);
    }

    #[test]
    fn counting_is_deterministic() {
        let c = ModelCounter::new("gpt-4o").unwrap();
        let s = r#"{"name":"echo","input_schema":{"type":"object"}}"#;
        assert_eq!(c.count(s), c.count(s));
    }

    #[test]
    fn canonical_rendering_is_compact_sorted_and_omits_absent_description() {
        // With a description present.
        let t = tool(
            "echo",
            Some("Echo it"),
            json!({ "type": "object", "properties": { "text": { "type": "string" } } }),
        );
        let rendered = canonical_tool_json(&t);
        assert!(!rendered.contains('\n'));
        assert!(!rendered.contains(": ")); // compact — no space after colon
                                           // keys sorted: description < input_schema < name
        let dpos = rendered.find("\"description\"").unwrap();
        let ipos = rendered.find("\"input_schema\"").unwrap();
        let npos = rendered.find("\"name\"").unwrap();
        assert!(dpos < ipos && ipos < npos, "keys not sorted: {rendered}");

        // Without a description, the key is omitted entirely.
        let t2 = tool("bare", None, json!({ "type": "object" }));
        let r2 = canonical_tool_json(&t2);
        assert!(!r2.contains("description"), "unexpected description: {r2}");
        assert!(r2.contains("\"name\":\"bare\""));
    }

    /// **The metric must not move when a server annotates its tools.**
    ///
    /// Annotations never reach the model — no provider tool object carries
    /// them — so they cost zero prompt tokens. If this ever fails, the canonical
    /// rendering has silently changed and every published figure (including the
    /// bundled census) has been invalidated without a changelog entry.
    #[test]
    fn annotations_do_not_change_the_canonical_rendering_or_the_count() {
        let bare: Tool = serde_json::from_value(json!({
            "name": "delete_thing",
            "description": "Delete a thing.",
            "inputSchema": { "type": "object", "properties": { "id": { "type": "string" } } },
        }))
        .unwrap();

        let annotated: Tool = serde_json::from_value(json!({
            "name": "delete_thing",
            "description": "Delete a thing.",
            "inputSchema": { "type": "object", "properties": { "id": { "type": "string" } } },
            "annotations": {
                "title": "A title long enough to be several tokens on its own",
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": false,
                "openWorldHint": true
            },
        }))
        .unwrap();

        assert!(annotated.annotations.is_some(), "fixture must be annotated");
        assert_eq!(
            canonical_tool_json(&bare),
            canonical_tool_json(&annotated),
            "annotations leaked into the canonical rendering"
        );

        let counter = ModelCounter::new("gpt-4o").unwrap();
        assert_eq!(
            counter.count(&canonical_tool_json(&bare)),
            counter.count(&canonical_tool_json(&annotated))
        );
        assert_eq!(
            budget_local("gpt-4o", &[bare], None).unwrap().total,
            budget_local("gpt-4o", &[annotated], None).unwrap().total
        );
    }

    #[test]
    fn budget_total_equals_sum_of_parts_plus_instructions() {
        let tools = vec![
            tool("a", Some("first"), json!({ "type": "object" })),
            tool("b", Some("second"), json!({ "type": "object" })),
        ];
        let mb = budget_local("gpt-4o", &tools, Some("Some server instructions.")).unwrap();
        let sum: usize = mb.tools.iter().map(|t| t.tokens).sum();
        assert_eq!(mb.total, sum + mb.instructions_tokens.unwrap());
        assert_eq!(mb.tools.len(), 2);
        assert!(mb.per_tool_exactness.is_exact());
        assert!(mb.total_exactness.is_exact());
    }

    #[test]
    fn budget_without_instructions_has_none() {
        let tools = vec![tool("a", None, json!({ "type": "object" }))];
        let mb = budget_local("claude-sonnet", &tools, None).unwrap();
        assert!(mb.instructions_tokens.is_none());
        assert_eq!(mb.total, mb.tools[0].tokens);
        assert!(!mb.per_tool_exactness.is_exact());
    }

    #[test]
    fn unknown_model_is_an_error() {
        assert!(matches!(
            budget_local("gpt-9", &[], None),
            Err(TokenError::UnknownModel { .. })
        ));
    }
}
