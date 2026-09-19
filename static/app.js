"use strict";

const $ = (s, r = document) => r.querySelector(s);
const $$ = (s, r = document) => Array.from(r.querySelectorAll(s));

const state = { rulesets: [], datasets: [], analyses: [], plans: [], planVersion: 0, planDecisions: {} };

async function api(method, path, body, idempotencyKey) {
  const opts = { method, headers: {} };
  if (body !== undefined) {
    opts.headers["Content-Type"] = "application/json";
    opts.body = JSON.stringify(body);
  }
  if (idempotencyKey) opts.headers["Idempotency-Key"] = idempotencyKey;
  const res = await fetch(path, opts);
  const text = await res.text();
  let data = null;
  try { data = text ? JSON.parse(text) : null; } catch { data = text; }
  if (!res.ok) {
    const msg = data && data.error ? `${data.error.code}: ${data.error.message}\n` +
      (data.error.details && Object.keys(data.error.details).length ? JSON.stringify(data.error.details, null, 2) : "")
      : `HTTP ${res.status}`;
    throw new Error(msg);
  }
  return data;
}

function showError(e) {
  $("#err-text").textContent = String(e.message || e);
  $("#err-dialog").showModal();
}
$("#err-close").onclick = () => $("#err-dialog").close();

function cp(cp) {
  const v = cp.scalar ?? cp.surrogate ?? cp.invalid_byte;
  const kind = cp.scalar !== undefined ? "U+" : cp.surrogate !== undefined ? "S+" : "0x";
  return `${kind}${v.toString(16).toUpperCase().padStart(kind === "0x" ? 2 : 4, "0")}`;
}
function cpChar(u) {
  if (u.kind === "scalar") return String.fromCodePoint(u.scalar);
  if (u.kind === "surrogate") return `\\u${u.surrogate.toString(16)}`;
  return `\\x${u.invalid_byte.toString(16)}`;
}

function renderStages(stages) {
  return stages.map(st => {
    const units = st.units.map((u, i) => {
      const note = st.notes[i] ? ` <span class="issue">${escapeHtml(st.notes[i])}</span>` : "";
      const detail = `<div class="cp-detail">来源(provenance)：上游#${st.provenance[i]}；值 ${cp(u)} 字符「${escapeHtml(cpChar(u))}」</div>`;
      return `<div><span class="cp">${cp(u)}</span>${detail}${note}</div>`;
    }).join("");
    return `<div class="stage"><div class="stage-name">${escapeHtml(st.name)} (${st.units.length})</div>${units}</div>`;
  }).join("");
}

function escapeHtml(s) {
  return String(s).replace(/[&<>"]/g, c => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
}

function issuesHtml(issues) {
  return issues.map(i => {
    const cls = i.kind === "mixed_script" ? "issue mixed" : "issue";
    return `<div class="${cls}">${escapeHtml(JSON.stringify(i))}</div>`;
  }).join("");
}

// ---- tabs ----
$$("nav button").forEach(b => b.onclick = () => {
  $$("nav button").forEach(x => x.classList.remove("active"));
  $$(".tab").forEach(x => x.classList.remove("active"));
  b.classList.add("active");
  $("#tab-" + b.dataset.tab).classList.add("active");
  refresh();
});

// ---- global refresh ----
async function refresh() {
  const s = await api("GET", "/api/state");
  state.rulesets = s.rulesets;
  state.datasets = s.datasets;
  state.analyses = s.analyses;
  state.plans = s.plans;

  $("#tableinfo").textContent = s.rulesets.length
    ? `当前规则集 rev ${s.rulesets[s.rulesets.length - 1].revision}（已批准方案不会因升级重算）`
    : "尚无规则集";

  $("#rulesets").innerHTML = s.rulesets.map(r =>
    `<div class="card">rev ${r.revision} <b>${escapeHtml(r.label)}</b> ` +
    `fold=${r.config.case_fold} ${r.config.normalization} stripDI=${r.config.strip_default_ignorable} ` +
    `script=${r.config.restrict_script || "—"}` +
    `<div class="muted">${escapeHtml(JSON.stringify(r.tables))}</div></div>`).join("");

  $("#datasets").innerHTML = s.datasets.map(d =>
    `<div class="card">#${d.id} <b>${escapeHtml(d.label)}</b> — ${d.count} 条记录</div>`).join("");

  const optsA = s.datasets.map(d => `<option value="${d.id}">#${d.id} ${escapeHtml(d.label)}</option>`).join("");
  const optsR = s.rulesets.map(r => `<option value="${r.id}">#${r.id} rev${r.revision} ${escapeHtml(r.label)}</option>`).join("");
  $("#analysis-dataset").innerHTML = optsA;
  $("#analysis-ruleset").innerHTML = optsR;
  if (s.rulesets.length) $("#analysis-ruleset").value = s.rulesets[s.rulesets.length - 1].id;

  const optsAn = s.analyses.map(a =>
    `<option value="${a.id}">#${a.id} ds${a.dataset_id}/rs${a.ruleset_id}/rev${a.ruleset_revision} ` +
    `(${a.buckets} 桶, ${a.conflict_buckets} 冲突)</option>`).join("");
  $("#analyses").innerHTML = s.analyses.map(a =>
    `<div class="card">分析 #${a.id}：数据集 ${a.dataset_id}，规则集 ${a.ruleset_id} rev ${a.ruleset_revision}，` +
    `${a.buckets} 个桶，<span class="tag bad">${a.conflict_buckets} 个冲突候选</span></div>`).join("");
  $("#view-analysis").innerHTML = optsAn;
  $("#plan-analysis").innerHTML = optsAn;
  $("#search-analysis").innerHTML = optsAn;
  $("#compare-a").innerHTML = optsAn;
  $("#compare-b").innerHTML = optsAn;
  $("#export-plan").innerHTML = s.plans.map(p =>
    `<option value="${p.id}">#${p.id} 分析${p.analysis_id} ${p.status}</option>`).join("");
  $("#view-plan").innerHTML = s.plans.map(p =>
    `<option value="${p.id}">#${p.id} 分析${p.analysis_id} ${p.status}</option>`).join("");
}

// ---- rulesets ----
$("#ruleset-form").onsubmit = async e => {
  e.preventDefault();
  const f = new FormData(e.target);
  try {
    await api("POST", "/api/rulesets", {
      label: f.get("label"),
      case_fold: f.get("case_fold") === "on",
      normalization: f.get("normalization"),
      strip_default_ignorable: f.get("strip_default_ignorable") === "on",
      restrict_script: f.get("restrict_script") || null,
    });
    refresh();
  } catch (err) { showError(err); }
};
$("#upgrade-form").onsubmit = async e => {
  e.preventDefault();
  const note = new FormData(e.target).get("note");
  try { await api("POST", "/api/tables/upgrade", { note }); refresh(); }
  catch (err) { showError(err); }
};

// ---- import (idempotency: same key replays stored response) ----
$("#import-form").onsubmit = async e => {
  e.preventDefault();
  const f = new FormData(e.target);
  const key = "import-" + await digestText(f.get("text"));
  try {
    const r = await api("POST", "/api/datasets",
      { label: f.get("label"), text: f.get("text") }, key);
    alert(`导入完成：数据集 #${r.id}，${r.count} 条（重复请求返回同一结果）`);
    refresh();
  } catch (err) { showError(err); }
};
async function digestText(t) {
  const buf = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(t));
  return Array.from(new Uint8Array(buf)).map(b => b.toString(16).padStart(2, "0")).join("");
}

// ---- analysis view ----
$("#build-analysis").onclick = async () => {
  try {
    const an = await api("POST", "/api/analyses", {
      dataset_id: Number($("#analysis-dataset").value),
      ruleset_id: Number($("#analysis-ruleset").value),
    });
    $("#view-analysis").value = String(an.id);
    await loadAnalysis();
    refresh();
  } catch (err) { showError(err); }
};
$("#apply-filters").onclick = loadAnalysis;

async function loadAnalysis() {
  const id = $("#view-analysis").value;
  if (!id) return;
  const params = new URLSearchParams();
  const stage = $("#stage-filter").value;
  if (stage) params.set("stage", stage);
  if ($("#issue-only").checked) params.set("issue", "1");
  const rec = $("#record-filter").value.trim();
  if (rec) params.set("record", rec);
  const data = await api("GET", `/api/analyses/${id}?${params}`);

  $("#buckets").innerHTML = data.buckets.map(b => {
    const cls = b.record_nos.length > 1 ? (b.identical_originals ? "bucket dup" : "bucket conflict") : "bucket";
    const tag = b.record_nos.length === 1 ? '<span class="tag ok">唯一</span>'
      : b.identical_originals ? '<span class="tag warn">原文重复</span>'
      : '<span class="tag bad">冲突候选</span>';
    const cps = b.canonical_cps.map(c => `U+${c.toString(16).toUpperCase().padStart(4, "0")}`).join(" ");
    return `<div class="card ${cls}">
      <b>桶 ${escapeHtml(b.bucket_hex.slice(0, 12))}…</b> ${tag}
      <div>规范值：${escapeHtml(b.canonical)} <span class="muted">${cps}</span></div>
      <div>记录：${b.record_nos.map(escapeHtml).join(", ")}</div></div>`;
  }).join("");

  $("#records").innerHTML = data.records.map(r => {
    const flags = [];
    if (r.mixed_script) flags.push('<span class="tag warn">混合脚本</span>');
    if (r.restriction_violated) flags.push('<span class="tag bad">违反脚本限制</span>');
    if (r.issues.some(i => i.kind === "invalid_utf8")) flags.push('<span class="tag bad">非法 UTF-8</span>');
    if (r.issues.some(i => i.kind === "lone_surrogate")) flags.push('<span class="tag bad">单独代理</span>');
    if (r.issues.some(i => i.kind === "noncharacter")) flags.push('<span class="tag warn">非字符</span>');
    return `<div class="card">
      <div><b>${escapeHtml(r.record_no)}</b> ${flags.join(" ")}</div>
      <div class="muted">原始字节 hex：${escapeHtml(r.raw_hex)}</div>
      <div>规范值：${escapeHtml(r.canonical)}</div>
      ${issuesHtml(r.issues)}
      <details><summary>展开各阶段精确码点</summary>${renderStages(r.stages)}</details>
    </div>`;
  }).join("");

  $$(".cp").forEach(el => el.onclick = () => el.classList.toggle("open"));
}

// ---- plans ----
$("#create-plan").onclick = async () => {
  try {
    const p = await api("POST", "/api/plans", { analysis_id: Number($("#plan-analysis").value) });
    await refresh();
    $("#view-plan").value = String(p.id);
    await loadPlan();
  } catch (err) { showError(err); }
};
$("#view-plan").onchange = loadPlan;

async function loadPlan() {
  const id = $("#view-plan").value;
  if (!id) return;
  const p = await api("GET", `/api/plans/${id}`);
  state.planVersion = p.version;
  state.planDecisions = p.decisions || {};
  $("#plan-detail").innerHTML = p.buckets
    .filter(b => b.record_nos.length > 1 && !b.identical_originals)
    .map(b => {
      const key = b.bucket_hex;
      const d = p.decisions[key] || { action: "undecided", new_value: "", keep_old_aliases: false };
      const actions = ["undecided", "rename", "keep_alias", "reject"]
        .map(a => `<option value="${a}" ${d.action === a ? "selected" : ""}>${a}</option>`).join("");
      return `<div class="card bucket conflict" data-bucket="${key}">
        <div><b>${escapeHtml(b.canonical)}</b> — ${b.record_nos.map(escapeHtml).join(", ")}</div>
        <div class="row">
          <select class="d-action">${actions}</select>
          <input class="d-new" placeholder="新规范值（rename 时必填）" value="${escapeHtml(d.new_value || "")}">
          <label><input type="checkbox" class="d-alias" ${d.keep_old_aliases ? "checked" : ""}> 保留旧别名</label>
          <button class="secondary d-save">保存该组</button>
        </div></div>`;
    }).join("");
  $$(".d-save").forEach(btn => btn.onclick = async e => {
    const card = e.target.closest(".card");
    const body = {
      base_version: state.planVersion,
      decisions: { [card.dataset.bucket]: {
        action: $(".d-action", card).value,
        new_value: $(".d-new", card).value,
        keep_old_aliases: $(".d-alias", card).checked,
      } },
    };
    try {
      const r = await api("PUT", `/api/plans/${id}/decisions`, body);
      state.planVersion = r.version;
      alert("已保存（版本 " + r.version + "）");
    } catch (err) { showError(err); }
  });
}

$("#simulate-plan").onclick = async () => {
  const id = $("#view-plan").value;
  const extra = $("#extra-records").value.split(",").map(s => s.trim()).filter(Boolean);
  try {
    const r = await api("POST", `/api/plans/${id}/simulate`, { extra_records: extra });
    $("#sim-output").textContent = JSON.stringify(r, null, 2);
  } catch (err) { showError(err); }
};
$("#approve-plan").onclick = async () => {
  const id = $("#view-plan").value;
  const extra = $("#extra-records").value.split(",").map(s => s.trim()).filter(Boolean);
  try {
    const r = await api("POST", `/api/plans/${id}/approve`,
      { base_version: state.planVersion, extra_records: extra, commit: true });
    $("#sim-output").textContent = JSON.stringify(r, null, 2);
    await loadPlan(); refresh();
  } catch (err) { showError(err); }
};

// ---- search / compare / export ----
$("#search-btn").onclick = async () => {
  const id = $("#search-analysis").value;
  const q = encodeURIComponent($("#search-q").value);
  try {
    const r = await api("GET", `/api/search/${id}?q=${q}`);
    $("#search-output").textContent = JSON.stringify(r, null, 2);
  } catch (err) { showError(err); }
};
$("#compare-btn").onclick = async () => {
  try {
    const r = await api("GET", `/api/compare?a=${$("#compare-a").value}&b=${$("#compare-b").value}`);
    $("#compare-output").textContent = JSON.stringify(r, null, 2);
  } catch (err) { showError(err); }
};
$("#export-btn").onclick = async () => {
  try {
    const r = await api("GET", `/api/plans/${$("#export-plan").value}/export`);
    $("#export-output").textContent = JSON.stringify(r, null, 2);
  } catch (err) { showError(err); }
};

refresh();
