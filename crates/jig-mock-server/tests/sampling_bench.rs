//! End-to-end tests for the **sampling-backed bench** — `jig bench` with no
//! credentials anywhere.
//!
//! Three processes take part:
//!
//! ```text
//!   jig-mock-server --sampling-client   (the host: advertises `sampling`,
//!         │                              answers sampling/createMessage)
//!         │ stdio
//!   jig serve                           (jig as an MCP server)
//!         │ stdio
//!   jig-mock-server                     (the target being benched)
//! ```
//!
//! The headline test is a **parity** test: the same tool selections, delivered
//! once through a provider HTTP API and once through MCP sampling, must produce
//! the same distribution. That is the property that makes the sampling path a
//! real substitute rather than a different measurement wearing the same name.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use serde_json::{json, Value};

/// The `jig-mock-server` binary (this crate's own).
fn mock_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_jig-mock-server"))
}

/// The `jig` binary: a sibling in the same target directory.
fn jig_bin() -> PathBuf {
    let mut p = mock_bin();
    p.set_file_name(if cfg!(windows) { "jig.exe" } else { "jig" });
    assert!(
        p.exists(),
        "jig binary not found at {} — run with `cargo test --workspace --all-targets`",
        p.display()
    );
    p
}

/// A double-quoted path, so Jig's command splitter keeps a path containing
/// spaces as one token.
fn quoted(path: &Path, extra: &str) -> String {
    if extra.is_empty() {
        format!("\"{}\"", path.display())
    } else {
        format!("\"{}\" {extra}", path.display())
    }
}

// ---------------------------------------------------------------------------
// The scripted-host harness
// ---------------------------------------------------------------------------

/// A scripted sampling response selecting `tool` with `arguments`, in the JSON
/// form `SAMPLING_RESPONSE_PROTOCOL` asks the model for.
fn pick(tool: &str, arguments: Value) -> String {
    json!({ "tool": tool, "arguments": arguments }).to_string()
}

/// The reservation arguments the mock provider's `reservation` scenario emits,
/// so the two paths are fed byte-identical selections.
fn reservation_args() -> Value {
    json!({ "party": { "size": 2, "seating": "outdoor" }, "date": "2026-01-01" })
}

/// Run the scripted host against `jig serve`, calling one tool, and return the
/// tool's `result` object.
fn run_host(tool: &str, arguments: Value, script: &[String], extra_env: &[(&str, &str)]) -> Value {
    let mut cmd = Command::new(mock_bin());
    cmd.arg("--sampling-client")
        .env("JIG_FIXTURE_TOOL", tool)
        .env("JIG_FIXTURE_ARGS", arguments.to_string())
        .env(
            "JIG_FIXTURE_SCRIPT",
            serde_json::to_string(script).expect("serializable script"),
        );
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let out = cmd
        .arg("--")
        .arg(jig_bin())
        .arg("serve")
        .stderr(Stdio::inherit())
        .output()
        .expect("spawn the scripted host");
    assert!(
        out.status.success(),
        "the scripted host failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout
        .lines()
        .last()
        .unwrap_or_else(|| panic!("the host printed nothing:\n{stdout}"));
    let frame: Value = serde_json::from_str(line)
        .unwrap_or_else(|e| panic!("host output was not JSON ({e}): {line}"));
    frame["result"].clone()
}

/// Arguments for a `bench_server` call against the mock target server.
fn bench_args(runs: usize) -> Value {
    json!({
        "stdio": quoted(&mock_bin(), ""),
        "task": "Book a table for two outdoors on the first of January",
        "runs": runs,
    })
}

// ---------------------------------------------------------------------------
// The direct-API path, for comparison
// ---------------------------------------------------------------------------

/// The mock model provider, on an OS-assigned port.
struct Provider {
    child: Child,
    port: u16,
}

impl Provider {
    fn start() -> Provider {
        let mut child = Command::new(mock_bin())
            .arg("--provider")
            .arg("0")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the mock provider");
        // The fixture announces its actual bound port on one of its streams.
        let stderr = child.stderr.take().expect("provider stderr");
        let mut reader = BufReader::new(stderr);
        let mut port = None;
        for _ in 0..20 {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            if let Some(rest) = line.split("127.0.0.1:").nth(1) {
                let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                if let Ok(p) = digits.parse::<u16>() {
                    port = Some(p);
                    break;
                }
            }
        }
        let port = port.unwrap_or_else(|| {
            let _ = child.kill();
            panic!("the mock provider never announced its port");
        });
        Provider { child, port }
    }

    fn base_url(&self, scenario: &str) -> String {
        format!("http://127.0.0.1:{}/{scenario}", self.port)
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Run `jig bench` against the mock target through the mock provider, keylessly,
/// and return the parsed `--json` document.
fn run_direct_bench(provider: &Provider, scenario: &str, runs: usize) -> Value {
    let out = Command::new(jig_bin())
        .arg("bench")
        .arg("--stdio")
        .arg(quoted(&mock_bin(), ""))
        .arg("--task")
        .arg("Book a table for two outdoors on the first of January")
        .arg("--base-url")
        .arg(provider.base_url(scenario))
        .arg("--no-auth")
        .arg("--runs")
        .arg(runs.to_string())
        .arg("--json")
        .env_remove("JIG_BENCH_BASE_URL")
        .env_remove("JIG_BENCH_API_KEY")
        .output()
        .expect("spawn jig bench");
    assert!(
        out.status.success(),
        "jig bench failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("bench --json emits valid JSON")
}

/// Reduce either report's distribution to a comparable shape.
fn shape(distribution: &Value) -> Value {
    json!({
        "total": distribution["total"],
        "selected": distribution["selected"],
        "hallucinated": distribution["hallucinated"],
        "noTool": distribution["noTool"],
        "providerError": distribution["providerError"],
        "consistent": distribution["consistent"],
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The headline parity test: identical selections through the provider API and
/// through MCP sampling must yield an identical distribution.
#[test]
fn sampling_and_direct_api_paths_agree_on_the_distribution() {
    const RUNS: usize = 4;
    let provider = Provider::start();

    // The provider's `reservation` scenario picks `make_reservation` with these
    // arguments on every hit; the script says exactly the same thing.
    let direct = run_direct_bench(&provider, "reservation", RUNS);
    let direct_shape = shape(&direct["models"][0]["distribution"]);

    let script = vec![pick("make_reservation", reservation_args()); RUNS];
    let sampled = run_host("bench_server", bench_args(RUNS), &script, &[]);
    assert_eq!(sampled["isError"], false, "{sampled}");
    let sampled_shape = shape(&sampled["structuredContent"]["distribution"]);

    assert_eq!(
        sampled_shape, direct_shape,
        "the sampling path measured a different distribution than the API path"
    );
    assert_eq!(sampled_shape["selected"][0]["tool"], "make_reservation");
    assert_eq!(sampled_shape["selected"][0]["count"], RUNS);
    assert_eq!(sampled_shape["consistent"], true);
}

/// The same parity, with a *mixed* distribution — the case that actually
/// exercises the aggregation rather than a single repeated answer.
#[test]
fn a_mixed_distribution_matches_across_both_paths() {
    const RUNS: usize = 4;
    let provider = Provider::start();

    // `alternate` picks echo on even hits and make_reservation on odd ones.
    let direct = run_direct_bench(&provider, "alternate", RUNS);
    let direct_shape = shape(&direct["models"][0]["distribution"]);

    let script = vec![
        pick("echo", json!({ "text": "hello" })),
        pick("make_reservation", reservation_args()),
        pick("echo", json!({ "text": "hello" })),
        pick("make_reservation", reservation_args()),
    ];
    let sampled = run_host("bench_server", bench_args(RUNS), &script, &[]);
    let sampled_shape = shape(&sampled["structuredContent"]["distribution"]);

    assert_eq!(sampled_shape, direct_shape);
    assert_eq!(sampled_shape["consistent"], false);
    assert_eq!(sampled_shape["total"], RUNS);
}

/// Every outcome in the taxonomy must be reachable through sampling, using the
/// *same* classification rules as the API path — including invalid arguments,
/// hallucinated names, and a plain text answer.
#[test]
fn the_full_outcome_taxonomy_is_reachable_through_sampling() {
    let script = vec![
        // A clean selection.
        pick("echo", json!({ "text": "hello" })),
        // Valid JSON, invalid against the schema (`date` missing, `size` a
        // string, `seating` outside the enum).
        pick(
            "make_reservation",
            json!({ "party": { "size": "two", "seating": "rooftop" } }),
        ),
        // A tool the target does not expose.
        pick("no_such_tool", json!({ "q": "x" })),
        // No tool at all.
        json!({ "tool": null, "answer": "Nothing here fits that request." }).to_string(),
    ];
    let result = run_host("bench_server", bench_args(4), &script, &[]);
    let doc = &result["structuredContent"];

    let dist = &doc["distribution"];
    assert_eq!(dist["total"], 4);
    assert_eq!(dist["noTool"], 1);
    assert_eq!(dist["hallucinated"][0]["name"], "no_such_tool");

    let runs = doc["results"].as_array().expect("results");
    assert_eq!(runs[0]["outcome"]["type"], "selected");
    assert_eq!(runs[0]["outcome"]["argsValid"], true);
    // The shared JSON-Schema validator ran on the sampled arguments too.
    assert_eq!(runs[1]["outcome"]["type"], "selected");
    assert_eq!(
        runs[1]["outcome"]["argsValid"], false,
        "sampled arguments must be schema-validated exactly like API ones"
    );
    assert_eq!(runs[2]["outcome"]["type"], "hallucinated_tool");
    assert_eq!(runs[3]["outcome"]["type"], "no_tool");
}

/// Honesty: the report carries whatever model identity the host gave, and says
/// the host chose it.
#[test]
fn the_host_reported_model_is_recorded_verbatim() {
    let script = vec![pick("echo", json!({ "text": "hello" }))];
    let result = run_host(
        "bench_server",
        bench_args(2),
        &script,
        &[("JIG_FIXTURE_MODEL", "some-host-model-9000")],
    );
    let doc = &result["structuredContent"];

    assert_eq!(doc["hostModels"][0], "some-host-model-9000");
    assert_eq!(doc["modelSelectedBy"], "host");
    assert_eq!(doc["modelAccess"], "mcp-sampling");
    assert_eq!(doc["keyless"], true);
    assert_eq!(doc["results"][0]["hostModel"], "some-host-model-9000");

    let summary = result["content"][0]["text"].as_str().expect("a summary");
    assert!(summary.contains("some-host-model-9000"), "{summary}");
    assert!(summary.contains("chosen by the host"), "{summary}");
}

/// Honesty, the harder half: a host that names no model must be recorded as
/// unknown — never silently attributed to some plausible default.
#[test]
fn a_host_that_names_no_model_is_recorded_as_unknown() {
    let script = vec![pick("echo", json!({ "text": "hello" }))];
    let result = run_host(
        "bench_server",
        bench_args(2),
        &script,
        &[("JIG_FIXTURE_MODEL", "none")],
    );
    let doc = &result["structuredContent"];

    let label = doc["modelLabel"].as_str().expect("a model label");
    assert!(
        label.contains("unknown") && label.contains("host-selected"),
        "an unnamed host model must read as unknown, got {label:?}"
    );
    assert_eq!(doc["hostModels"].as_array().unwrap().len(), 1);
    // And nothing anywhere claims a concrete vendor model.
    let text = serde_json::to_string(doc).unwrap();
    assert!(!text.contains("gpt-4o"), "{text}");
    assert!(!text.contains("claude-sonnet"), "{text}");
}

/// A host that refuses (the spec's own "User rejected sampling request") is
/// recorded as an error per run, not as a silent no-selection.
#[test]
fn a_refusing_host_is_recorded_as_an_error_not_a_no_tool() {
    let script = vec![pick("echo", json!({ "text": "hello" }))];
    let result = run_host(
        "bench_server",
        bench_args(2),
        &script,
        &[("JIG_FIXTURE_REJECT", "1")],
    );
    let doc = &result["structuredContent"];

    assert_eq!(doc["distribution"]["providerError"], 2);
    assert_eq!(doc["distribution"]["noTool"], 0);
    let detail = doc["results"][0]["outcome"]["detail"]
        .as_str()
        .expect("a detail");
    assert!(
        detail.contains("User rejected sampling request"),
        "{detail}"
    );
}

/// The negative control for the whole path: the very same fixture, with the
/// `sampling` capability withheld, must fail loudly instead of producing a
/// result.
#[test]
fn a_host_without_the_sampling_capability_gets_the_actionable_refusal() {
    let out = Command::new(mock_bin())
        .arg("--sampling-client")
        .arg("--no-sampling")
        .env("JIG_FIXTURE_TOOL", "bench_server")
        .env("JIG_FIXTURE_ARGS", bench_args(2).to_string())
        .env("JIG_FIXTURE_SCRIPT", "[]")
        .arg("--")
        .arg(jig_bin())
        .arg("serve")
        .output()
        .expect("spawn the scripted host");
    assert!(out.status.success());

    let stdout = String::from_utf8_lossy(&out.stdout);
    let frame: Value = serde_json::from_str(stdout.lines().last().expect("output")).expect("JSON");
    let result = &frame["result"];

    assert_eq!(result["isError"], true);
    assert!(result.get("structuredContent").is_none());
    let text = result["content"][0]["text"].as_str().expect("a message");
    assert!(text.contains("does not support MCP sampling"), "{text}");
    assert!(text.contains("--base-url"), "{text}");
    assert!(text.contains("ANTHROPIC_API_KEY"), "{text}");
}

/// The keyless tools reached through `jig serve` must work on a host with no
/// sampling at all — the point being that only `bench_server` ever needed it.
#[test]
fn the_other_tools_work_on_a_host_without_sampling() {
    let out = Command::new(mock_bin())
        .arg("--sampling-client")
        .arg("--no-sampling")
        .env("JIG_FIXTURE_TOOL", "check_server")
        .env(
            "JIG_FIXTURE_ARGS",
            json!({ "stdio": quoted(&mock_bin(), "") }).to_string(),
        )
        .env("JIG_FIXTURE_SCRIPT", "[]")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .arg("--")
        .arg(jig_bin())
        .arg("serve")
        .output()
        .expect("spawn the scripted host");
    assert!(out.status.success());

    let stdout = String::from_utf8_lossy(&out.stdout);
    let frame: Value = serde_json::from_str(stdout.lines().last().expect("output")).expect("JSON");
    let result = &frame["result"];
    assert_eq!(result["isError"], false, "{result}");
    assert_eq!(
        result["structuredContent"]["server"]["name"],
        "jig-mock-server"
    );
    assert!(result["structuredContent"]["composite"].is_number());
}
