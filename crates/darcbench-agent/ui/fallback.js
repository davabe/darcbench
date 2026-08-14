/*
 * DARCBench built-in console.
 *
 * Served as a separate file rather than inline so the agent's Content-Security
 * -Policy can stay at `script-src 'self'` with no `unsafe-inline` escape hatch.
 *
 * Auth flow, mirroring the React UI:
 *   1. Read `?token=` from the URL.
 *   2. POST it to /api/v1/session, which sets an HttpOnly session cookie.
 *   3. Strip the token from the address bar so it does not linger in history.
 *   4. Keep the token in memory for the `Authorization` header, which mutating
 *      requests require (the cookie alone is refused, as CSRF protection).
 */
(function () {
  "use strict";

  var token = null;
  var runId = null;
  var source = null;
  var metrics = new Map();

  var $ = function (id) { return document.getElementById(id); };

  function setText(id, value) {
    var el = $(id);
    if (el) { el.textContent = value; }
  }

  function log(kind, detail) {
    var el = $("log");
    if (!el) { return; }
    var line = document.createElement("div");
    var label = document.createElement("b");
    label.textContent = kind;
    line.appendChild(label);
    if (detail) { line.appendChild(document.createTextNode(" " + detail)); }
    el.appendChild(line);
    // Keep the DOM bounded: a deep run emits thousands of events, and an
    // unbounded log would make the browser the slowest part of the system.
    while (el.childElementCount > 400) { el.removeChild(el.firstChild); }
    el.scrollTop = el.scrollHeight;
  }

  function api(path, options) {
    options = options || {};
    options.headers = options.headers || {};
    if (token) { options.headers["Authorization"] = "Bearer " + token; }
    options.credentials = "same-origin";
    return fetch(path, options).then(function (response) {
      if (!response.ok) {
        return response.json().catch(function () { return {}; }).then(function (body) {
          throw new Error(body.message || (response.status + " " + response.statusText));
        });
      }
      return response.json();
    });
  }

  function fmt(value, digits) {
    if (value === null || value === undefined || !isFinite(value)) { return "—"; }
    return Number(value).toFixed(digits === undefined ? 0 : digits);
  }

  function gib(bytes) {
    if (!bytes) { return "—"; }
    return (bytes / 1073741824).toFixed(1) + " GiB";
  }

  function setState(text, cls) {
    var el = $("state");
    if (!el) { return; }
    el.textContent = text;
    el.className = "state " + (cls || "");
  }

  // --- bootstrap ----------------------------------------------------------

  function bootstrap() {
    var params = new URLSearchParams(window.location.search);
    token = params.get("token");

    var ready = token
      ? api("/api/v1/session?token=" + encodeURIComponent(token), { method: "POST" })
          .then(function () {
            // Remove the secret from the address bar, history and any future
            // Referer header.
            window.history.replaceState({}, "", window.location.pathname);
          })
      : Promise.resolve();

    return ready
      .then(function () { return api("/api/v1/meta"); })
      .then(function (meta) {
        if (!meta.scoring_calibrated) {
          var banner = $("calibration");
          if (banner) { banner.hidden = false; }
          setText(
            "calibration-text",
            "Scoring model " + meta.scoring_model + " has not been calibrated against a physical " +
            "DARC-REF-1 reference machine. Raw measurements are real; the derived scores are " +
            "development output and are not comparable with any calibrated release."
          );
        }
        return api("/api/v1/profiles");
      })
      .then(function (data) {
        var select = $("profile");
        if (!select) { return; }
        select.innerHTML = "";
        data.profiles.forEach(function (profile) {
          var option = document.createElement("option");
          option.value = profile.key;
          option.textContent =
            profile.key + " (" + profile.nominal_minutes[0] + "–" +
            profile.nominal_minutes[1] + " min)" + (profile.available ? "" : " — unavailable");
          option.disabled = !profile.available;
          select.appendChild(option);
        });
        var firstAvailable = data.profiles.find(function (p) { return p.available; });
        if (firstAvailable) { select.value = firstAvailable.key; }
      })
      .catch(function (error) {
        log("error", error.message);
        setState("not authenticated", "bad");
      });
  }

  // --- run control ---------------------------------------------------------

  function start() {
    var select = $("profile");
    metrics.clear();
    renderMetrics();
    setText("s-total", "—");
    var categories = $("s-categories");
    if (categories) { categories.innerHTML = ""; }

    api("/api/v1/runs", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ profile: select ? select.value : "quick", force: true })
    })
      .then(function (run) {
        runId = run.run_id;
        log("run.accepted", runId);
        $("start").disabled = true;
        $("cancel").disabled = false;
        setState("running", "warn");
        listen(runId);
      })
      .catch(function (error) {
        log("error", error.message);
        setState("failed to start", "bad");
      });
  }

  function cancel() {
    if (!runId) { return; }
    $("cancel").disabled = true;
    setState("cancelling", "warn");
    api("/api/v1/runs/" + encodeURIComponent(runId) + "/cancel", { method: "POST" })
      .catch(function (error) { log("error", error.message); });
  }

  // --- event stream ---------------------------------------------------------

  function listen(id) {
    if (source) { source.close(); }
    // EventSource cannot set headers, so the session cookie authenticates the
    // stream. Streaming is read-only, so cookie auth carries no CSRF risk.
    source = new EventSource("/api/v1/runs/" + encodeURIComponent(id) + "/events");
    source.onmessage = function (event) { handle(JSON.parse(event.data)); };
    source.onerror = function () { log("stream", "disconnected; the browser will retry"); };
    ["run.created", "run.preflight.started", "run.preflight.completed", "module.queued",
     "module.preparing", "module.warmup", "module.started", "module.sample", "module.telemetry",
     "module.warning", "module.completed", "module.failed", "module.cancelled",
     "score.provisional", "score.final", "report.generated", "run.completed",
     "run.invalidated", "stream.heartbeat"].forEach(function (kind) {
      source.addEventListener(kind, function (event) { handle(JSON.parse(event.data)); });
    });
  }

  function handle(event) {
    switch (event.type) {
      case "run.created":
        log("run.created", event.profile + " · " + event.modules.length + " module(s)");
        break;

      case "run.preflight.completed":
        log("preflight", event.risk + (event.passed ? " · passed" : " · BLOCKED") +
          " · ~" + event.estimated_duration_s + "s");
        event.findings.forEach(function (finding) {
          log("  " + finding.severity, finding.check + ": " + finding.message);
        });
        break;

      case "module.started":
        setText("t-module", event.module.id);
        log("module.started", event.module.id + "@" + event.module.version);
        break;

      case "module.sample":
        if (!event.warmup) {
          metrics.set(event.metric_key, {
            key: event.metric_key, value: event.value, unit: event.unit
          });
          renderMetrics();
        }
        var progress = $("progress");
        if (progress) { progress.value = event.module_progress; }
        break;

      case "module.telemetry":
        setText("t-cpu", fmt(event.cpu_busy_pct, 1) + " %");
        setText("t-external", fmt(event.cpu_external_busy_pct, 1) + " %");
        setText("t-steal", fmt(event.cpu_steal_pct, 2) + " %");
        setText("t-load", fmt(event.load1, 2));
        setText("t-mem", gib(event.mem_used_bytes) + " / " + gib(event.mem_total_bytes));
        setText("t-freq", event.cpu_freq_mhz ? fmt(event.cpu_freq_mhz, 0) + " MHz" : "n/a");
        break;

      case "module.warning":
        log("warning", event.code + ": " + event.message);
        break;

      case "module.completed":
        event.result.metrics.forEach(function (metric) {
          metrics.set(metric.key, {
            key: metric.key,
            value: metric.value,
            unit: metric.unit,
            cv: metric.summary.cv
          });
        });
        renderMetrics();
        log("module.completed", event.result.module.id + " · " + event.result.status);
        break;

      case "module.failed":
        log("module.failed", event.module.id + ": " + event.error);
        break;

      case "score.provisional":
      case "score.final":
        renderScores(event);
        break;

      case "report.generated":
        setText("artifacts",
          "Bundle " + event.bundle_sha256 + " (" + event.bytes + " bytes). " +
          "Formats: " + event.formats.join(", ") + ".");
        break;

      case "run.completed":
        setState(event.state, event.state === "completed" ? "ok" : "bad");
        log("run.completed",
          event.state + " · verdict " + event.verdict.state +
          " · " + event.modules_completed + " completed, " + event.modules_failed + " failed");
        event.verdict.reasons.forEach(function (reason) {
          log("  verdict", typeof reason === "string" ? reason : JSON.stringify(reason));
        });
        finish();
        break;

      case "run.invalidated":
        setState("invalidated", "bad");
        finish();
        break;

      default:
        break;
    }
  }

  function finish() {
    if (source) { source.close(); source = null; }
    $("start").disabled = false;
    $("cancel").disabled = true;
    var progress = $("progress");
    if (progress) { progress.value = 1; }
    if (runId) {
      var reportLink = document.createElement("a");
      reportLink.href = "/api/v1/runs/" + encodeURIComponent(runId) + "/report";
      reportLink.textContent = "Open full HTML report";
      var artifacts = $("artifacts");
      if (artifacts) {
        artifacts.appendChild(document.createTextNode(" "));
        artifacts.appendChild(reportLink);
      }
    }
  }

  // --- rendering ---------------------------------------------------------------

  function renderMetrics() {
    var body = $("metrics");
    if (!body) { return; }
    body.innerHTML = "";
    if (metrics.size === 0) {
      var empty = document.createElement("tr");
      var cell = document.createElement("td");
      cell.colSpan = 4;
      cell.style.color = "var(--dim)";
      cell.textContent = "No results yet.";
      empty.appendChild(cell);
      body.appendChild(empty);
      return;
    }
    Array.from(metrics.values())
      .sort(function (a, b) { return a.key.localeCompare(b.key); })
      .forEach(function (metric) {
        var row = document.createElement("tr");
        // textContent throughout: metric keys and units come from the agent,
        // but building DOM nodes rather than HTML strings keeps injection
        // structurally impossible.
        [metric.key, fmt(metric.value, 2), metric.unit,
         metric.cv === null || metric.cv === undefined ? "—" : fmt(metric.cv * 100, 1) + " %"]
          .forEach(function (value, index) {
            var cell = document.createElement("td");
            if (index === 1 || index === 3) { cell.className = "num"; }
            cell.textContent = value;
            row.appendChild(cell);
          });
        body.appendChild(row);
      });
  }

  function renderScores(event) {
    setText("s-total", fmt(event.total, 0));
    var container = $("s-categories");
    if (!container) { return; }
    container.innerHTML = "";
    event.categories.forEach(function (category) {
      var tile = document.createElement("div");
      tile.className = "tile";
      var key = document.createElement("div");
      key.className = "k";
      key.textContent = category.label;
      var value = document.createElement("div");
      value.className = "v";
      value.textContent = fmt(category.score, 0);
      tile.appendChild(key);
      tile.appendChild(value);
      container.appendChild(tile);
    });
    if (event.uncalibrated) {
      var banner = $("calibration");
      if (banner) { banner.hidden = false; }
    }
  }

  // --- wire up -------------------------------------------------------------------

  document.addEventListener("DOMContentLoaded", function () {
    $("start").addEventListener("click", start);
    $("cancel").addEventListener("click", cancel);
    bootstrap();
  });
})();
