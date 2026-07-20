//! The tool surface `jig serve` exposes, and the handlers behind it.
//!
//! # These descriptions are themselves under test
//!
//! Jig grades other servers on schema hygiene and description quality. Its own
//! surface is graded by that same rubric in an integration test that runs
//! `jig check` against `jig serve` and refuses anything below an A. So every
//! tool here carries a `title`, a description long enough to be informative and
//! short enough not to be a monologue, a distinct vocabulary from its
//! neighbours, and a fully typed and described parameter schema.
//!
//! # One measurement, two front doors
//!
//! Each handler calls the exact code path its CLI verb calls — `jig check` and
//! `check_server` share [`crate::check::observe_and_evaluate`], and both render
//! through the same JSON renderer. A tool that graded differently from the
//! command line would be worse than no tool at all.

use std::sync::Arc;
use std::time::Duration;

use jig_core::{ProtocolTap, Tool};
use serde_json::{json, Map, Value};

use super::{sampling, ServeState};
use crate::Target;

/// How many tools this server exposes. Asserted against [`catalog`] so the
/// startup banner can never drift from reality.
pub(crate) const TOOL_COUNT: usize = 6;

/// Connection defaults inherited from the `jig serve` command line, applied to
/// any tool call that does not override them.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Defaults {
    /// Per-request timeout in seconds when connecting to a target server.
    pub(crate) timeout_secs: u64,
    /// Inbound message size cap in bytes.
    pub(crate) max_message_bytes: u64,
}

/// Why a `tools/call` could not be attempted at all (as opposed to a tool that
/// ran and reported a failure, which is a normal result with `isError`).
pub(crate) enum CallError {
    /// No tool by that name.
    UnknownTool(String),
    /// The `arguments` object was unusable.
    BadArguments(String),
}

// ---------------------------------------------------------------------------
// Schema construction
// ---------------------------------------------------------------------------

/// One property in a tool's input schema: a JSON Schema fragment plus the
/// human description the rubric (rightly) insists on.
fn prop(schema: Value, description: &str) -> Value {
    let mut m = schema.as_object().cloned().unwrap_or_default();
    m.insert("description".to_string(), json!(description));
    Value::Object(m)
}

/// Assemble an input schema from named properties and a required list.
///
/// The `annotations` object carries MCP's behavioural hints. It sits *inside*
/// `inputSchema` rather than beside it because that is the only place a client
/// reading through `jig_core::protocol::Tool` can see it — that type models no
/// tool-level `annotations` field, so hints placed there are dropped on the
/// floor. Noted rather than hidden: this is a wart in the type, not a
/// preference.
fn schema(properties: Vec<(&str, Value)>, required: &[&str], annotations: Value) -> Value {
    let mut props = Map::new();
    for (name, spec) in properties {
        props.insert(name.to_string(), spec);
    }
    json!({
        "type": "object",
        "properties": Value::Object(props),
        "required": required,
        "additionalProperties": false,
        "annotations": annotations,
    })
}

/// The two mutually-exclusive ways to name a target server, shared by every
/// tool that connects to one.
fn target_properties() -> Vec<(&'static str, Value)> {
    vec![
        (
            "stdio",
            prop(
                json!({ "type": "string" }),
                "Command line launching the target server over stdio, e.g. \
                 \"npx -y @modelcontextprotocol/server-github\". Give this or `http`, not both.",
            ),
        ),
        (
            "http",
            prop(
                json!({ "type": "string" }),
                "URL of a remote target server speaking Streamable HTTP, e.g. \
                 \"https://example.com/mcp\". Give this or `stdio`, not both.",
            ),
        ),
    ]
}

/// The optional per-call connection timeout, shared by the connecting tools.
fn timeout_property() -> (&'static str, Value) {
    (
        "timeout_seconds",
        prop(
            json!({ "type": "integer", "minimum": 0 }),
            "Seconds to wait for each request to the target before giving up. \
             0 waits indefinitely. Defaults to the value `jig serve` was started with.",
        ),
    )
}

/// A read-only, non-destructive annotation set — true of every tool here.
/// `openWorldHint` is true for the connecting tools (they reach an external
/// server) and false for the purely local one.
fn read_only(open_world: bool) -> Value {
    json!({
        "readOnlyHint": true,
        "destructiveHint": false,
        "idempotentHint": true,
        "openWorldHint": open_world,
    })
}

// ---------------------------------------------------------------------------
// The catalog
// ---------------------------------------------------------------------------

/// Every tool this server exposes, in `tools/list` order.
pub(crate) fn catalog() -> Vec<Value> {
    let mut check_props = target_properties();
    check_props.push((
        "percentiles",
        prop(
            json!({ "type": "string" }),
            "Path to an ecosystem census file for the cost comparison, or \"none\" to score \
             against fixed bands instead. Defaults to the census built into this binary.",
        ),
    ));
    check_props.push(timeout_property());

    let mut budget_props = target_properties();
    budget_props.push((
        "models",
        prop(
            json!({ "type": "array", "items": { "type": "string" } }),
            "Which models to price against, e.g. [\"gpt-4o\", \"claude-sonnet\"]. \
             Defaults to a standard spread.",
        ),
    ));
    budget_props.push(timeout_property());

    let mut context_props = target_properties();
    context_props.push((
        "model",
        prop(
            json!({ "type": "string" }),
            "Whose tokenizer and request dialect to render with, e.g. \"gpt-4o\" or \
             \"claude-sonnet\".",
        ),
    ));
    context_props.push(timeout_property());

    let mut inspect_props = target_properties();
    inspect_props.push(timeout_property());

    let mut bench_props = target_properties();
    bench_props.push((
        "task",
        prop(
            json!({ "type": "string" }),
            "The plain-language job to hand the model, e.g. \"book a table for two tonight\".",
        ),
    ));
    bench_props.push((
        "runs",
        prop(
            json!({ "type": "integer", "minimum": 1, "maximum": 20 }),
            "How many times to repeat the task. More repetitions expose instability that a \
             single attempt hides. Defaults to 3.",
        ),
    ));
    bench_props.push((
        "temperature",
        prop(
            json!({ "type": "number", "minimum": 0.0, "maximum": 2.0 }),
            "Requested sampling temperature. Advisory — the host may override it, and the \
             result says so when it does. Defaults to 1.0.",
        ),
    ));

    vec![
        tool_json(
            "check_server",
            "Grade an MCP server",
            "Produce Jig's full report card for another MCP server: a 0-100 composite plus \
             separate marks for protocol compliance, context cost, schema hygiene, description \
             quality and robustness, each with the specific defects found and a ranked list of \
             repairs. Start here when the question is simply \"is this server any good?\".",
            schema(check_props, &[], read_only(true)),
        ),
        tool_json(
            "budget_server",
            "Price a tool surface in tokens",
            "Work out what another MCP server costs you in context-window tokens before anyone \
             types a word — its tool definitions measured individually and summed, for each \
             model you name. Use this to decide whether a server earns the room it occupies.",
            schema(budget_props, &[], read_only(true)),
        ),
        tool_json(
            "context_server",
            "Show the exact model-facing request",
            "Reveal the literal request body a language model would receive for another MCP \
             server: its tools translated into the provider's function-calling dialect, the \
             framing prompt, and a stand-in task, annotated with where the tokens go. Reads the \
             target only; contacts no model vendor.",
            schema(context_props, &[], read_only(true)),
        ),
        tool_json(
            "inspect_server",
            "List everything a server advertises",
            "Complete a handshake with another MCP server and report what it declares: \
             implementation name and version, the protocol revision agreed on, its capability \
             flags, and the full inventory of tools, resources and prompts with their schemas. \
             The plain \"what is in here?\" question.",
            schema(inspect_props, &[], read_only(true)),
        ),
        tool_json(
            "bench_server",
            "Measure which tool a model picks",
            "Put a live model in front of another MCP server's tools, hand it a task, and record \
             which tool it reaches for and with what arguments — repeatedly, so wavering shows \
             up as a spread rather than hiding behind one lucky attempt. Borrows your host's own \
             model through MCP sampling, so no vendor credential is involved.",
            schema(bench_props, &["task"], read_only(true)),
        ),
        tool_json(
            "list_local_servers",
            "Find MCP servers already on this machine",
            "Enumerate the MCP servers configured in this machine's desktop and editor \
             applications, merged into one list and labelled by which config file each came \
             from. Environment variable values are replaced with dots before they leave this \
             process; only the variable names appear.",
            schema(vec![], &[], read_only(false)),
        ),
    ]
}

fn tool_json(name: &str, title: &str, description: &str, input_schema: Value) -> Value {
    json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": input_schema,
    })
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Execute a `tools/call`.
pub(crate) async fn call(
    state: &Arc<ServeState>,
    params: &Value,
    defaults: Defaults,
) -> Result<Value, CallError> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| CallError::BadArguments("tools/call requires a `name`".to_string()))?;
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    if !args.is_object() {
        return Err(CallError::BadArguments(
            "`arguments` must be an object".to_string(),
        ));
    }

    let outcome = match name {
        "check_server" => check_server(&args, defaults).await,
        "budget_server" => budget_server(&args, defaults).await,
        "context_server" => context_server(&args, defaults).await,
        "inspect_server" => inspect_server(&args, defaults).await,
        "bench_server" => sampling::bench_server(state, &args, defaults).await,
        "list_local_servers" => list_local_servers(),
        other => return Err(CallError::UnknownTool(other.to_string())),
    };

    Ok(match outcome {
        Ok((summary, structured)) => json!({
            "content": [ { "type": "text", "text": summary } ],
            "structuredContent": structured,
            "isError": false,
        }),
        // A tool that could not do its job reports that *as a result*, per MCP:
        // `isError: true` is a well-formed answer, not a protocol failure. It
        // lets the model read and act on the reason.
        Err(message) => json!({
            "content": [ { "type": "text", "text": message } ],
            "isError": true,
        }),
    })
}

/// A handler's outcome: a human summary plus the machine-readable report.
pub(crate) type ToolOutcome = Result<(String, Value), String>;

/// Resolve the `stdio` / `http` arguments into a connection target.
pub(crate) fn target_from(args: &Value) -> Result<Target, String> {
    let stdio = args
        .get("stdio")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty());
    let http = args
        .get("http")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty());
    if stdio.is_none() && http.is_none() {
        return Err(
            "Name the server to target: pass `stdio` with a command line, or `http` with an \
             endpoint URL."
                .to_string(),
        );
    }
    Target::resolve(stdio, http, None, Vec::new())
}

/// The effective timeout for a call: the `timeout_seconds` argument if given,
/// else the server's start-up default.
pub(crate) fn timeout_of(args: &Value, defaults: Defaults) -> u64 {
    args.get("timeout_seconds")
        .and_then(Value::as_u64)
        .unwrap_or(defaults.timeout_secs)
}

/// Connect to a target and list its tools plus its instructions — the shared
/// opening move of `budget_server`, `context_server` and `bench_server`.
pub(crate) async fn list_target_tools(
    target: &Target,
    defaults: Defaults,
    timeout_secs: u64,
) -> Result<(jig_core::Implementation, Vec<Tool>, Option<String>), String> {
    let tap = ProtocolTap::new();
    let client = target
        .connect(tap, timeout_secs, defaults.max_message_bytes)
        .await?;
    let tools = client.list_tools().await.map_err(|e| e.to_string())?;
    let server = client.server_info().clone();
    let instructions = client.instructions().map(str::to_string);
    client.shutdown().await.map_err(|e| e.to_string())?;
    Ok((server, tools, instructions))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn check_server(args: &Value, defaults: Defaults) -> ToolOutcome {
    let target = target_from(args)?;
    let percentiles_path = args.get("percentiles").and_then(Value::as_str);
    let percentiles = crate::check::load_percentiles(percentiles_path.map(std::path::Path::new))?;

    let tap = ProtocolTap::new();
    let report = crate::check::observe_and_evaluate(
        &target,
        &tap,
        percentiles.as_ref(),
        timeout_of(args, defaults),
        defaults.max_message_bytes,
    )
    .await?;

    // The same renderers the CLI uses, so the tool and the verb agree.
    let summary = crate::check::render_human(&report);
    let structured: Value =
        serde_json::from_str(&crate::check::render_json(&report)).map_err(|e| e.to_string())?;
    Ok((summary, structured))
}

async fn budget_server(args: &Value, defaults: Defaults) -> ToolOutcome {
    let target = target_from(args)?;
    let models: Vec<String> = match args.get("models").and_then(Value::as_array) {
        Some(list) => list
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        None => Vec::new(),
    };
    let models = if models.is_empty() {
        crate::budget::DEFAULT_MODELS
            .iter()
            .map(|s| s.to_string())
            .collect()
    } else {
        models
    };

    let (server, tools, instructions) =
        list_target_tools(&target, defaults, timeout_of(args, defaults)).await?;

    let mut budgets = Vec::with_capacity(models.len());
    for model in &models {
        budgets.push(
            jig_core::budget_local(model, &tools, instructions.as_deref())
                .map_err(|e| e.to_string())?,
        );
    }

    let summary = crate::budget::render_table(&server, &budgets);
    let structured: Value =
        serde_json::from_str(&crate::budget::render_json(&server, &tools, &budgets))
            .map_err(|e| e.to_string())?;
    Ok((summary, structured))
}

async fn context_server(args: &Value, defaults: Defaults) -> ToolOutcome {
    let target = target_from(args)?;
    let model = args
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("gpt-4o")
        .to_string();

    let (server, tools, instructions) =
        list_target_tools(&target, defaults, timeout_of(args, defaults)).await?;

    let (_, provider, api_model) = jig_core::tokens::bench_model_spec(&model)
        .ok_or_else(|| format!("Unknown model '{model}'. Known: {}", known_models()))?;
    let view =
        jig_core::build_context(provider, &model, api_model, &tools, instructions.as_deref())
            .map_err(|e| e.to_string())?;

    let summary = crate::context::render_human(&server, &view);
    let structured: Value = serde_json::from_str(&crate::context::render_json(&server, &view))
        .map_err(|e| e.to_string())?;
    Ok((summary, structured))
}

async fn inspect_server(args: &Value, defaults: Defaults) -> ToolOutcome {
    let target = target_from(args)?;
    let tap = ProtocolTap::new();
    let client = target
        .connect(tap, timeout_of(args, defaults), defaults.max_message_bytes)
        .await?;

    let tools = client.list_tools().await.map_err(|e| e.to_string())?;
    let resources = client.list_resources().await.map_err(|e| e.to_string())?;
    let prompts = client.list_prompts().await.map_err(|e| e.to_string())?;

    let structured = crate::render::inspect_json_doc(
        client.server_info(),
        client.protocol_version(),
        client.capabilities(),
        client.instructions(),
        &tools,
        &resources,
        &prompts,
    );
    let summary = crate::render::inspect_report(&client, &tools, &resources, &prompts);
    client.shutdown().await.map_err(|e| e.to_string())?;
    Ok((summary, structured))
}

fn list_local_servers() -> ToolOutcome {
    let discovered = jig_core::discover();
    // `servers::render_json` is the redacting renderer — environment values
    // never reach it as plaintext. Reusing it is what guarantees this tool
    // cannot become the one place a token escapes.
    let structured: Value = serde_json::from_str(&crate::servers::render_json(&discovered))
        .map_err(|e| e.to_string())?;
    Ok((crate::servers::render_table(&discovered), structured))
}

fn known_models() -> String {
    jig_core::tokens::known_models().join(", ")
}

/// The default timeout a sampling-backed bench allows the host per run.
pub(crate) const BENCH_SAMPLING_TIMEOUT: Duration = super::SERVER_REQUEST_TIMEOUT;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn catalog_size_matches_the_advertised_count() {
        assert_eq!(catalog().len(), TOOL_COUNT);
    }

    /// Every tool must survive Jig's own protocol rules: a legal name, a title,
    /// a description, and a schema whose every property is typed and described.
    /// The end-to-end dogfood test grades the running server; this one catches
    /// a regression at unit speed, without spawning anything.
    #[test]
    fn every_tool_satisfies_the_rubric_it_grades_others_by() {
        let mut names = BTreeSet::new();
        for tool in catalog() {
            let name = tool["name"].as_str().expect("a name");
            assert!(names.insert(name.to_string()), "duplicate tool {name}");

            // SEP-986 name format.
            assert!(
                (1..=64).contains(&name.chars().count())
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '-')),
                "tool name {name} is not a legal MCP tool name"
            );
            assert!(!name.contains('-'), "{name}: keep one naming convention");

            assert!(
                !tool["title"].as_str().unwrap_or_default().trim().is_empty(),
                "{name} has no title"
            );

            let description = tool["description"].as_str().unwrap_or_default();
            assert!(!description.trim().is_empty(), "{name} has no description");

            let schema = &tool["inputSchema"];
            assert_eq!(schema["type"], "object", "{name}: schema must be an object");
            assert!(
                schema.get("annotations").is_some(),
                "{name}: behavioural annotations are missing"
            );
            for (prop, spec) in schema["properties"].as_object().expect("properties") {
                assert!(
                    spec.get("type").is_some() || spec.get("enum").is_some(),
                    "{name}.{prop} declares no type"
                );
                assert!(
                    !spec["description"]
                        .as_str()
                        .unwrap_or_default()
                        .trim()
                        .is_empty(),
                    "{name}.{prop} has no description"
                );
            }
        }
    }

    /// The description-quality dimension calls a description of four gpt-4o
    /// tokens or fewer "terse" and one of 160 or more "verbose". Sit inside
    /// that window with room to spare on both ends.
    #[test]
    fn descriptions_are_neither_terse_nor_verbose() {
        let counter = jig_core::ModelCounter::new("gpt-4o").expect("tokenizer");
        for tool in catalog() {
            let name = tool["name"].as_str().unwrap();
            let n = counter.count(tool["description"].as_str().unwrap());
            assert!(n > 4, "{name}: description is terse ({n} tokens)");
            assert!(n < 160, "{name}: description is verbose ({n} tokens)");
        }
    }

    /// The tool-set advisor flags two tools whose descriptions share 80% or
    /// more of their content words — a model cannot tell such tools apart.
    /// Ours must be comfortably distinct from one another.
    #[test]
    fn no_two_descriptions_read_alike() {
        let tools: Vec<Tool> = catalog()
            .into_iter()
            .map(|t| serde_json::from_value(t).expect("a valid Tool"))
            .collect();
        let costs: Vec<jig_core::ToolTokenCost> = tools
            .iter()
            .map(|t| jig_core::ToolTokenCost {
                name: t.name.clone(),
                tokens: 100,
            })
            .collect();
        let findings = jig_core::advise_tool_set(&tools, &costs);
        let overlap: Vec<_> = findings
            .iter()
            .filter(|f| f.message.contains("overlap") || f.message.contains("distinguish"))
            .collect();
        assert!(
            overlap.is_empty(),
            "tools are not distinguishable: {overlap:#?}"
        );
    }

    /// `Target` deliberately implements no `Debug` (it can carry a discovered
    /// server's environment), so unwrap the error side by hand.
    fn target_error(args: Value) -> String {
        match target_from(&args) {
            Ok(_) => panic!("expected {args} to be rejected as a target"),
            Err(e) => e,
        }
    }

    #[test]
    fn a_call_without_a_target_explains_what_to_pass() {
        let err = target_error(json!({}));
        assert!(err.contains("stdio"), "{err}");
        assert!(err.contains("http"), "{err}");
    }

    #[test]
    fn a_blank_target_string_is_not_a_target() {
        // An empty string is a plausible thing for a model to emit; treating it
        // as "a stdio command" would produce a baffling spawn failure.
        let err = target_error(json!({ "stdio": "   " }));
        assert!(err.contains("Name the server to target"), "{err}");
    }

    #[test]
    fn timeout_argument_overrides_the_default() {
        let defaults = Defaults {
            timeout_secs: 30,
            max_message_bytes: 1024,
        };
        assert_eq!(timeout_of(&json!({}), defaults), 30);
        assert_eq!(timeout_of(&json!({ "timeout_seconds": 5 }), defaults), 5);
    }
}
