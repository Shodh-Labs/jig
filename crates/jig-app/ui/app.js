// jig workbench — the webview half.
//
// This file renders. It does not decide. Every score, band, threshold, token
// count, and error sentence arrives already computed from Rust; the only
// numbers computed here are pixel positions. That split is deliberate: the
// rubric lives in `jig-core`, its presentation rules in `crates/jig-app/src/dto.rs`,
// and if a threshold ever needs changing it must change there, once, for the
// CLI and the app together.
//
// The webview never speaks MCP. It has no network access of any kind — the CSP
// permits `self` and the Tauri IPC channel, nothing else.

"use strict";

const { invoke } = window.__TAURI__.core;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

const state = {
  discovered: [],
  selected: null,      // name of the selected discovered server
  connected: null,     // ConnectResult
  wire: null,          // WireSnapshot
  selectedSpan: null,  // seq of the inspected span
  report: null,
  context: null,
  model: "gpt-4o",
  polling: null,
  busy: false,
};

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

const $ = (id) => document.getElementById(id);

/** Escape the five HTML-significant characters.
 *
 *  Every string that reaches the DOM through innerHTML passes through here.
 *  Tool names and descriptions are attacker-controlled — they come from whatever
 *  server the user pointed at — and `jig-cli` locks this behaviour with a
 *  `hostile_tool_name_is_escaped_not_executed` test. The app owes the same
 *  guarantee. */
function esc(s) {
  return String(s ?? "")
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

/** Escape first, then turn backtick pairs into mono spans — the same order as
 *  `report.rs::render_inline`, so markup in a finding can never execute. An
 *  unbalanced trailing backtick auto-closes. */
function inlineCode(s) {
  const parts = esc(s).split("`");
  let out = "";
  for (let i = 0; i < parts.length; i++) {
    out += i % 2 === 0 ? parts[i] : `<span class="mono">${parts[i]}</span>`;
  }
  return out;
}

/** Thousands separators, matching the report card's `commas()`. */
function commas(n) {
  return String(n).replace(/\B(?=(\d{3})+(?!\d))/g, ",");
}

/** Format a duration given in microseconds, choosing a unit the way the design
 *  prototype's axis labels do: µs under a millisecond, ms under a second. */
function dur(micros) {
  if (micros === null || micros === undefined) return "—";
  if (micros < 1000) return `${micros} µs`;
  if (micros < 1_000_000) return `${(micros / 1000).toFixed(1)} ms`;
  return `${(micros / 1_000_000).toFixed(2)} s`;
}

/** An absolute offset from session start, for the `t+` pills. */
function stamp(micros) {
  if (micros < 1_000_000) return `t+${(micros / 1000).toFixed(1)} ms`;
  return `t+${(micros / 1_000_000).toFixed(3)} s`;
}

function setBusy(on, label) {
  state.busy = on;
  const lamp = $("lamp");
  if (on) {
    lamp.className = "lamp busy";
    lamp.textContent = label || "working";
  } else if (state.connected) {
    lamp.className = "lamp live";
    lamp.textContent = "live";
  } else {
    lamp.className = "lamp";
    lamp.textContent = "idle";
  }
}

/** Render an error exactly as it arrived from Rust.
 *
 *  `JigError`'s Display strings are specific and actionable by design — a
 *  timeout names the method and the elapsed time, an unresolved server name
 *  lists the names that do exist. Wrapping them in "something went wrong" would
 *  throw away the whole point, so the message is shown verbatim. */
function showError(containerId, err) {
  const msg = typeof err === "string" ? err : (err && err.message) || String(err);
  $(containerId).innerHTML =
    `<div class="error"><span class="etitle">error</span>${esc(msg)}</div>`;
}

function clearError(containerId) {
  $(containerId).innerHTML = "";
}

// ---------------------------------------------------------------------------
// JSON rendering — brass keys, as in the design prototype's inspector
// ---------------------------------------------------------------------------

function renderJson(value, indent = 0) {
  const pad = "  ".repeat(indent);
  const padIn = "  ".repeat(indent + 1);

  if (value === null) return `<span class="jnull">null</span>`;
  if (typeof value === "boolean") return `<span class="jb">${value}</span>`;
  if (typeof value === "number") return `<span class="jn">${value}</span>`;
  if (typeof value === "string") return `<span class="js">${esc(JSON.stringify(value))}</span>`;

  if (Array.isArray(value)) {
    if (value.length === 0) return "[]";
    const items = value.map((v) => padIn + renderJson(v, indent + 1));
    return `[\n${items.join(",\n")}\n${pad}]`;
  }

  const keys = Object.keys(value);
  if (keys.length === 0) return "{}";
  const items = keys.map(
    (k) => `${padIn}<span class="jk">${esc(JSON.stringify(k))}</span>: ${renderJson(value[k], indent + 1)}`
  );
  return `{\n${items.join(",\n")}\n${pad}}`;
}

// ---------------------------------------------------------------------------
// Tabs
// ---------------------------------------------------------------------------

function selectPane(name) {
  document.querySelectorAll(".tab").forEach((t) => {
    t.setAttribute("aria-selected", String(t.dataset.pane === name));
  });
  document.querySelectorAll(".pane").forEach((p) => {
    p.classList.toggle("active", p.id === `pane-${name}`);
  });
  if (name === "wire") refreshWire();
}

document.querySelectorAll(".tab").forEach((t) => {
  t.addEventListener("click", () => {
    if (!t.disabled) selectPane(t.dataset.pane);
  });
});

function setConnectedChrome(on) {
  ["tab-wire", "tab-report", "tab-context"].forEach((id) => {
    $(id).disabled = !on;
  });
  $("btn-disconnect").disabled = !on;
  $("btn-check").disabled = !on;
}

// ---------------------------------------------------------------------------
// Connect pane
// ---------------------------------------------------------------------------

async function rescan() {
  try {
    const d = await invoke("discover_servers");
    state.discovered = d.entries;
    renderServerList(d);
  } catch (e) {
    showError("connect-error", e);
  }
}

function renderServerList(d) {
  const list = $("serverlist");
  if (!d.entries.length) {
    list.innerHTML =
      `<div class="empty">no servers found in the standard config locations</div>`;
  } else {
    list.innerHTML = d.entries
      .map(
        (e) => `
        <button class="srow${e.disabled ? " disabled" : ""}" data-name="${esc(e.name)}"
                aria-selected="${state.selected === e.name}">
          <span>
            <span class="sname">${esc(e.name)}</span>
            <span class="ssum">${esc(e.summary)}</span>
          </span>
          <span class="ssrc">${esc(e.source)}</span>
        </button>`
      )
      .join("");
    list.querySelectorAll(".srow").forEach((row) => {
      row.addEventListener("click", () => {
        state.selected = row.dataset.name;
        // Selecting a discovered server clears the manual boxes, so there is
        // never ambiguity about what Connect will actually do.
        $("manual-stdio").value = "";
        $("manual-http").value = "";
        renderServerList({ entries: state.discovered, warnings: d.warnings });
      });
    });
  }

  $("discovery-warnings").innerHTML = d.warnings.length
    ? `<div class="error" style="margin-top:.8rem"><span class="etitle">${d.warnings.length} config warning${
        d.warnings.length === 1 ? "" : "s"
      }</span>${d.warnings.map(esc).join("\n")}</div>`
    : "";
}

/** Work out what the user asked for. Manual entry wins over a list selection
 *  only when it is non-empty, and stdio wins over HTTP if somehow both are. */
function currentTarget() {
  const stdio = $("manual-stdio").value.trim();
  const http = $("manual-http").value.trim();
  if (stdio) {
    // Splitting happens in Rust (session::split_command) so quoting rules are
    // tested once; here we just hand over the raw line as a single command with
    // no args when it has no spaces, else let Rust split it.
    return { kind: "stdio", raw: stdio };
  }
  if (http) return { kind: "http", url: http };
  if (state.selected) return { kind: "discovered", name: state.selected };
  return null;
}

function connectOptions() {
  const t = parseInt($("opt-timeout").value, 10);
  const m = parseInt($("opt-maxbytes").value, 10);
  return {
    timeoutSecs: Number.isFinite(t) && t >= 0 ? t : 30,
    maxMessageBytes: Number.isFinite(m) && m >= 0 ? m : 67108864,
  };
}

async function doConnect() {
  const target = currentTarget();
  if (!target) {
    showError("connect-error", "pick a discovered server, or enter a command or URL");
    return;
  }
  clearError("connect-error");
  setBusy(true, "connecting");
  $("btn-connect").disabled = true;
  $("handshake").innerHTML = `<div class="empty">handshaking…</div>`;

  try {
    // A raw stdio line is split in Rust so the quoting rules live in one tested
    // place. `split_command` is exposed through the command's argument shape.
    let payload;
    if (target.kind === "stdio") {
      const parts = splitCommandLine(target.raw);
      payload = { kind: "stdio", command: parts[0], args: parts.slice(1) };
    } else {
      payload = target;
    }

    const r = await invoke("connect", { target: payload, options: connectOptions() });
    state.connected = r;
    state.report = null;
    state.context = null;
    state.selectedSpan = null;
    renderHandshake(r);
    setConnectedChrome(true);
    $("count-context").textContent = `·${r.toolCount}`;
    updateSessionMeta();
    startPolling();
    await loadContext();
  } catch (e) {
    state.connected = null;
    setConnectedChrome(false);
    $("handshake").innerHTML = "";
    showError("connect-error", e);
    updateSessionMeta();
  } finally {
    $("btn-connect").disabled = false;
    setBusy(false);
  }
}

/** A minimal mirror of `session::split_command` for the manual box.
 *
 *  The authoritative implementation is in Rust and is what actually runs; this
 *  copy exists only so the webview can present the command as a program plus
 *  arguments before sending it. Kept deliberately tiny and quote-aware. */
function splitCommandLine(line) {
  const parts = [];
  let cur = "";
  let inQuotes = false;
  let hasToken = false;
  for (const c of line) {
    if (c === '"') {
      inQuotes = !inQuotes;
      hasToken = true;
    } else if (/\s/.test(c) && !inQuotes) {
      if (hasToken) {
        parts.push(cur);
        cur = "";
        hasToken = false;
      }
    } else {
      cur += c;
      hasToken = true;
    }
  }
  if (hasToken) parts.push(cur);
  return parts;
}

function renderHandshake(r) {
  const caps = r.capabilityKeys.length
    ? r.capabilityKeys.map((k) => `<span class="chip">${esc(k)}</span>`).join("")
    : `<span class="chip plain">none advertised</span>`;

  const toolNote = r.toolsAdvertised
    ? `${r.toolCount} tool${r.toolCount === 1 ? "" : "s"}`
    : `not advertised`;

  $("handshake").innerHTML = `
    <div class="readouts">
      <div class="ro">
        <div class="n">${esc(r.server.name)}</div>
        <div class="l">server</div>
        <div class="src">v${esc(r.server.version)}</div>
      </div>
      <div class="ro">
        <div class="n num">${esc(r.protocolVersion)}</div>
        <div class="l">protocol</div>
        <div class="src">${esc(r.transport)} transport</div>
      </div>
      <div class="ro">
        <div class="n num">${commas(r.toolCount)}</div>
        <div class="l">tools</div>
        <div class="src">${esc(toolNote)}</div>
      </div>
      <div class="ro">
        <div class="n num">${commas(r.handshakeMs)} ms</div>
        <div class="l">handshake</div>
        <div class="src">${commas(r.resourceCount)} resources · ${commas(r.promptCount)} prompts</div>
      </div>
    </div>
    <div style="margin-top:1.2rem">
      <span class="label">Capabilities</span>
      <div class="chips">${caps}</div>
    </div>
    ${
      r.instructions
        ? `<div style="margin-top:1.2rem">
             <span class="label">Instructions</span>
             <div class="well" style="padding:.8rem .9rem">
               <div class="schema">${esc(r.instructions)}</div>
             </div>
           </div>`
        : ""
    }`;
}

function updateSessionMeta() {
  const m = $("sessionmeta");
  if (!state.connected) {
    m.textContent = "no session — pick a server in Connect";
    return;
  }
  const r = state.connected;
  m.innerHTML = `session <b>${esc(r.server.name)} v${esc(r.server.version)}</b> · ${esc(
    r.transport
  )} · proto ${esc(r.protocolVersion)}`;
}

async function doDisconnect() {
  stopPolling();
  setBusy(true, "closing");
  try {
    await invoke("disconnect");
  } catch (e) {
    showError("connect-error", e);
  } finally {
    state.connected = null;
    state.wire = null;
    state.report = null;
    state.context = null;
    setConnectedChrome(false);
    $("handshake").innerHTML = "";
    $("count-wire").textContent = "";
    $("count-context").textContent = "";
    updateSessionMeta();
    setBusy(false);
    selectPane("connect");
  }
}

// ---------------------------------------------------------------------------
// Wire pane
// ---------------------------------------------------------------------------

function startPolling() {
  stopPolling();
  // The tap is poll-only in `jig-core` — it has no channel or callback — so the
  // wire view samples it. 500ms is well under human perception for "live" and
  // far cheaper than the deep clone `entries()` costs at higher rates.
  state.polling = setInterval(refreshWire, 500);
  refreshWire();
}

function stopPolling() {
  if (state.polling) clearInterval(state.polling);
  state.polling = null;
}

async function refreshWire() {
  if (!state.connected) return;
  try {
    const snap = await invoke("wire_snapshot");
    state.wire = snap;
    $("count-wire").textContent = `·${snap.spans.length}`;
    if ($("pane-wire").classList.contains("active")) renderWire();
  } catch (e) {
    // A failed poll must not spam the UI — the next tick will retry.
    console.error("wire poll failed:", e);
  }
}

/** Project a timestamp onto the folded axis, returning 0..1.
 *
 *  A faithful mirror of `wire::project` in Rust. It is duplicated rather than
 *  round-tripped because it is called once per span per frame and it computes a
 *  pixel position, not a fact — the Rust side remains authoritative for the
 *  axis itself (which stretches fold and which do not). `wire.rs` carries the
 *  tests that pin the behaviour. */
function project(axis, foldWidth, t) {
  if (!axis.length) return 0;
  const drawnTotal = axis.reduce(
    (a, s) => a + (s.folded ? 0 : s.endMicros - s.startMicros),
    0
  );
  const foldCount = axis.filter((s) => s.folded).length;
  const foldShare = Math.min(foldCount * foldWidth, 0.9);
  const drawnShare = 1 - foldShare;

  let pos = 0;
  for (const seg of axis) {
    const d = seg.endMicros - seg.startMicros;
    const w = seg.folded
      ? Math.min(foldWidth, foldShare)
      : drawnTotal > 0
      ? drawnShare * (d / drawnTotal)
      : drawnShare / axis.length;

    if (t <= seg.startMicros) return Math.max(0, Math.min(1, pos));
    if (t <= seg.endMicros) {
      const frac = d === 0 ? 0 : (t - seg.startMicros) / d;
      return Math.max(0, Math.min(1, pos + w * frac));
    }
    pos += w;
  }
  return Math.max(0, Math.min(1, pos));
}

/** The x-extent of each folded stretch, as percentages, for the hatched bands. */
function foldBands(axis, foldWidth) {
  return axis
    .filter((s) => s.folded)
    .map((s) => ({
      left: project(axis, foldWidth, s.startMicros) * 100,
      right: project(axis, foldWidth, s.endMicros) * 100,
      hidden: s.endMicros - s.startMicros,
    }));
}

function renderWire() {
  const snap = state.wire;
  const body = $("wire-body");
  if (!snap || !snap.spans.length) {
    body.innerHTML = `<div class="empty">no traffic yet</div>`;
    return;
  }

  const axis = snap.axis;
  const fw = snap.foldWidthFraction;
  const bands = foldBands(axis, fw);
  const bandHtml = bands
    .map(
      (b) =>
        `<div class="foldband" style="left:${b.left.toFixed(2)}%;width:${(
          b.right - b.left
        ).toFixed(2)}%"></div>`
    )
    .join("");

  // Axis ticks: the ends of every drawn segment, plus a label inside each fold.
  const ticks = [];
  const seen = new Set();
  for (const seg of axis) {
    for (const t of [seg.startMicros, seg.endMicros]) {
      if (seen.has(t)) continue;
      seen.add(t);
      ticks.push(t);
    }
  }
  ticks.sort((a, b) => a - b);
  const first = ticks[0];
  const last = ticks[ticks.length - 1];

  // Drop labels that would collide. A folded axis puts marks very close
  // together on either side of a fold (the whole point is that little drawn
  // width separates two distant times), so without this the labels overprint
  // each other and the scale becomes unreadable. The first and last ticks are
  // always kept — they anchor the axis.
  const MIN_GAP_PCT = 9;
  const placed = [];
  for (const t of ticks) {
    const p = project(axis, fw, t) * 100;
    const isEdge = t === first || t === last;
    const crowded = placed.some((q) => Math.abs(q.p - p) < MIN_GAP_PCT);
    if (isEdge || !crowded) placed.push({ t, p });
  }
  // If the last tick crowded an earlier one, that earlier one yields.
  const lastP = placed[placed.length - 1].p;
  const kept = placed.filter(
    (q, i) => i === 0 || i === placed.length - 1 || Math.abs(q.p - lastP) >= MIN_GAP_PCT
  );

  const tickHtml = kept
    .map(({ t, p }) => {
      // The end ticks are anchored inward so their labels stay inside the
      // panel; everything between is centred on its mark.
      const edge = t === first ? " first" : t === last ? " last" : "";
      return `<div class="tick${edge}" style="left:${p.toFixed(2)}%"><span>${
        t === first ? "0" : dur(t - first)
      }</span></div>`;
    })
    .join("");

  const foldLabels = bands
    .map(
      (b) =>
        `<div class="foldlabel" style="left:${((b.left + b.right) / 2).toFixed(
          2
        )}%">⟨fold: ${dur(b.hidden)}⟩</div>`
    )
    .join("");

  const lanes = snap.spans.map((s) => renderLane(s, axis, fw, bandHtml)).join("");

  const inspector = state.selectedSpan !== null
    ? renderInspector(snap.spans.find((s) => s.seq === state.selectedSpan))
    : null;

  body.innerHTML = `
    <div class="wire-layout${inspector ? "" : " no-inspector"}">
      <div class="card timeline">
        <div style="position:relative">
          <div class="axis">${bandHtml}${tickHtml}${foldLabels}</div>
        </div>
        <div class="lanes">${lanes}</div>
        <div class="legend">
          <div><span class="swatch" style="background:var(--ember)"></span> request → response</div>
          <div><span class="swatch striped"></span> crosses a fold</div>
          <div><span class="swatch dot" style="background:var(--ember)"></span> notification (jig → server, no reply)</div>
          <div><span class="swatch diamond" style="background:var(--jade)"></span> unsolicited server notification</div>
          <div><span class="swatch dot" style="background:var(--brass)"></span> server → jig request</div>
        </div>
      </div>
      ${inspector ? `<aside class="card inspector">${inspector}</aside>` : ""}
    </div>`;

  body.querySelectorAll(".lane").forEach((el) => {
    el.addEventListener("click", () => {
      const seq = parseInt(el.dataset.seq, 10);
      state.selectedSpan = state.selectedSpan === seq ? null : seq;
      renderWire();
    });
  });
  const close = body.querySelector(".insp-close");
  if (close) {
    close.addEventListener("click", () => {
      state.selectedSpan = null;
      renderWire();
    });
  }
}

function renderLane(s, axis, fw, bandHtml) {
  const start = project(axis, fw, s.startMicros) * 100;
  const selected = state.selectedSpan === s.seq;

  let mark;
  let cls = "";
  let sub;

  if (s.kind === "request" || s.kind === "server_request") {
    if (s.pending) {
      mark = `<div class="bar pending" style="left:${start.toFixed(2)}%;width:2%"></div>`;
      sub = `${s.id !== null ? `id ${JSON.stringify(s.id)} · ` : ""}pending`;
    } else {
      const end = project(axis, fw, s.endMicros) * 100;
      // A span that spends part of its life inside a folded stretch is striped:
      // its drawn width is not proportional to its real duration, and pretending
      // otherwise would be a lie about the measurement.
      const crosses = axis.some(
        (seg) => seg.folded && seg.startMicros >= s.startMicros && seg.endMicros <= s.endMicros
      );
      const barCls = s.isError ? "bar err" : crosses ? "bar crosses" : "bar";
      mark = `<div class="${barCls}" style="left:${start.toFixed(2)}%;width:${Math.max(
        end - start,
        0.3
      ).toFixed(2)}%"></div>`;
      sub = `${s.id !== null ? `id ${JSON.stringify(s.id)} · ` : ""}${dur(s.durationMicros)}${
        s.isError ? " · error" : ""
      }`;
    }
    if (s.kind === "server_request") {
      cls = "";
      sub = `server → jig · ${sub}`;
    }
  } else if (s.kind === "server_notification") {
    cls = "push";
    mark = `<div class="mark push" style="left:${start.toFixed(2)}%"></div>`;
    sub = "server push · no id";
  } else if (s.kind === "client_notification") {
    mark = `<div class="mark client" style="left:${start.toFixed(2)}%"></div>`;
    sub = "jig → server · no reply";
  } else {
    cls = "pollution";
    mark = `<div class="mark pollution" style="left:${start.toFixed(2)}%"></div>`;
    sub = s.offset !== null && s.offset !== undefined
      ? `not JSON-RPC · stdout byte ${commas(s.offset)}`
      : "not JSON-RPC · stdout";
  }

  const name =
    s.kind === "pollution" ? "stdout pollution" : s.method || "(response)";

  return `
    <button class="lane ${cls}" data-seq="${s.seq}" aria-selected="${selected}">
      <span class="meth">
        <span class="mname">${esc(name)}</span>
        <span class="msub">${esc(sub)}</span>
      </span>
      <span class="track">${bandHtml}${mark}</span>
    </button>`;
}

function renderInspector(s) {
  if (!s) return null;

  const pills = [];
  if (s.durationMicros !== null && s.durationMicros !== undefined) {
    pills.push(`<span class="pill ${s.isError ? "bad" : "ok"}">round trip ${dur(s.durationMicros)}</span>`);
  }
  pills.push(`<span class="pill">${stamp(s.startMicros)}</span>`);
  if (s.pending) pills.push(`<span class="pill warn">no response</span>`);
  if (s.kind === "server_notification") pills.push(`<span class="pill ok">server push</span>`);
  if (s.kind === "pollution") pills.push(`<span class="pill bad">breaks strict clients</span>`);
  if (s.offset !== null && s.offset !== undefined) {
    pills.push(`<span class="pill">stdout byte ${commas(s.offset)}</span>`);
  }

  const title = s.kind === "pollution" ? "stdout pollution" : s.method || "(response)";

  // Label the wells by who spoke, matching the design prototype.
  const outbound = s.kind === "request" || s.kind === "client_notification";
  const reqLabel = outbound ? "request — jig → server" : "message — server → jig";
  const resLabel = outbound ? "response — server → jig" : "response — jig → server";

  let note = "";
  if (s.kind === "server_notification") {
    note = `<p class="cap">An unsolicited notification: the server spoke without being asked. Legal, and worth knowing about — a client that ignores these will miss state changes.</p>`;
  } else if (s.kind === "pollution") {
    note = `<p class="cap">This line arrived on stdout but is not JSON-RPC. Strict MCP clients treat the stream as protocol-only; anything else here can break the session. Log to stderr instead.</p>`;
  } else if (s.pending) {
    note = `<p class="cap">This request has no response in the captured log — either it is still in flight, or the server never answered.</p>`;
  }

  return `
    <div class="insp-head">
      <span class="insp-title">${esc(title)}</span>
      <button class="insp-close" title="close">✕</button>
    </div>
    <div class="pills">${pills.join("")}</div>
    ${note}
    ${
      s.request
        ? `<div class="jsonwell">
             <span class="label">${esc(reqLabel)}</span>
             <div class="well"><pre>${renderJson(s.request)}</pre></div>
           </div>`
        : ""
    }
    ${
      s.response
        ? `<div class="jsonwell">
             <span class="label">${esc(resLabel)}</span>
             <div class="well"><pre>${renderJson(s.response)}</pre></div>
           </div>`
        : ""
    }`;
}

// ---------------------------------------------------------------------------
// Report card pane
// ---------------------------------------------------------------------------

async function runCheck() {
  clearError("report-body");
  setBusy(true, "grading");
  $("btn-check").disabled = true;
  $("report-body").innerHTML =
    `<div class="empty">opening a fresh session and grading…</div>`;
  try {
    const r = await invoke("run_check");
    state.report = r;
    renderReport(r);
  } catch (e) {
    $("report-body").innerHTML = "";
    showError("report-body", e);
  } finally {
    $("btn-check").disabled = false;
    setBusy(false);
  }
}

function renderReport(r) {
  const dims = r.dimensions
    .map((d) => {
      const bar = d.applicable
        ? `<span class="dimbar"><i style="width:${d.score}%;background:var(--${d.band})"></i></span>`
        : `<span class="dimbar"></span>`;
      const val = d.applicable ? d.score : "–";
      return `<div class="dim">
        <span class="lbl">${esc(d.label)} <small>·${d.weight}%</small></span>
        ${bar}
        <span class="v">${val}</span>
      </div>`;
    })
    .join("");

  const capnote = r.contextCap
    ? `<div class="capnote">${esc(r.contextCap.explanation)} — would have scored ${Math.round(
        r.contextCap.uncapped
      )}</div>`
    : "";

  const chart = r.chart;
  const chartRows = chart.tools
    .map(
      (t) => `<div class="crow">
        <span class="cname">${esc(t.name)}</span>
        <span class="ctrack">
          <i style="width:${t.fillPct.toFixed(1)}%"></i>
          ${chart.median > 0 ? `<span class="cmed" style="left:${chart.medianPct.toFixed(1)}%"></span>` : ""}
        </span>
        <span class="cval">${commas(t.tokens)}</span>
      </div>`
    )
    .join("");

  const findingRow = (f) =>
    `<div class="f"><span class="sev ${f.severityClass}">${esc(f.severity)}</span><span>${inlineCode(
      f.message
    )}<span class="fix">${inlineCode(f.fix)}</span></span></div>`;

  const advisor = r.advisor.length
    ? `<div class="sec">
         <h2>Advisor — tool-set findings (${r.advisor.length})</h2>
         <p class="cap">Deterministic detectors for the failure modes that make models pick the wrong tool. Not scored into the grade.</p>
         <div class="card flist">${r.advisor.map(findingRow).join("")}</div>
       </div>`
    : "";

  const topFixes = r.topFixes.length
    ? `<div class="sec">
         <h2>Top fixes, in order of impact</h2>
         <div class="card">
           <ol class="fixes">${r.topFixes
             .map(
               (f) =>
                 `<li>${inlineCode(f.message)}<span class="fix">${esc(
                   f.dimension
                 )} · ${inlineCode(f.fix)}</span></li>`
             )
             .join("")}</ol>
         </div>
       </div>`
    : "";

  const perDim = r.dimensions
    .filter((d) => d.findings.length)
    .map(
      (d) => `<div class="sec">
        <h2>${esc(d.label)} — ${d.applicable ? d.score : "n/a"}${
        d.heuristic ? ` <span class="cap" style="display:inline">(heuristic)</span>` : ""
      }</h2>
        <p class="cap">${esc(d.summary)}</p>
        <div class="card flist">${d.findings.map(findingRow).join("")}</div>
      </div>`
    )
    .join("");

  const toolCallout = r.toolCountCallout
    ? `<div class="co">
         <h3>Tool count: past the accuracy cliff</h3>
         <div class="big">${commas(r.toolCount)} tools</div>
         <p>Published measurements show model tool-selection accuracy degrading materially past ~30–50 tools. A mis-selected tool is a wrong <i>action</i>, not just a wrong answer.</p>
       </div>`
    : "";

  $("report-body").innerHTML = `
    <div class="instrument hero">
      <div class="scoreblock">
        <div class="n" style="color:var(--${r.gradeBand})">${r.composite}</div>
        <div class="g" style="background:var(--${r.gradeBand})">grade ${esc(r.grade)}</div>
        <div class="sub">out of 100 · composite of 5 weighted dimensions</div>
        ${capnote}
      </div>
      <div class="dims">${dims}</div>
    </div>

    <div class="callouts">
      <div class="co">
        <h3>Context bill: every conversation starts here</h3>
        <div class="big">${commas(r.totalTokens)} tokens</div>
        <p>${esc(r.provenance.prose)}</p>
      </div>
      ${toolCallout}
    </div>

    ${
      chart.tools.length
        ? `<div class="sec">
             <h2>Where the tokens go — top ${chart.shown} of ${chart.total} tool${
             chart.total === 1 ? "" : "s"
           }</h2>
             <p class="cap">Priced with the exact gpt-4o tokenizer.${
               chart.median > 0
                 ? ` The vertical line marks the server's own median tool (${commas(
                     chart.median
                   )} tok).`
                 : ""
             }</p>
             <div class="card">${chartRows}</div>
             ${
               chart.topRatio
                 ? `<p class="cap" style="margin-top:.6rem">median tool = ${commas(
                     chart.median
                   )} tok · top tool <span class="mono">${esc(
                     chart.tools[0].name
                   )}</span> is ${chart.topRatio.toFixed(1)}× median</p>`
                 : ""
             }
           </div>`
        : ""
    }

    ${advisor}
    ${topFixes}
    ${perDim}

    <div class="honesty">
      <b>Honesty notes.</b> ${r.honestyNotes.map(esc).join(" ")}
      Graded by the same engine as <span class="mono">jig check</span> · ${esc(r.rubricVersion)}
    </div>`;
}

// ---------------------------------------------------------------------------
// Context pane
// ---------------------------------------------------------------------------

async function loadModels() {
  try {
    const models = await invoke("list_models");
    const sel = $("model-select");
    sel.innerHTML = models.map((m) => `<option value="${esc(m)}">${esc(m)}</option>`).join("");

    // Set the value on the element rather than trusting a `selected` attribute
    // in an innerHTML string: the webview may restore prior form state, which
    // would leave the dropdown naming one model while `state.model` priced
    // another. The two must never disagree — the exactness label on this pane
    // is a claim about which tokenizer produced the numbers beside it.
    if (!models.includes(state.model)) state.model = models[0];
    sel.value = state.model;
  } catch (e) {
    console.error("model list failed:", e);
  }
}

async function loadContext() {
  if (!state.connected) return;
  try {
    const c = await invoke("build_context", { model: state.model });
    state.context = c;
    renderContext(c);
  } catch (e) {
    $("context-body").innerHTML = "";
    showError("context-body", e);
  }
}

function renderContext(c) {
  const rows = c.tools
    .map(
      (t) => `<details class="toolrow">
        <summary class="toolsum">
          <span>
            <span class="tname">${esc(t.name)}</span>
            <span class="tdesc">${esc(t.description || "no description")}</span>
          </span>
          <span class="ttrack"><i style="width:${t.sharePct.toFixed(1)}%"></i></span>
          <span class="tval">${commas(t.tokens)}</span>
        </summary>
        <div class="toolbody">
          <pre class="schema">${esc(t.schemaLines.join("\n"))}</pre>
        </div>
      </details>`
    )
    .join("");

  const instr = c.instructionsTokens !== null && c.instructionsTokens !== undefined;

  $("context-body").innerHTML = `
    <div class="budget-strip">
      <div class="bcell">
        <div class="bn">${commas(c.totalTokens)}</div>
        <div class="bl">total tokens</div>
      </div>
      <div class="bcell">
        <div class="bn">${commas(c.toolsTokens)}</div>
        <div class="bl">tools</div>
      </div>
      <div class="bcell">
        <div class="bn">${commas(c.systemTokens)}</div>
        <div class="bl">system prompt</div>
      </div>
      <div class="bcell">
        <div class="bn">${esc(c.tokenizer)}</div>
        <div class="bl">${esc(c.exactness)}</div>
      </div>
    </div>

    ${
      instr
        ? `<p class="cap">The server also sent instructions worth ${commas(
            c.instructionsTokens
          )} tokens. They are counted but deliberately excluded from the total above, because bench does not send them.</p>`
        : ""
    }

    <div class="sec">
      <h2>Per-tool cost — heaviest first</h2>
      <p class="cap">Click a tool to see the schema exactly as it is rendered into the request body.</p>
      <div class="card" style="padding:.3rem .9rem">${rows || `<div class="empty">no tools</div>`}</div>
    </div>

    <div class="sec">
      <h2>Request body</h2>
      <p class="cap">The exact ${esc(c.apiModel)} request jig priced — nothing simulated.</p>
      <div class="well"><pre style="padding:.9rem;max-height:26rem;overflow:auto;font-family:var(--mono);font-size:.74rem;margin:0">${renderJson(
        c.body
      )}</pre></div>
    </div>`;
}

// ---------------------------------------------------------------------------
// Wiring
// ---------------------------------------------------------------------------

$("btn-connect").addEventListener("click", doConnect);
$("btn-disconnect").addEventListener("click", doDisconnect);
$("btn-rescan").addEventListener("click", rescan);
$("btn-check").addEventListener("click", runCheck);
$("model-select").addEventListener("change", (e) => {
  state.model = e.target.value;
  loadContext();
});

// Enter in either manual box connects.
["manual-stdio", "manual-http"].forEach((id) => {
  $(id).addEventListener("keydown", (e) => {
    if (e.key === "Enter") doConnect();
  });
  $(id).addEventListener("input", () => {
    // Typing a manual target clears any list selection.
    if ($(id).value.trim()) {
      state.selected = null;
      renderServerList({ entries: state.discovered, warnings: [] });
    }
  });
});

loadModels();
rescan();
