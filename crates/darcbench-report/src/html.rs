//! Self-contained HTML report rendering.
//!
//! The output has no external references at all: no CDN, no webfont, no image
//! request. A report is often opened from a laptop that cannot reach the server
//! it describes, and a report that phones home would also leak the fact that
//! someone opened it.
//!
//! Every dynamic value goes through [`escape`]. Inventory strings come from
//! `/proc` and `/sys` on a machine DARCBench does not control, so they are
//! untrusted input as far as this renderer is concerned.

use std::borrow::Cow;

use darcbench_protocol::ResultState;

use crate::bundle::Bundle;

/// Characters that must not reach the document as themselves.
fn needs_escaping(ch: char) -> bool {
    matches!(ch, '&' | '<' | '>' | '"' | '\'')
}

/// Escapes text for insertion into HTML element content or a quoted attribute.
///
/// Borrows when there is nothing to escape, which is the case for almost every
/// value in a report - metric keys, units, digests, versions. Only genuinely
/// hostile or unusual inventory strings pay for a copy.
pub fn escape(input: &str) -> Cow<'_, str> {
    let Some(first) = input.find(needs_escaping) else {
        return Cow::Borrowed(input);
    };
    let mut out = String::with_capacity(input.len() + 16);
    out.push_str(&input[..first]);
    for ch in input[first..].chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(ch),
        }
    }
    Cow::Owned(out)
}

fn fmt_score(value: Option<f64>) -> String {
    match value {
        Some(v) if v.is_finite() => format!("{v:.0}"),
        _ => "-".to_string(),
    }
}

fn state_class(state: ResultState) -> &'static str {
    match state {
        ResultState::Invalid => "bad",
        ResultState::Partial | ResultState::Custom | ResultState::Local => "warn",
        _ => "ok",
    }
}

/// Renders a complete, printable HTML report.
pub fn render(bundle: &Bundle) -> String {
    let mut html = String::with_capacity(16 * 1024);
    html.push_str(HEAD);

    // --- header -----------------------------------------------------------
    html.push_str(&format!(
        r#"<header class="hdr">
  <div class="brand"><span class="mark">DARC//BENCH</span>
    <span class="sub">Deployment &middot; Application &middot; Runtime &middot; Compute</span></div>
  <div class="runmeta">
    <div><span class="k">Run</span> <code>{run}</code></div>
    <div><span class="k">Profile</span> {profile}</div>
    <div><span class="k">Finished</span> {finished}</div>
  </div>
</header>"#,
        run = escape(bundle.run.run_id.as_str()),
        profile = escape(bundle.run.profile.as_str()),
        finished = escape(&bundle.run.finished_at.to_rfc3339()),
    ));

    // --- calibration banner ------------------------------------------------
    if bundle.scores.uncalibrated {
        html.push_str(
            r#"<div class="banner">
  <strong>Provisional scores.</strong> The DARCBench scoring model
  (<code>dbs/0.1.0-dev</code>) has not yet been calibrated against a physical
  DARC-REF-1 reference machine. The reference values are declared targets, not
  measurements. Raw metrics below are real; the scores derived from them are
  development output and are not comparable with any future calibrated release.
</div>"#,
        );
    }

    // --- score summary -----------------------------------------------------
    html.push_str(&format!(
        r#"<section class="scores">
  <div class="total {tclass}">
    <div class="label">DARCBench Total Score</div>
    <div class="value">{total}</div>
    <div class="note">{tnote}</div>
  </div>
  <div class="grid">"#,
        tclass = state_class(bundle.verdict.state),
        total = fmt_score(bundle.scores.total),
        tnote = if bundle.scores.total_is_standard {
            "standard profile".to_string()
        } else {
            format!(
                "not a standard total &mdash; {} required categor(y|ies) missing",
                bundle.scores.missing_required_categories.len()
            )
        },
    ));

    for category in &bundle.scores.categories {
        html.push_str(&format!(
            r#"<div class="card"><div class="label">{label}</div><div class="value">{score}</div>
               <div class="note">{n} metric(s) &middot; weight {w:.0}%</div></div>"#,
            label = escape(&category.label),
            score = fmt_score(Some(category.score)),
            n = category.metric_count,
            w = category.weight * 100.0,
        ));
    }
    for (key, value) in &bundle.scores.facets {
        html.push_str(&format!(
            r#"<div class="card"><div class="label">{label}</div><div class="value">{score}</div>
               <div class="note">derived facet</div></div>"#,
            label = escape(&key.replace('_', " ")),
            score = fmt_score(Some(*value)),
        ));
    }
    html.push_str(&format!(
        r#"<div class="card"><div class="label">Stability Score</div><div class="value">{stab}</div>
           <div class="note">median CV {cv}</div></div>"#,
        stab = fmt_score(Some(bundle.scores.stability_score)),
        cv = bundle
            .scores
            .median_cv
            .map(|v| format!("{:.1}%", v * 100.0))
            .unwrap_or_else(|| "n/a".into()),
    ));
    if let Some(sustained) = &bundle.scores.sustained {
        html.push_str(&format!(
            r#"<div class="card"><div class="label">Sustained Performance Score</div>
               <div class="value">{score}</div>
               <div class="note">kept {kept:.0}% over {cycles} cycles</div></div>"#,
            score = fmt_score(Some(sustained.score)),
            kept = sustained.retention * 100.0,
            cycles = sustained.cycles,
        ));
    }
    html.push_str("</div></section>");

    // --- sustained performance ----------------------------------------------
    //
    // Placed above the verdict rather than beside the telemetry, because on a
    // cycling run this *is* the headline. A machine that scores 900 and keeps
    // 40% of it is a worse buy than one that scores 700 and keeps all of it,
    // and a reader who has to scroll past the environment table to learn that
    // has been told in the wrong order.
    if let (Some(sustained), Some(diagnosis)) =
        (&bundle.scores.sustained, &bundle.sustained_diagnosis)
    {
        let banner = if sustained.declined() {
            "banner warn-banner"
        } else {
            "banner"
        };
        html.push_str(&format!(
            r#"<section><h2>Sustained performance</h2>
            <div class="{banner}"><strong>{cause}</strong> {explanation}</div>
            <table><tbody>
            <tr><th>Retention</th><td>{retention:.1}% of the opening figure</td></tr>
            <tr><th>Cycles completed</th><td>{cycles}</td></tr>
            <tr><th>Scored from</th><td>cycle {scored} (the last complete one)</td></tr>
            <tr><th>Steal time</th><td>{steal_open:.1}% at the start, {steal_close:.1}% at the end
                (± {steal_sd:.1} points)</td></tr>
            <tr><th>Clock change</th><td>{freq}</td></tr>
            <tr><th>Temperature change</th><td>{temp}</td></tr>
            </tbody></table>"#,
            cause = escape(diagnosis.cause.label()),
            explanation = escape(&diagnosis.explanation),
            retention = sustained.retention * 100.0,
            cycles = sustained.cycles,
            scored = sustained.scored_cycle,
            steal_open = diagnosis.evidence.steal_pct_opening,
            steal_close = diagnosis.evidence.steal_pct_closing,
            steal_sd = diagnosis.evidence.steal_pct_stddev,
            freq = diagnosis
                .evidence
                .frequency_drop
                .map(|d| format!("{:+.1}%", -d * 100.0))
                .unwrap_or_else(|| "not exposed by this host".into()),
            temp = diagnosis
                .evidence
                .temperature_rise_c
                .map(|t| format!("{t:+.0} C"))
                .unwrap_or_else(|| "no sensor".into()),
        ));

        // Which subsystem lost the most. An aggregate that fell 20% says the
        // machine got slower; this says whether it was the disk.
        let mut worst: Vec<(&String, &f64)> = sustained.by_metric.iter().collect();
        worst.sort_by(|a, b| a.1.total_cmp(b.1));
        html.push_str("<h3>Retention by metric</h3><table><tbody>");
        for (key, value) in worst.iter().take(8) {
            html.push_str(&format!(
                "<tr><th>{}</th><td>{:.0}%</td></tr>",
                escape(key),
                *value * 100.0
            ));
        }
        html.push_str("</tbody></table></section>");
    }

    if let Some(reason) = &bundle.run.stopped_because {
        html.push_str(&format!(
            r#"<div class="banner warn-banner"><strong>This run was stopped early.</strong>
            {}</div>"#,
            escape(reason),
        ));
    }

    if !bundle.run.guards_not_enforced.is_empty() {
        // A guard that never fired and a guard that was never armed look
        // identical in a bundle otherwise, and a reader weighing whether to
        // trust a number needs to know which one they are looking at.
        html.push_str(
            r#"<div class="banner warn-banner"><strong>Not every guard could run here.</strong>
            <ul>"#,
        );
        for guard in &bundle.run.guards_not_enforced {
            html.push_str(&format!("<li>{}</li>", escape(guard)));
        }
        html.push_str("</ul></div>");
    }

    if bundle.scores.weak_link_applied {
        html.push_str(&format!(
            r#"<div class="banner warn-banner"><strong>Weak-link cap applied.</strong>
            The aggregate before the cap was {uncapped}; it was reduced to {capped} because one
            subsystem is far slower than the rest of the machine (balance index
            {balance}). See <code>docs/SCORING-SYSTEM.md</code>.</div>"#,
            uncapped = fmt_score(bundle.scores.uncapped_total),
            capped = fmt_score(bundle.scores.total),
            balance = bundle
                .scores
                .balance_index
                .map(|v| format!("{v:.2}"))
                .unwrap_or_else(|| "n/a".into()),
        ));
    }

    // --- verdict -----------------------------------------------------------
    html.push_str(&format!(
        r#"<section><h2>Result state</h2><p class="state {cls}">{state:?}</p><ul class="reasons">"#,
        cls = state_class(bundle.verdict.state),
        state = bundle.verdict.state,
    ));
    if bundle.verdict.reasons.is_empty() {
        html.push_str("<li>No findings.</li>");
    }
    for reason in &bundle.verdict.reasons {
        html.push_str(&format!("<li>{}</li>", escape(&format!("{reason:?}"))));
    }
    html.push_str(&format!(
        "</ul><p class=\"note\">Validator {}</p></section>",
        escape(&bundle.verdict.validator_version)
    ));

    // --- environment --------------------------------------------------------
    let env = &bundle.environment;
    html.push_str("<section><h2>Environment</h2><table><tbody>");
    let rows: Vec<(&str, String)> = vec![
        ("Scope", format!("{:?}", env.platform.scope)),
        (
            "Virtualization",
            env.platform
                .virtualization
                .clone()
                .unwrap_or_else(|| "none detected".into()),
        ),
        (
            "CPU",
            env.cpu.model.clone().unwrap_or_else(|| "unknown".into()),
        ),
        (
            "Topology",
            format!(
                "{} logical / {} physical core(s) / {} socket(s)",
                env.cpu.logical_cpus,
                env.cpu
                    .physical_cores
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "?".into()),
                env.cpu
                    .sockets
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "?".into()),
            ),
        ),
        (
            "Governor",
            env.cpu.governor.clone().unwrap_or_else(|| "unknown".into()),
        ),
        (
            "Memory",
            format!(
                "{:.1} GiB",
                env.memory.total_bytes as f64 / 1024.0_f64.powi(3)
            ),
        ),
        (
            "Kernel",
            env.platform
                .kernel_release
                .clone()
                .unwrap_or_else(|| "unknown".into()),
        ),
        (
            "Distribution",
            format!(
                "{} {}",
                env.platform.distribution.clone().unwrap_or_default(),
                env.platform
                    .distribution_version
                    .clone()
                    .unwrap_or_default()
            ),
        ),
        (
            "cgroup CPU limit",
            env.platform
                .cgroup_cpu_limit
                .map(|v| format!("{v:.2} CPU"))
                .unwrap_or_else(|| "none".into()),
        ),
        (
            "Agent",
            format!(
                "{} ({})",
                bundle.meta.agent_version, bundle.meta.build_profile
            ),
        ),
        ("Build target", bundle.meta.build_target.clone()),
    ];
    for (key, value) in rows {
        html.push_str(&format!(
            "<tr><th>{}</th><td>{}</td></tr>",
            escape(key),
            escape(value.trim())
        ));
    }
    html.push_str("</tbody></table>");
    if !env.gaps.is_empty() {
        html.push_str("<p class=\"note\">Undetermined: ");
        html.push_str(
            &env.gaps
                .iter()
                .map(|g| escape(&g.field))
                .collect::<Vec<_>>()
                .join(", "),
        );
        html.push_str("</p>");
    }
    html.push_str("</section>");

    // --- raw metrics ---------------------------------------------------------
    html.push_str("<section><h2>Raw measurements</h2>");
    for module in &bundle.modules {
        html.push_str(&format!(
            r#"<h3>{id} <span class="ver">v{ver}</span> <span class="badge">{status:?}</span></h3>
            <table class="metrics"><thead><tr>
            <th>Metric</th><th>Median</th><th>Unit</th><th>n</th><th>CV</th>
            <th>Min</th><th>Max</th><th>95% CI</th></tr></thead><tbody>"#,
            id = escape(module.module.id.as_str()),
            ver = escape(&module.module.version),
            status = module.status,
        ));
        for metric in &module.metrics {
            html.push_str(&format!(
                "<tr><td>{key}</td><td class=\"num\">{value:.2}</td><td>{unit}</td>\
                 <td class=\"num\">{n}</td><td class=\"num\">{cv}</td>\
                 <td class=\"num\">{min:.2}</td><td class=\"num\">{max:.2}</td>\
                 <td class=\"num\">{ci}</td></tr>",
                key = escape(&metric.key),
                value = metric.value,
                unit = escape(&metric.unit),
                n = metric.summary.n,
                cv = metric
                    .summary
                    .cv
                    .map(|v| format!("{:.1}%", v * 100.0))
                    .unwrap_or_else(|| "-".into()),
                min = metric.summary.min,
                max = metric.summary.max,
                ci = metric
                    .summary
                    .ci95
                    .map(|(lo, hi)| format!("{lo:.1} - {hi:.1}"))
                    .unwrap_or_else(|| "-".into()),
            ));
        }
        html.push_str("</tbody></table>");
        if !module.warnings.is_empty() {
            html.push_str("<ul class=\"warnings\">");
            for warning in &module.warnings {
                html.push_str(&format!(
                    "<li><code>{:?}</code> {}</li>",
                    warning.code,
                    escape(&warning.message)
                ));
            }
            html.push_str("</ul>");
        }
    }
    if bundle.modules.is_empty() {
        html.push_str("<p class=\"note\">No module produced measurements.</p>");
    }
    html.push_str("</section>");

    // --- telemetry ------------------------------------------------------------
    let t = &bundle.telemetry;
    html.push_str(&format!(
        r#"<section><h2>Telemetry during the run</h2><table><tbody>
        <tr><th>Samples</th><td>{samples}</td></tr>
        <tr><th>Mean CPU busy</th><td>{busy:.1}%</td></tr>
        <tr><th>Peak CPU used by other work</th><td>{external:.1}% of the machine, excluding this benchmark</td></tr>
        <tr><th>Mean / peak CPU steal</th><td>{steal_mean:.2}% / {steal_max:.2}%</td></tr>
        <tr><th>Mean I/O wait</th><td>{iowait:.2}%</td></tr>
        <tr><th>Peak load1</th><td>{load:.2}</td></tr>
        <tr><th>Peak swap used</th><td>{swap} MiB</td></tr>
        <tr><th>CPU frequency drift</th><td>{drift}</td></tr>
        <tr><th>Peak CPU temperature</th><td>{temp}</td></tr>
        </tbody></table></section>"#,
        samples = t.samples,
        busy = t.cpu_busy_pct_mean,
        // Spelled out in the row rather than left as a bare percentage: the
        // number only means anything once the reader knows it excludes the
        // benchmark itself.
        external = t.cpu_external_busy_pct_max,
        steal_mean = t.cpu_steal_pct_mean,
        steal_max = t.cpu_steal_pct_max,
        iowait = t.cpu_iowait_pct_mean,
        load = t.load1_max,
        swap = t.swap_used_bytes_max / (1024 * 1024),
        drift = t
            .frequency_drop()
            .map(|d| format!("{:.1}% lower at the end of the run", d * 100.0))
            .unwrap_or_else(|| "not observable".into()),
        temp = t
            .cpu_temp_c_max
            .map(|v| format!("{v:.1} &deg;C"))
            .unwrap_or_else(|| "n/a".into()),
    ));

    // --- provenance ------------------------------------------------------------
    html.push_str(&format!(
        r#"<section><h2>Provenance</h2><table><tbody>
        <tr><th>Bundle schema</th><td>{schema}</td></tr>
        <tr><th>Event protocol</th><td>{proto}</td></tr>
        <tr><th>Scoring model</th><td>{model} (reference {reference})</td></tr>
        <tr><th>Environment digest</th><td><code>{envd}</code></td></tr>
        <tr><th>Event stream digest</th><td><code>{evd}</code> over {ev} event(s)</td></tr>
        <tr><th>Signature</th><td>{sig}</td></tr>
        </tbody></table></section>"#,
        schema = escape(&bundle.meta.schema),
        proto = escape(&bundle.meta.protocol),
        model = escape(&bundle.scores.scoring_model),
        reference = escape(&bundle.scores.reference_profile),
        envd = escape(&bundle.run.environment_digest),
        evd = escape(&bundle.run.events_digest),
        ev = bundle.run.event_count,
        sig = match &bundle.signature {
            Some(s) => format!(
                "{} &middot; key <code>{}</code>",
                escape(&s.algorithm),
                escape(&s.key_id)
            ),
            None => "unsigned".to_string(),
        },
    ));

    html.push_str(FOOT);
    html
}

const HEAD: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="referrer" content="no-referrer">
<title>DARCBench report</title>
<style>
:root{--bg:#070a12;--panel:#0e1422;--line:#1d2740;--fg:#e8eef8;--dim:#93a1bd;
--cyan:#22e0ff;--blue:#3b7dff;--violet:#9b6bff;--ok:#3ddc97;--warn:#ffb547;--bad:#ff5c73}
*{box-sizing:border-box}
body{margin:0;padding:2rem 1.25rem 4rem;background:var(--bg);color:var(--fg);
font:15px/1.55 ui-sans-serif,system-ui,-apple-system,"Segoe UI",Roboto,sans-serif;
max-width:1100px;margin-inline:auto}
code{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:.86em;
background:#131c30;padding:.1em .4em;border-radius:4px;color:var(--cyan);word-break:break-all}
.hdr{display:flex;flex-wrap:wrap;gap:1.5rem;justify-content:space-between;
align-items:flex-end;border-bottom:1px solid var(--line);padding-bottom:1rem;margin-bottom:1.5rem}
.mark{font-weight:800;letter-spacing:.06em;font-size:1.5rem;
background:linear-gradient(90deg,var(--cyan),var(--blue) 55%,var(--violet));
-webkit-background-clip:text;background-clip:text;color:transparent}
.sub{display:block;color:var(--dim);font-size:.74rem;letter-spacing:.22em;text-transform:uppercase}
.runmeta{font-size:.82rem;color:var(--dim);text-align:right}
.runmeta .k{color:var(--dim);text-transform:uppercase;letter-spacing:.1em;font-size:.68rem;
margin-right:.4rem}
.banner{border:1px solid var(--warn);background:rgba(255,181,71,.09);color:#ffdca8;
padding:.85rem 1rem;border-radius:10px;margin-bottom:1.5rem;font-size:.9rem}
.warn-banner{margin-top:-.5rem}
.scores{margin-bottom:2rem}
.total{border:1px solid var(--line);border-radius:14px;padding:1.25rem 1.5rem;
background:linear-gradient(160deg,#101a30,#0b1120);margin-bottom:1rem}
.total .value{font-size:3.2rem;font-weight:800;line-height:1;letter-spacing:-.02em}
.total.ok .value{color:var(--ok)}.total.warn .value{color:var(--warn)}.total.bad .value{color:var(--bad)}
.grid{display:grid;gap:.75rem;grid-template-columns:repeat(auto-fill,minmax(190px,1fr))}
.card{border:1px solid var(--line);border-radius:10px;padding:.85rem 1rem;background:var(--panel)}
.card .value{font-size:1.6rem;font-weight:700;color:var(--cyan)}
.label{color:var(--dim);font-size:.7rem;text-transform:uppercase;letter-spacing:.12em}
.note{color:var(--dim);font-size:.78rem;margin-top:.25rem}
h2{font-size:1.05rem;letter-spacing:.04em;text-transform:uppercase;color:var(--dim);
border-bottom:1px solid var(--line);padding-bottom:.4rem;margin:2.2rem 0 .9rem}
h3{font-size:.98rem;margin:1.4rem 0 .5rem}
.ver{color:var(--dim);font-weight:400;font-size:.8rem}
.badge{font-size:.7rem;border:1px solid var(--line);border-radius:999px;padding:.1rem .55rem;
color:var(--dim);vertical-align:middle}
table{width:100%;border-collapse:collapse;font-size:.87rem}
th,td{text-align:left;padding:.45rem .6rem;border-bottom:1px solid var(--line);vertical-align:top}
th{color:var(--dim);font-weight:600;white-space:nowrap}
.metrics thead th{font-size:.7rem;text-transform:uppercase;letter-spacing:.08em}
.num{text-align:right;font-variant-numeric:tabular-nums}
.state{font-weight:700;font-size:1.1rem}
.state.ok{color:var(--ok)}.state.warn{color:var(--warn)}.state.bad{color:var(--bad)}
.reasons,.warnings{color:var(--dim);font-size:.85rem;padding-left:1.1rem}
.warnings li{margin:.2rem 0}
footer{margin-top:3rem;padding-top:1rem;border-top:1px solid var(--line);
color:var(--dim);font-size:.78rem}
@media print{body{background:#fff;color:#000;max-width:none}
.card,.total,table{border-color:#ccc}.mark{color:#000;-webkit-text-fill-color:#000}
.banner{border-color:#000;color:#000;background:#f5f5f5}}
@media (prefers-reduced-motion:reduce){*{animation:none!important;transition:none!important}}
</style></head><body>"#;

const FOOT: &str = r#"<footer>
Generated by DARCBench &mdash; Tombatossals Softworks LLC.
Raw measurements in this report are reproducible from the accompanying
<code>bundle.json</code>; scores are derived and can be recomputed from it.
</footer></body></html>"#;

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;
    use crate::bundle::{BundleMeta, RunRecord, TelemetrySummary};
    use darcbench_inventory::Inventory;
    use darcbench_protocol::{Profile, RunId, RunState, Verdict};

    fn bundle() -> Bundle {
        let now = chrono::Utc::now();
        let inventory = Inventory::collect();
        Bundle {
            meta: BundleMeta::new("0.1.0-test"),
            run: RunRecord {
                run_id: RunId::try_new().expect("id"),
                profile: Profile::Quick,
                state: RunState::Completed,
                started_at: now,
                finished_at: now,
                duration_ms: 1234,
                modules: vec![],
                environment_digest: "sha256:aa".into(),
                events_digest: "sha256:bb".into(),
                event_count: 7,
                stopped_because: None,
                guards_not_enforced: vec![],
                comparability_not_recorded: vec![],
            },
            environment: inventory,
            modules: vec![],
            scores: darcbench_scoring::ScoringModel::current().score_run(Profile::Quick, &[]),
            verdict: Verdict {
                state: ResultState::Partial,
                reasons: vec![],
                validator_version: "dbv/0.1.0".into(),
            },
            telemetry: TelemetrySummary::default(),
            sustained_diagnosis: None,
            signature: None,
        }
    }

    #[test]
    fn escaping_neutralises_html_and_attribute_injection() {
        assert_eq!(
            escape("<script>alert(1)</script>"),
            "&lt;script&gt;alert(1)&lt;/script&gt;"
        );
        assert_eq!(escape(r#"" onload="x"#), "&quot; onload=&quot;x");
        assert_eq!(escape("a & b"), "a &amp; b");
        assert_eq!(escape("it's"), "it&#x27;s");
    }

    #[test]
    fn report_is_self_contained() {
        let html = render(&bundle());
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.ends_with("</html>"));
        for forbidden in ["http://", "https://", "//cdn", "<script"] {
            assert!(
                !html.contains(forbidden),
                "report must not reference `{forbidden}`; reports are opened offline and must \
                 not phone home"
            );
        }
    }

    #[test]
    fn uncalibrated_status_is_stated_prominently() {
        let html = render(&bundle());
        assert!(html.contains("Provisional scores"));
        assert!(html.contains("has not yet been calibrated"));
    }

    #[test]
    fn hostile_inventory_strings_cannot_inject_markup() {
        let mut b = bundle();
        b.environment.cpu.model =
            Some("<img src=x onerror=alert(1)>Evil CPU \" onmouseover=\"y".into());
        b.environment.platform.kernel_release = Some("</table><script>bad()</script>".into());
        let html = render(&b);
        assert!(!html.contains("<img src=x"));
        assert!(!html.contains("<script>bad()"));
        assert!(html.contains("&lt;img src=x"));
    }

    /// On a cycling run this is the headline, and a headline that is only in
    /// the JSON has not been reported.
    ///
    /// A machine scoring 900 and keeping 40% of it is a worse buy than one
    /// scoring 700 and keeping all of it. That comparison is only possible if
    /// the retention, the cause and the evidence are all on the page.
    #[test]
    fn a_cycling_run_reports_what_it_retained_and_why() {
        let mut b = bundle();
        b.scores.sustained = Some(darcbench_scoring::SustainedOutcome {
            retention: 0.42,
            score: 420.0,
            cycles: 12,
            scored_cycle: 11,
            by_metric: [("cpu.mixed/crypto_sha256.single".to_string(), 0.31)]
                .into_iter()
                .collect(),
        });
        b.sustained_diagnosis = Some(crate::diagnosis::SustainedDiagnosis {
            cause: crate::diagnosis::SustainedCause::BurstCreditExhaustion,
            explanation: "Steal rose while the clock held steady.".into(),
            evidence: crate::diagnosis::SustainedEvidence {
                steal_pct_opening: 0.2,
                steal_pct_closing: 61.0,
                steal_pct_stddev: 28.0,
                frequency_drop: Some(0.0),
                temperature_rise_c: None,
                samples: 3600,
            },
        });
        let html = render(&b);

        assert!(html.contains("Sustained Performance Score"));
        assert!(
            html.contains("Burst credits exhausted"),
            "the cause must be named"
        );
        assert!(html.contains("Steal rose while the clock held steady."));
        assert!(html.contains("42.0%"), "the retention itself must be shown");
        // The evidence, so the diagnosis can be argued with rather than trusted.
        assert!(html.contains("61.0%"));
        // And which subsystem lost the most.
        assert!(html.contains("cpu.mixed/crypto_sha256.single"));

        // A single-pass run shows none of it rather than an empty section.
        let plain = render(&bundle());
        assert!(!plain.contains("Sustained performance"));
        assert!(!plain.contains("Sustained Performance Score"));
    }

    /// A watchdog abort and an operator cancelling look identical without this.
    #[test]
    fn a_run_stopped_by_the_watchdog_says_so_on_the_page() {
        let mut b = bundle();
        b.run.stopped_because =
            Some("Stopped by the watchdog: package temperature held at 101 C.".into());
        let html = render(&b);
        assert!(html.contains("stopped early"));
        assert!(html.contains("101 C"));
        assert!(!render(&bundle()).contains("stopped early"));
    }

    #[test]
    fn report_includes_provenance_and_signature_state() {
        let html = render(&bundle());
        assert!(html.contains("Provenance"));
        assert!(html.contains("unsigned"));
        assert!(html.contains("darcbench.bundle/1"));
    }

    #[test]
    fn signed_bundle_shows_the_key_id() {
        let key = crate::signing::AgentKey::generate().expect("keygen");
        let mut b = bundle();
        b.sign(&key).expect("sign");
        let html = render(&b);
        assert!(html.contains(&key.key_id()));
        assert!(!html.contains("unsigned"));
    }
}
