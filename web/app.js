const ROW_H = 54;
const COLS = 24;
let dashboards = [];
let currentPath = "overview.json";
let currentDash = null;
let settings = {};
let cars = [];
let dashGen = 0;
let dashAbort = null;
let uiLayout = "classic";
let currentMeta = "vehicle";
let liveTimer = null;
const dashCache = new Map();
const META_IDS = new Set(["vehicle", "battery", "trips", "software"]);

function isAbort(e) {
  return !!(e && (e.name === "AbortError" || e.code === 20));
}

const $ = (id) => document.getElementById(id);

function toLocalInput(d) {
  const pad = (n) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

let currentRange = "30d";
const RANGE_LABEL = {
  "1d": "24h",
  "7d": "7 days",
  "30d": "30 days",
  "90d": "90 days",
  "1y": "1 year",
  all: "All time",
};

function rangeSpanLabel() {
  const months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
  const short = (v) => {
    const m = String(v || "").match(/^(\d{4})-(\d{2})-(\d{2})/);
    if (!m) return "";
    return `${Number(m[3])} ${months[Number(m[2]) - 1]}`;
  };
  const a = short($("from")?.value);
  const b = short($("to")?.value);
  return a && b ? `${a} – ${b}` : "Dates";
}

function paintRange() {
  const btn = $("range-toggle");
  if (!btn) return;
  btn.textContent = RANGE_LABEL[currentRange] || rangeSpanLabel();
  btn.setAttribute("aria-expanded", $("range")?.classList.contains("open") ? "true" : "false");
  document.querySelectorAll(".presets button").forEach((b) => {
    b.classList.toggle("is-on", b.dataset.range === currentRange);
  });
}

function setRangeOpen(open) {
  $("range")?.classList.toggle("open", open);
  paintRange();
}

function applyRange(key) {
  currentRange = key;
  const to = new Date();
  const from = new Date(to);
  if (key === "1d") from.setDate(to.getDate() - 1);
  else if (key === "7d") from.setDate(to.getDate() - 7);
  else if (key === "30d") from.setDate(to.getDate() - 30);
  else if (key === "90d") from.setDate(to.getDate() - 90);
  else if (key === "1y") from.setFullYear(to.getFullYear() - 1);
  else if (key === "all") from.setFullYear(2016, 0, 1);
  $("from").value = toLocalInput(from);
  $("to").value = toLocalInput(to);
  paintRange();
}

function parseGrafanaTimeFrom(rel, toMs) {
  const m = String(rel || "").trim().match(/^(\d+)\s*([smhdwMy])$/);
  if (!m) return null;
  const n = Number(m[1]);
  const u = m[2];
  if (u === "s") return toMs - n * 1000;
  if (u === "m") return toMs - n * 60 * 1000;
  if (u === "h") return toMs - n * 3600 * 1000;
  if (u === "d") return toMs - n * 86400 * 1000;
  if (u === "w") return toMs - n * 7 * 86400 * 1000;
  const d = new Date(toMs);
  if (u === "M") d.setMonth(d.getMonth() - n);
  else if (u === "y") d.setFullYear(d.getFullYear() - n);
  else return null;
  return d.getTime();
}

function autoInterval(fromMs, toMs) {
  const range = Math.max(1, (toMs - fromMs) / 1000);
  const need = Math.max(5, range / 1600);
  const steps = [5, 10, 15, 30, 60, 120, 300, 600, 900, 1800, 3600, 7200, 10800, 21600, 43200, 86400, 259200, 604800];
  const secs = steps.find((s) => s >= need) || Math.ceil(need);
  if (secs % 86400 === 0) return `${secs / 86400}d`;
  if (secs % 3600 === 0) return `${secs / 3600}h`;
  if (secs % 60 === 0) return `${secs / 60}m`;
  return `${secs}s`;
}

function downsampleRows(rows, maxPts) {
  if (!rows || rows.length <= maxPts) return rows;
  const step = Math.ceil(rows.length / maxPts);
  const out = [];
  for (let i = 0; i < rows.length; i += step) out.push(rows[i]);
  const last = rows[rows.length - 1];
  if (out[out.length - 1] !== last) out.push(last);
  return out;
}

function withPanelTime(v, panel) {
  const from = parseGrafanaTimeFrom(panel?.timeFrom, v.to_ms);
  if (from == null) return v;
  return { ...v, from_ms: from };
}

function vars() {
  const from = new Date($("from").value);
  const to = new Date($("to").value);
  const length = settings.unit_of_length || "km";
  return {
    car_id: Number($("car").value || 1),
    from_ms: from.getTime(),
    to_ms: to.getTime(),
    length_unit: length,
    temp_unit: settings.unit_of_temperature || "C",
    preferred_range: settings.preferred_range || "rated",
    pressure_unit: settings.unit_of_pressure || "bar",
    speed_unit: length === "mi" ? "mph" : "kmh",
    interval: autoInterval(from.getTime(), to.getTime()),
    extras: Object.fromEntries(new URLSearchParams(location.search)),
  };
}

async function api(path, opts) {
  let r;
  try {
    r = await fetch(path, { credentials: "same-origin", ...opts });
  } catch (e) {
    if (opts?.signal?.aborted || isAbort(e)) {
      const err = new Error("aborted");
      err.name = "AbortError";
      throw err;
    }
    throw e;
  }
  if (r.status === 401) {
    await tmAuth.route(await tmAuth.status());
    throw new Error("sign in required");
  }
  if (!r.ok) throw new Error(`${path} ${r.status}`);
  return r.json();
}

function flattenPanels(panels, acc = []) {
  for (const p of panels || []) {
    acc.push(p);
    if (p.panels) flattenPanels(p.panels, acc);
  }
  return acc;
}

function renderNav() {
  if (uiLayout === "grouped") {
    const pages = [
      ["vehicle", "Vehicle"],
      ["battery", "Battery"],
      ["trips", "Trips"],
      ["software", "Software"],
    ];
    $("nav-list").innerHTML =
      `<h2>Car</h2>` +
      pages
        .map(
          ([id, title]) =>
            `<a href="#${id}" data-path="${id}" class="${id === currentMeta ? "active" : ""}">${title}</a>`
        )
        .join("");
    return;
  }
  const folders = {};
  for (const d of dashboards) {
    (folders[d.folder] ||= []).push(d);
  }
  $("nav-list").innerHTML = Object.entries(folders)
    .map(
      ([folder, items]) =>
        `<h2>${escapeHtml(folder)}</h2>` +
        items
          .map(
            (d) =>
              `<a href="#${escapeHtml(d.path)}" data-path="${escapeHtml(d.path)}" class="${d.path === currentPath ? "active" : ""}">${escapeHtml(d.title)}</a>`
          )
          .join("")
    )
    .join("");
}

async function loadDashboard(path) {
  const gen = ++dashGen;
  dashAbort?.abort();
  dashAbort = new AbortController();
  const { signal } = dashAbort;
  currentPath = path;
  renderNav();
  clearLive();
  try {
    const dash = await api("/api/dashboards/" + path, { signal });
    if (gen !== dashGen) return;
    currentDash = dash;
    $("title").textContent = dash.title || path;
    const panels = flattenPanels(dash.panels);
    const maxY = panels.reduce((m, p) => {
      const g = p.gridPos || { y: 0, h: 8 };
      return Math.max(m, g.y + g.h);
    }, 8);
    const board = $("board");
    board.classList.remove("grouped");
    board.style.height = maxY * ROW_H + 24 + "px";
    board.innerHTML = "";
    const v = vars();
    for (const panel of panels) {
      if (gen !== dashGen) return;
      const el = document.createElement("div");
      const g = panel.gridPos || { x: 0, y: 0, w: 24, h: 8 };
      el.className =
        "panel" +
        (panel.type === "row" ? " row-header" : "") +
        (g.h <= 3 ? " compact" : "") +
        (panel.type === "stat" || panel.type === "gauge" ? " panel-kpi" : "");
      el.style.left = (g.x / COLS) * 100 + "%";
      el.style.width = (g.w / COLS) * 100 + "%";
      el.style.top = g.y * ROW_H + "px";
      el.style.height = g.h * ROW_H - 6 + "px";
      el.innerHTML = `<h3>${escapeHtml(interpTitle(panel.title || "", v, panel))}</h3><div class="body"></div>`;
      board.appendChild(el);
      fillPanel(el.querySelector(".body"), panel, v, { gen, signal });
    }
  } catch (e) {
    if (isAbort(e) || gen !== dashGen) return;
    $("board").innerHTML = `<div class="err">${escapeHtml(e.message)}</div>`;
  }
}

function stale(ctl) {
  return ctl.gen != null && ctl.gen !== dashGen;
}

function attachIds(q, sql) {
  if (/drive_id|charging_process_id/.test(sql || "")) {
    const params = new URLSearchParams(location.search);
    if (params.get("drive_id")) q.drive_id = Number(params.get("drive_id"));
    if (params.get("charging_process_id")) q.charging_process_id = Number(params.get("charging_process_id"));
  }
  return q;
}

async function queryPanelSql(sql, v, panel, ctl, extra = {}) {
  const q = attachIds({ sql, ...withPanelTime(v, panel), ...extra }, sql);
  const data = await api("/api/query", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(q),
    signal: ctl.signal,
  });
  if (stale(ctl) || (data && data.cancelled)) {
    const err = new Error("aborted");
    err.name = "AbortError";
    throw err;
  }
  return data;
}

const PREVIEW_TRACK_SQL = `SELECT latitude, longitude FROM (
  SELECT p.latitude AS latitude, p.longitude AS longitude, d.start_date AS sort_date, 0 AS seq
  FROM drives d
  JOIN positions p ON p.id = d.start_position_id
  WHERE d.car_id = $car_id AND $__timeFilter(d.start_date)
  UNION ALL
  SELECT p.latitude, p.longitude, COALESCE(d.end_date, d.start_date), 1
  FROM drives d
  JOIN positions p ON p.id = d.end_position_id
  WHERE d.car_id = $car_id AND $__timeFilter(d.start_date)
) ORDER BY sort_date, seq`;

async function fillGeomapTrack(body, panel, v, ctl, targets) {
  try {
    try {
      const preview = await queryPanelSql(PREVIEW_TRACK_SQL, v, panel, ctl, { max_buckets: 400 });
      if (preview && preview.ok !== false) {
        drawGeomap(body, panel, preview.columns, preview.rows, { preview: true });
      }
    } catch (e) {
      if (isAbort(e) || stale(ctl)) return;
    }
    const results = [];
    for (const t of targets) {
      try {
        results.push(await queryPanelSql(t.rawSql, v, panel, ctl, { max_buckets: 8000 }));
      } catch (e) {
        if (isAbort(e) || stale(ctl)) return;
        results.push({ ok: false, error: e.message || String(e) });
      }
    }
    if (stale(ctl)) return;
    const ok = results.filter((r) => r && r.ok !== false && !r.cancelled);
    if (!ok.length) {
      if (!body._tmMap) {
        body.innerHTML = `<div class="err">${escapeHtml(results[0]?.error || "query failed")}</div>`;
      }
      return;
    }
    draw(body, panel, results);
  } catch (e) {
    if (isAbort(e) || stale(ctl)) return;
    if (!body._tmMap) body.innerHTML = `<div class="err">${escapeHtml(e.message)}</div>`;
  }
}

async function fillPanel(body, panel, v, ctl = {}, dash = currentDash) {
  if (panel.type === "row" || panel.type === "text" || panel.type === "dashlist") {
    body.textContent = panel.options?.content || "";
    return;
  }
  const targets = [];
  for (const t of panel.targets || []) {
    let sql = t.rawSql;
    if (!sql && t.panelId != null && dash) {
      const src = flattenPanels(dash.panels).find((p) => p.id === t.panelId);
      sql = src?.targets?.find((x) => x.rawSql)?.rawSql;
    }
    if (sql) targets.push({ rawSql: sql });
  }
  if (!targets.length) {
    body.innerHTML = "";
    return;
  }
  const layerType = (panel.options?.layers || []).map((l) => l.type).find(Boolean) || "route";
  if (panel.type === "geomap" && layerType !== "markers") {
    await fillGeomapTrack(body, panel, v, ctl, targets);
    return;
  }
  try {
    const results = [];
    for (const t of targets) {
      const q = { sql: t.rawSql, ...withPanelTime(v, panel) };
      if (panel.type === "geomap" || /drive_id|charging_process_id/.test(t.rawSql)) {
        const params = new URLSearchParams(location.search);
        if (params.get("drive_id")) q.drive_id = Number(params.get("drive_id"));
        if (params.get("charging_process_id")) q.charging_process_id = Number(params.get("charging_process_id"));
      }
      try {
        const data = await api("/api/query", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify(q),
          signal: ctl.signal,
        });
        if (ctl.gen != null && ctl.gen !== dashGen) return;
        if (data && data.cancelled) return;
        results.push(data);
      } catch (e) {
        if (isAbort(e) || (ctl.gen != null && ctl.gen !== dashGen)) return;
        results.push({ ok: false, error: e.message || String(e) });
      }
    }
    if (ctl.gen != null && ctl.gen !== dashGen) return;
    const ok = results.filter((r) => r && r.ok !== false && !r.cancelled);
    if (!ok.length) {
      body.innerHTML = `<div class="err">${escapeHtml(results[0]?.error || "query failed")}</div>`;
      return;
    }
    draw(body, panel, results);
  } catch (e) {
    if (isAbort(e) || (ctl.gen != null && ctl.gen !== dashGen)) return;
    body.innerHTML = `<div class="err">${escapeHtml(e.message)}</div>`;
  }
}

function interpTitle(s, v, panel) {
  if (!s) return panel?.type === "row" ? "" : "";
  const car = cars.find((c) => Number(c.id) === Number(v.car_id));
  let title = s
    .replaceAll("${car_id}", car?.name || String(v.car_id))
    .replaceAll("${car_id}", car?.name || String(v.car_id))
    .replaceAll("$car_id", car?.name || String(v.car_id))
    .replaceAll("${period}", v.period || "month")
    .replaceAll("$period", v.period || "month")
    .replaceAll("$length_unit", v.length_unit || "km")
    .replaceAll("${length_unit}", v.length_unit || "km")
    .replaceAll("$temp_unit", v.temp_unit || "C")
    .replaceAll("$speed_unit", v.speed_unit || "kmh")
    .replaceAll("$preferred_range", v.preferred_range || "rated")
    .replaceAll("${preferred_range}", v.preferred_range || "rated")
    .replaceAll("$charge_type", "all")
    .replaceAll("${charge_type}", "all");
  if (panel?.timeFrom) title = `${title} · ${panel.timeFrom}`;
  return title;
}

function ciGet(obj, key) {
  if (!obj || key == null) return undefined;
  if (Object.prototype.hasOwnProperty.call(obj, key)) return obj[key];
  const found = Object.keys(obj).find((k) => k.toLowerCase() === String(key).toLowerCase());
  return found != null ? obj[found] : undefined;
}

function hiddenCol(c, panel) {
  const n = String(c || "");
  if (/(_ts|_path)$/i.test(n) || ["car_id", "drive_id", "start_path", "end_path", "path", "__value", "__text", "date_from", "date_to"].includes(n.toLowerCase())) {
    return true;
  }
  const length = settings.unit_of_length || "km";
  const temp = settings.unit_of_temperature || "C";
  if (length === "mi" && /_km$/i.test(n) && !/_kmh$/i.test(n)) return true;
  if (length === "km" && /_mi$/i.test(n)) return true;
  if (temp === "C" && /_f$/i.test(n)) return true;
  if (temp === "F" && /_c$/i.test(n)) return true;
  if (!panel) return false;
  const org = organizeOpts(panel);
  if (ciGet(org.excludeByName, n)) return true;
  if (/^count\(\*\)$/i.test(n) && ciGet(org.excludeByName, "count")) return true;
  const props = fieldOverride(panel, n);
  return Boolean(props["custom.hideFrom.viz"]);
}

function prettyCols(cols, panel) {
  return (cols || []).filter((c) => !hiddenCol(c, panel));
}

function prettyCol(c) {
  if (!c) return "";
  let s = String(c);
  s = s.replace(/^(count|sum|avg|min|max)\((.+)\)/i, (_, _fn, inner) => (inner === "*" ? "" : inner));
  s = s.replace(/^consumption_(?:net_|gross_)?(km|mi)$/i, (_, u) => `Wh/${u}`);
  s = s.replace(/_mih$/i, " mph");
  s = s.replace(/_kmh$/i, " km/h");
  s = s.replace(/_mi$/i, " mi");
  s = s.replace(/_km$/i, " km");
  s = s.replace(/_kwh$/i, " kWh");
  s = s.replace(/_/g, " ");
  if (s.length > 40 || s.includes("(")) return "";
  return s;
}

function escapeHtml(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c])
  );
}

/** Allow only simple CSS color tokens for inline style sinks. */
function safeCssColor(c) {
  const s = String(c ?? "").trim();
  if (!s || s === "transparent") return "";
  if (/^#([0-9a-f]{3}|[0-9a-f]{6}|[0-9a-f]{8})$/i.test(s)) return s;
  if (/^rgba?\(\s*[\d.%\s,]+\s*\)$/i.test(s)) return s;
  if (/^hsla?\(\s*[\d.%\s,/]+\s*\)$/i.test(s)) return s;
  if (/^[a-z]{1,20}$/i.test(s)) return s.toLowerCase();
  if (/^var\(--[a-z0-9-]+\)$/i.test(s)) return s;
  return "";
}

function styleColor(c) {
  const color = safeCssColor(c);
  return color ? `color:${color}` : "";
}

function styleBg(c) {
  const color = safeCssColor(c);
  return color ? `background:${color}` : "";
}

function organizeOpts(panel) {
  const t = (panel.transformations || []).find((x) => x.id === "organize");
  return t?.options || {};
}

function fieldOverride(panel, col) {
  const org = organizeOpts(panel);
  const aliases = new Set([col]);
  const renamed = ciGet(org.renameByName, col);
  if (renamed) aliases.add(renamed);
  for (const [src, dst] of Object.entries(org.renameByName || {})) {
    if (dst === col || String(dst).toLowerCase() === String(col).toLowerCase()) aliases.add(src);
  }
  const lower = [...aliases].map((a) => String(a).toLowerCase());
  const props = {};
  for (const o of panel.fieldConfig?.overrides || []) {
    const m = o.matcher || {};
    let hit = false;
    if (m.id === "byName" && m.options != null) hit = lower.includes(String(m.options).toLowerCase());
    if (m.id === "byRegexp" && m.options) {
      const raw = String(m.options).replace(/^\/|\/[a-z]*$/gi, "");
      try {
        const re = new RegExp(raw, "i");
        hit = [...aliases].some((a) => re.test(a));
      } catch {
        hit = false;
      }
    }
    if (!hit) continue;
    for (const p of o.properties || []) props[p.id] = p.value;
  }
  return props;
}

function namedColor(c) {
  if (!c || c === "transparent") return c || null;
  const n = String(c).toLowerCase();
  return (
    {
      green: "#56A64B",
      red: "#E02F44",
      orange: "#FFB357",
      yellow: "#F2CC0C",
      blue: "#3274D9",
      "dark-blue": "#3274D9",
      "dark-red": "#C4162A",
      "light-orange": "#FF9830",
      "light-green": "#73BF69",
      "semi-dark-green": "#37872D",
      "super-light-green": "#C8F2C2",
    }[n] || c
  );
}

function thresholdColor(steps, n) {
  if (!Number.isFinite(n) || !steps?.length) return null;
  const sorted = [...steps].sort((a, b) => (a.value ?? -Infinity) - (b.value ?? -Infinity));
  let color = null;
  for (const s of sorted) {
    if (s.value == null || n >= Number(s.value)) color = s.color;
  }
  return namedColor(color);
}

function overrideMap(props, val) {
  const maps = props.mappings || [];
  const keys = [val == null ? "null" : String(val)];
  if (val === 0 || val === "0" || val === false) keys.push("false");
  if (val === 1 || val === "1" || val === true) keys.push("true");
  for (const m of maps) {
    if (m.type !== "value" || !m.options) continue;
    for (const key of keys) {
      const hit = m.options[key];
      if (hit) return { text: hit.text ?? key, color: namedColor(hit.color) };
    }
  }
  return null;
}

function grafanaUnit(unit, col) {
  if (!unit || unit === "none" || unit === "short" || unit === "locale") return "";
  const map = {
    percentunit: "%",
    percent: "%",
    kwatth: "kWh",
    kwatt: "kW",
    watt: "W",
    volt: "V",
    amp: "A",
    celsius: "°C",
    fahrenheit: "°F",
    dateTimeAsLocal: "",
    dtdurations: "",
    s: "",
    clocks: "",
    bytes: "B",
    lengthm: "m",
    lengthkm: "km",
    lengthmi: "mi",
    lengthft: "ft",
    velocitykmh: "km/h",
    velocitymph: "mph",
    pressurebar: "bar",
    pressurepsi: "psi",
    m: "min",
    h: "h",
    d: "d",
  };
  if (unit === "m" && /^(alt|elev|height)/i.test(col || "")) return "m";
  if (Object.prototype.hasOwnProperty.call(map, unit)) return map[unit];
  const stripped = String(unit).replace(/^(velocity|length|pressure)/, "");
  if (stripped && stripped !== unit) {
    if (stripped === "kmh") return "km/h";
    if (stripped === "mph") return "mph";
    return stripped;
  }
  if (/^[A-Za-z°%µμ]+(\/[A-Za-z]+)?$/.test(unit)) return unit;
  return "";
}

function withUnit(text, unit, col) {
  const u = grafanaUnit(unit, col);
  if (!u) return text;
  if (u === "%") return String(text).includes("%") ? text : `${text}%`;
  if (String(text).includes(u)) return text;
  return `${text} ${u}`;
}

function formatCell(panel, col, val) {
  const props = fieldOverride(panel, col);
  const mapped = overrideMap(props, val);
  if (mapped) return mapped;
  if (val == null || val === "") return { text: "", color: null };
  const unit = props.unit || panel.fieldConfig?.defaults?.unit || "";
  if (
    unit === "dateTimeAsLocal" ||
    /^(date|time|date_from|date_to)$/i.test(col) ||
    looksLikeEpoch(col, val)
  ) {
    return { text: formatTime(val), color: null };
  }
  const n = typeof val === "number" ? val : Number(val);
  let color = null;
  if (props.thresholds?.steps) color = thresholdColor(props.thresholds.steps, n);
  if (!Number.isFinite(n)) return { text: String(val), color };
  if (isPercentUnit(unit, col, n)) {
    const pct = n <= 1.5 ? n * 100 : n;
    return { text: `${pct.toFixed(props.decimals ?? 1)}%`, color };
  }
  if (unit === "percent") return { text: `${n.toFixed(props.decimals ?? 1)}%`, color };
  if (unit === "s" || unit === "dtdurations") return { text: formatDuration(n), color };
  const dec = props.decimals;
  const text = dec != null ? n.toFixed(dec) : formatNum(n);
  return { text: withUnit(text, unit, col), color };
}

function looksLikeEpoch(col, val) {
  if (!/date|time|period/i.test(col)) return false;
  const n = Number(val);
  if (!Number.isFinite(n)) return false;
  return (n > 1e12 && n < 2e13) || (n > 1e9 && n < 2e10);
}

function isPercentUnit(unit, col, n) {
  if (unit === "percentunit") return true;
  if (!/efficiency|overhead_pct|standby/i.test(col)) return false;
  return Number.isFinite(n) && n >= 0 && n <= 1.5;
}

function formatDuration(secs) {
  const s = Math.abs(secs);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  if (h >= 24) return `${Math.floor(h / 24)}d ${h % 24}h`;
  if (h >= 1) return `${h}h ${String(m).padStart(2, "0")}m`;
  return `${m}m`;
}

function headerLabel(panel, col) {
  const renamed = ciGet(organizeOpts(panel).renameByName, col);
  const props = fieldOverride(panel, col);
  let name = renamed && String(renamed).trim() ? renamed : props.displayName && String(props.displayName).trim() ? props.displayName : prettyCol(col) || col;
  const u = grafanaUnit(props.unit || "", col);
  if (u && u !== "%" && name && !name.includes(u)) name = `${name} (${u})`;
  return name;
}

function percentile(arr, p) {
  const a = arr.filter((x) => Number.isFinite(x)).sort((x, y) => x - y);
  if (!a.length) return null;
  return a[Math.min(a.length - 1, Math.floor((a.length - 1) * p))];
}

function vampireStats(rows) {
  const loss = rows.map((r) => Number(r.range_lost_per_hour_km ?? r.range_lost_per_hour_mi));
  const power = rows.map((r) => Number(r.avg_power));
  return {
    lossP75: percentile(loss, 0.75),
    lossP90: percentile(loss, 0.9),
    powerP75: percentile(power, 0.75),
  };
}

function vampireRowClass(row, stats) {
  const standby = Number(row.standby);
  const loss = Number(row.range_lost_per_hour_km ?? row.range_lost_per_hour_mi);
  const power = Number(row.avg_power);
  let score = 0;
  const reasons = [];
  if (Number.isFinite(standby) && standby < 0.3) {
    score += 1;
    reasons.push("low standby");
  }
  if (Number.isFinite(standby) && standby < 0.15) score += 1;
  if (Number.isFinite(loss) && stats.lossP75 != null && loss >= stats.lossP75 && loss > 0) {
    score += 1;
    reasons.push("high range loss / h");
  }
  if (Number.isFinite(loss) && stats.lossP90 != null && loss >= stats.lossP90 && loss > 0) score += 1;
  if (Number.isFinite(power) && (power >= 80 || (stats.powerP75 != null && power >= stats.powerP75 && power >= 40))) {
    score += 1;
    reasons.push("high average power");
  }
  if (Number(row.has_reduced_range) === 1) reasons.push("reduced range");
  if (score >= 2) return { cls: "row-bad", title: "Poor vampire drain: " + reasons.join(", ") };
  if (score >= 1) return { cls: "row-warn", title: "Elevated vampire drain: " + reasons.join(", ") };
  return { cls: "", title: "" };
}

function joinKey(cols) {
  return ["date", "display", "date_from", "time", "metric"].find((k) => (cols || []).includes(k));
}

function mergeFrames(results) {
  const frames = (results || []).filter((r) => r && r.ok !== false);
  if (!frames.length) return { columns: [], rows: [] };
  const normalized = frames.map((f) => ({ columns: f.columns || [], rows: f.rows || [] }));
  if (normalized.length === 1) return normalized[0];
  const columns = [];
  const seen = new Set();
  for (const f of normalized) {
    for (const c of f.columns) {
      if (!seen.has(c)) {
        seen.add(c);
        columns.push(c);
      }
    }
  }
  const maps = normalized.map((f) => {
    const k = joinKey(f.columns);
    const by = new Map();
    for (const row of f.rows) {
      const key = k != null ? String(row[k] ?? "") : JSON.stringify(row);
      by.set(key, { ...(by.get(key) || {}), ...row });
    }
    return by;
  });
  const keys = new Set();
  for (const m of maps) for (const k of m.keys()) keys.add(k);
  const rows = [...keys].map((k) => {
    const row = {};
    for (const m of maps) Object.assign(row, m.get(k) || {});
    return row;
  });
  return { columns, rows };
}

function fieldOperand(side, row) {
  if (!side) return null;
  if (side.fixed != null && side.fixed !== "") {
    const n = Number(side.fixed);
    return Number.isFinite(n) ? n : null;
  }
  const name = side.matcher?.options;
  if (!name) return null;
  const raw = Object.prototype.hasOwnProperty.call(row, name)
    ? row[name]
    : row[Object.keys(row).find((k) => k.toLowerCase() === String(name).toLowerCase())];
  if (raw == null || raw === "") return null;
  const n = Number(raw);
  return Number.isFinite(n) ? n : null;
}

function applyCalculateFields(panel, columns, rows) {
  const calcs = (panel.transformations || []).filter((t) => t.id === "calculateField");
  if (!calcs.length) return { columns, rows };
  let cols = columns.slice();
  let out = rows.map((r) => ({ ...r }));
  for (const t of calcs) {
    const alias = interpTitle(t.options?.alias || "", vars(), panel);
    const bin = t.options?.binary;
    if (!alias || !bin) continue;
    if (!cols.includes(alias)) cols.push(alias);
    const op = bin.operator || "+";
    for (const row of out) {
      const a = fieldOperand(bin.left, row);
      const b = fieldOperand(bin.right, row);
      let v = null;
      if (a != null && b != null) {
        if (op === "/") v = b === 0 ? null : a / b;
        else if (op === "*") v = a * b;
        else if (op === "-") v = a - b;
        else if (op === "+") v = a + b;
      }
      row[alias] = v;
    }
    if (t.options?.replaceFields) {
      cols = [alias];
      out = out.map((r) => ({ [alias]: r[alias] }));
    }
  }
  return { columns: cols, rows: out };
}

function applyFilterFields(panel, columns, rows) {
  const t = (panel.transformations || []).find((x) => x.id === "filterFieldsByName");
  if (!t) return { columns, rows };
  const inc = t.options?.include || {};
  const names = (inc.names || []).map((n) => String(n).toLowerCase());
  let re = null;
  if (inc.pattern) {
    try {
      re = new RegExp(inc.pattern);
    } catch {
      re = null;
    }
  }
  if (!names.length && !re) return { columns, rows };
  const cols = columns.filter((c) => {
    const n = String(c);
    if (names.includes(n.toLowerCase())) return true;
    if (re && re.test(n)) return true;
    return false;
  });
  if (!cols.length) return { columns, rows };
  return { columns: cols, rows };
}

function applyJoinByField(panel, results) {
  const t = (panel.transformations || []).find((x) => x.id === "joinByField");
  const by = t?.options?.byField;
  if (!by) return mergeFrames(results);
  const frames = (results || []).filter((r) => r && r.ok !== false);
  if (frames.length <= 1) return mergeFrames(results);
  const maps = frames.map((f) => {
    const col = (f.columns || []).find((c) => String(c).toLowerCase() === String(by).toLowerCase()) || by;
    const m = new Map();
    for (const row of f.rows || []) {
      const key = String(row[col] ?? "");
      m.set(key, { ...(m.get(key) || {}), ...row });
    }
    return m;
  });
  let keys = [...maps[0].keys()];
  if (t.options?.mode === "inner") {
    for (const m of maps.slice(1)) keys = keys.filter((k) => m.has(k));
  } else {
    const all = new Set(keys);
    for (const m of maps.slice(1)) for (const k of m.keys()) all.add(k);
    keys = [...all];
  }
  const columns = [];
  const seen = new Set();
  for (const f of frames) {
    for (const c of f.columns || []) {
      if (!seen.has(c)) {
        seen.add(c);
        columns.push(c);
      }
    }
  }
  const rows = keys.map((k) => {
    const row = {};
    for (const m of maps) Object.assign(row, m.get(k) || {});
    return row;
  });
  return { columns, rows };
}

function transformFrames(panel, results) {
  let { columns, rows } = applyJoinByField(panel, results);
  ({ columns, rows } = applyCalculateFields(panel, columns, rows));
  ({ columns, rows } = applyFilterFields(panel, columns, rows));
  columns = orderColumns(panel, columns);
  return { columns, rows };
}

function reduceField(rows, col, calc) {
  const vals = [];
  for (const r of rows || []) {
    const n = num(r[col]);
    if (n != null) vals.push(n);
  }
  if (!vals.length) return (rows || [])[0]?.[col];
  const name = String(calc || "lastNotNull").toLowerCase();
  if (name === "sum") return vals.reduce((a, b) => a + b, 0);
  if (name === "mean" || name === "avg") return vals.reduce((a, b) => a + b, 0) / vals.length;
  if (name === "max") return Math.max(...vals);
  if (name === "min") return Math.min(...vals);
  if (name === "first" || name === "firstnotnull") return vals[0];
  return vals[vals.length - 1];
}

function orderColumns(panel, columns) {
  const idx = organizeOpts(panel).indexByName || {};
  return columns.slice().sort((a, b) => {
    const ia = ciGet(idx, a);
    const ib = ciGet(idx, b);
    if (ia == null && ib == null) return 0;
    if (ia == null) return 1;
    if (ib == null) return -1;
    return ia - ib;
  });
}

function dateCol(cols) {
  const names = cols || [];
  const preferred = ["date_from", "start_date", "date", "time", "end_date", "start_date_ts"];
  for (const p of preferred) if (names.includes(p)) return p;
  return names.find((c) => /^(date|time)/i.test(c) || /(_date|_at|_ts)$/i.test(c));
}

function toSortTime(v) {
  if (v == null || v === "") return 0;
  if (typeof v === "number") return v > 1e11 ? v : v > 1e9 ? v * 1000 : v;
  const s = String(v);
  const t = Date.parse(s.includes("T") || s.includes(" ") ? s.replace(" ", "T") + ( /Z|[+-]\d\d/.test(s) ? "" : "Z") : s);
  return Number.isFinite(t) ? t : 0;
}

function sortRowsByDate(cols, rows) {
  const col = dateCol(cols);
  if (!col) return rows;
  return rows.slice().sort((a, b) => toSortTime(b[col]) - toSortTime(a[col]));
}

function emptyCol(rows, col) {
  return !(rows || []).some((r) => r[col] != null && r[col] !== "");
}

function storyRowId(kind, row) {
  const keys = kind === "drive" ? ["drive_id", "Drive ID"] : ["id", "charging_process_id", "Charging Process ID"];
  for (const key of keys) {
    if (row[key] != null && row[key] !== "") return row[key];
  }
  const names = Object.keys(row);
  for (const key of keys) {
    const found = names.find((k) => k.toLowerCase() === key.toLowerCase());
    if (found && row[found] != null && row[found] !== "") return row[found];
  }
  return null;
}

function drawTable(body, panel, cols, rows) {
  const slice = sortRowsByDate(cols, rows).slice(0, 500);
  const show = cols.filter((c) => !hiddenCol(c, panel) && !emptyCol(slice, c));
  const headers = show.length ? show : cols.filter((c) => !hiddenCol(c, panel));
  const isVampire = headers.some((c) => /range_lost_per_hour|standby/.test(c));
  const stats = isVampire ? vampireStats(slice) : null;
  const open = body.closest(".panel")?.dataset.open || "";
  body.innerHTML =
    `<table><thead><tr>${headers.map((c) => `<th>${escapeHtml(headerLabel(panel, c))}</th>`).join("")}</tr></thead><tbody>` +
    slice
      .map((r) => {
        const sev = isVampire ? vampireRowClass(r, stats) : { cls: "", title: "" };
        const id = open ? storyRowId(open, r) : null;
        const cls = [id != null ? "row-open" : "", sev.cls].filter(Boolean).join(" ");
        const title = sev.title || (id != null ? (open === "drive" ? "Open this drive" : "Open this charge") : "");
        const attrs =
          (cls ? ` class="${cls}"` : "") +
          (title ? ` title="${escapeHtml(title)}"` : "") +
          (id != null ? ` data-open-id="${escapeHtml(String(id))}" tabindex="0" role="button"` : "");
        return `<tr${attrs}>${headers
          .map((c) => {
            const cell = formatCell(panel, c, r[c]);
            const color = styleColor(cell.color);
            const st = color ? ` style="${color};font-weight:600"` : "";
            return `<td${st}>${escapeHtml(cell.text)}</td>`;
          })
          .join("")}</tr>`;
      })
      .join("") +
    "</tbody></table>";
}

function draw(body, panel, results) {
  const type = panel.type;
  let { columns: cols, rows } = transformFrames(panel, results);
  if (type === "table") rows = sortRowsByDate(cols, rows);
  if (type === "bargauge") {
    drawBarGauge(body, panel, cols, rows);
    return;
  }
  if (type === "stat" || type === "gauge") {
    drawStat(body, panel, cols, rows, type);
    return;
  }
  if (type === "table") {
    drawTable(body, panel, cols, rows);
    return;
  }
  if (type === "geomap") {
    drawGeomap(body, panel, cols, rows);
    return;
  }
  if (type === "piechart") {
    const pieCols = [];
    const pieSeen = new Set();
    const pieRows = [];
    for (const f of results || []) {
      if (!f || f.ok === false) continue;
      for (const c of f.columns || []) {
        if (!pieSeen.has(c)) {
          pieSeen.add(c);
          pieCols.push(c);
        }
      }
      pieRows.push(...(f.rows || []));
    }
    drawPieChart(body, panel, pieCols.length ? pieCols : cols, pieRows.length ? pieRows : rows);
    return;
  }
  if (type === "state-timeline") {
    drawStateTimeline(body, panel, cols, rows);
    return;
  }
  if (type === "barchart") {
    drawBarChart(body, panel, cols, rows);
    return;
  }
  if (type === "heatmap") {
    drawHeatmap(body, panel, cols, rows);
    return;
  }
  if (type === "timeseries" || type === "xychart") {
    drawChart(body, panel, cols, rows, results);
    return;
  }
  body.innerHTML = `<div class="err">unsupported panel ${escapeHtml(type)}</div>`;
}

const STATE_FALLBACK = {
  0: { text: "online", color: "#6ED0E0" },
  1: { text: "driving", color: "#8F3BB8" },
  2: { text: "charging", color: "#F2CC0C" },
  3: { text: "offline", color: "#FFB357" },
  4: { text: "asleep", color: "#56A64B" },
  5: { text: "online", color: "#6ED0E0" },
  6: { text: "updating", color: "#E02F44" },
  online: { text: "online", color: "#6ED0E0" },
  driving: { text: "driving", color: "#8F3BB8" },
  charging: { text: "charging", color: "#F2CC0C" },
  offline: { text: "offline", color: "#FFB357" },
  asleep: { text: "asleep", color: "#56A64B" },
  updating: { text: "updating", color: "#E02F44" },
};

function mappingTable(panel) {
  const out = {};
  const maps = panel.fieldConfig?.defaults?.mappings || [];
  for (const m of maps) {
    if (m.type !== "value" || !m.options) continue;
    for (const [k, v] of Object.entries(m.options)) {
      if (k === "null" || !v) continue;
      out[k] = { text: v.text || k, color: v.color || STATE_FALLBACK[k]?.color || "#8b93a7" };
    }
  }
  return out;
}

function mapValue(panel, val) {
  const table = mappingTable(panel);
  const key = val == null ? "null" : String(val);
  const hit =
    table[key] ||
    table[String(key).toLowerCase()] ||
    (panel?.type === "state-timeline" ? STATE_FALLBACK[key] || STATE_FALLBACK[String(key).toLowerCase()] : null);
  if (hit) return { text: hit.text, color: hit.color, mapped: true };
  return { text: val == null ? "–" : String(val), color: null, mapped: false };
}

function fieldDisplayName(panel, col) {
  const props = fieldOverride(panel, col);
  const dn = props.displayName || ciGet(organizeOpts(panel).renameByName, col);
  if (dn && String(dn).trim()) return String(dn).replace(/:\s*$/, "");
  return "";
}

function setPanelHeading(body, text) {
  const h3 = body?.previousElementSibling;
  if (!h3 || h3.tagName !== "H3" || String(h3.textContent || "").trim()) return;
  h3.textContent = text;
}

function statFieldName(panel, col, rows, metricCol) {
  let name = fieldDisplayName(panel, col) || String(panel.fieldConfig?.defaults?.displayName || "");
  if (name.includes("${__cell_0}") || name.includes("$__cell_0")) {
    const r0 = rows[0] || {};
    name = String(r0[metricCol] ?? r0[col] ?? "");
  }
  if (!name && metricCol && rows[0]?.[metricCol] != null) name = String(rows[0][metricCol]);
  if (!name) name = prettyCol(col) || col || "";
  return interpTitle(String(name).replace(/:\s*$/, ""), vars(), panel);
}

function drawStat(body, panel, cols, rows, type) {
  const calc = (panel.options?.reduceOptions?.calcs || panel.options?.fieldOptions?.calcs || ["lastNotNull"])[0];
  const textMode = panel.options?.textMode || "auto";
  const defaultsUnit = panel.fieldConfig?.defaults?.unit || "";
  const wantTime =
    defaultsUnit === "dateTimeAsLocal" || /time/i.test(panel.options?.reduceOptions?.fields || "");
  const show = prettyCols(cols, panel);
  const metricCol = show.find((c) => /^(metric|name)$/i.test(c));
  let fields = show.filter((c) => c !== metricCol && c !== "time");
  const numeric = fields.filter((c) => rows.some((r) => num(r[c]) != null));
  if (numeric.length) fields = numeric;
  if (wantTime) {
    const tcol = cols.find((c) => c === "time" || c === "date") || fields[0];
    fields = tcol ? [tcol] : fields;
  }
  if (!fields.length) fields = show.filter((c) => c !== "time");
  const items = fields.map((col) => {
    const val = wantTime ? (rows[rows.length - 1] || {})[col] : reduceField(rows, col, calc);
    const mapped = mapValue(panel, val);
    return {
      col,
      val,
      mapped,
      name: statFieldName(panel, col, rows, metricCol),
      text: formatStat(panel, val, col, mapped),
      unit: statUnit(panel, col, val, mapped),
    };
  });
  const untitled = !String(panel.title || "").trim();
  const showName = textMode === "value_and_name" || textMode === "name" || (textMode === "auto" && untitled);
  const htmlFor = (it, named) => {
    const colorCss = styleColor(it.mapped?.color);
    const color = colorCss ? ` style="${colorCss}"` : "";
    const name = named && it.name ? `<span class="stat-name">${escapeHtml(it.name)}</span>` : "";
    const unit = it.unit ? `<span class="stat-unit">${escapeHtml(it.unit)}</span>` : "";
    return `<div class="stat"${color}>${name}<span class="stat-val">${escapeHtml(it.text || "–")}</span>${unit}</div>`;
  };
  if (items.length > 1) {
    body.innerHTML = `<div class="stat-grid">${items.map((it) => htmlFor(it, true)).join("")}</div>`;
    return;
  }
  const it = items[0] || { text: "–", name: "", unit: "", mapped: {} };
  if (showName && it.name) setPanelHeading(body, it.name);
  body.innerHTML = htmlFor(it, false);
  if (type === "stat") return;
  const n = typeof it.val === "number" ? it.val : Number(it.val);
  if (Number.isFinite(n)) {
    const unit = fieldOverride(panel, it.col).unit || defaultsUnit;
    const pct = isPercentUnit(unit, it.col, n)
      ? Math.max(0, Math.min(100, n <= 1.5 ? n * 100 : n))
      : Math.max(0, Math.min(100, n));
    body.innerHTML += `<div class="gauge-bar"><span style="width:${pct}%;${styleBg(it.mapped.color) || styleBg("var(--accent)")}"></span></div>`;
  }
}

function formatStat(panel, val, col, mapped) {
  if (val == null || val === "") return "–";
  const props = fieldOverride(panel, col);
  const unit = props.unit || panel.fieldConfig?.defaults?.unit || "";
  if (unit === "dateTimeAsLocal" || col === "time") return formatTime(val);
  if (mapped?.mapped) return mapped.text;
  if (typeof val === "string" && !Number.isFinite(Number(val))) return val;
  const n = Number(val);
  if (!Number.isFinite(n)) return String(val);
  if ((unit === "m" || unit === "min") && /duration/i.test(col || "")) return formatDuration(n * 60);
  if (isPercentUnit(unit, col, n) || unit === "percent") {
    const pct = unit === "percent" || n > 1.5 ? n : n * 100;
    return pct.toFixed(props.decimals ?? 1);
  }
  if (unit === "s" || unit === "dtdurations") return formatDuration(n);
  return props.decimals != null ? n.toFixed(props.decimals) : formatNum(n);
}

function statUnit(panel, col, val, mapped) {
  if (val == null || val === "" || mapped?.mapped) return "";
  const props = fieldOverride(panel, col);
  const unit = props.unit || panel.fieldConfig?.defaults?.unit || "";
  if (unit === "dateTimeAsLocal" || unit === "s" || unit === "dtdurations") return "";
  if ((unit === "m" || unit === "min") && /duration/i.test(col || "")) return "";
  if (typeof val === "string" && !Number.isFinite(Number(val))) return "";
  const n = Number(val);
  if (isPercentUnit(unit, col, n) || unit === "percent") return "%";
  return grafanaUnit(unit, col);
}

function formatTime(v) {
  let ms = null;
  if (typeof v === "number") ms = v > 1e12 ? v : v * 1000;
  else ms = Date.parse(String(v).replace(" ", "T") + "Z");
  if (!Number.isFinite(ms)) return String(v ?? "–");
  const d = new Date(ms);
  return d.toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" });
}

function drawStateTimeline(el, panel, cols, rows) {
  if (!rows.length) {
    el.innerHTML = '<div class="err">no data</div>';
    return;
  }
  const timeCol = cols.find((c) => c === "time" || c === "date") || cols[0];
  const valCol = cols.find((c) => c !== timeCol) || cols[0];
  const byTime = new Map();
  for (const r of rows) {
    const t = toEpoch(r[timeCol]);
    if (t == null) continue;
    byTime.set(t, r[valCol]);
  }
  const pts = [...byTime.entries()].sort((a, b) => a[0] - b[0]).map(([t, v]) => ({ t, v }));
  if (!pts.length) {
    el.innerHTML = '<div class="err">no data</div>';
    return;
  }
  const fromEl = $("from")?.value;
  const toEl = $("to")?.value;
  let from = fromEl ? new Date(fromEl).getTime() / 1000 : pts[0].t;
  let to = toEl ? new Date(toEl).getTime() / 1000 : pts[pts.length - 1].t;
  if (!Number.isFinite(from) || !Number.isFinite(to) || to <= from) {
    from = pts[0].t;
    to = Math.max(pts[pts.length - 1].t, from + 1);
  }
  const segs = [];
  for (let i = 0; i < pts.length; i++) {
    const start = Math.max(pts[i].t, from);
    const rawEnd = i + 1 < pts.length ? pts[i + 1].t : to;
    const end = Math.min(rawEnd, to);
    if (end <= start || pts[i].t > to || rawEnd < from) continue;
    const last = segs[segs.length - 1];
    if (last && last.v === pts[i].v) last.end = end;
    else segs.push({ start, end, v: pts[i].v });
  }
  if (segs.length && segs[segs.length - 1].end < to) segs[segs.length - 1].end = to;

  const w = Math.max(320, el.clientWidth || 640);
  const barH = 36;
  const axisH = 22;
  const h = barH + axisH + 8;
  const span = to - from;
  const parts = [];
  parts.push(`<svg class="state-tl" viewBox="0 0 ${w} ${h}" preserveAspectRatio="none">`);
  for (const s of segs) {
    const x = ((s.start - from) / span) * w;
    const sw = Math.max(1.5, ((s.end - s.start) / span) * w);
    const m = mapValue(panel, s.v);
    const fill = safeCssColor(m.color) || "#8b93a7";
    const tc = luminance(fill) > 0.55 ? "#111217" : "#f4f6fb";
    const label = sw > 44 ? m.text : "";
    const title = `${m.text} · ${formatTime(s.start)} – ${formatTime(s.end)}`;
    parts.push(
      `<g><title>${escapeHtml(title)}</title><rect x="${x.toFixed(2)}" y="4" width="${sw.toFixed(2)}" height="${barH}" rx="3" fill="${fill}"/>` +
        (label
          ? `<text x="${(x + sw / 2).toFixed(2)}" y="${4 + barH / 2 + 4}" text-anchor="middle" fill="${tc}" font-size="11" font-weight="600">${escapeHtml(label)}</text>`
          : "") +
        `</g>`
    );
  }
  const ticks = 5;
  for (let i = 0; i <= ticks; i++) {
    const t = from + (span * i) / ticks;
    const x = (w * i) / ticks;
    parts.push(
      `<text x="${x.toFixed(2)}" y="${h - 4}" text-anchor="${i === 0 ? "start" : i === ticks ? "end" : "middle"}" fill="#8b93a7" font-size="10">${escapeHtml(axisLabel(t, span))}</text>`
    );
  }
  parts.push("</svg>");
  const seen = new Map();
  for (const s of segs) {
    const m = mapValue(panel, s.v);
    if (!seen.has(m.text)) seen.set(m.text, m.color || "#8b93a7");
  }
  const legend = [...seen.entries()]
    .map(([name, color]) => {
      const bg = styleBg(color);
      return `<span class="tl-swatch"><i${bg ? ` style="${bg}"` : ""}></i>${escapeHtml(name)}</span>`;
    })
    .join("");
  el.innerHTML = parts.join("") + `<div class="tl-legend">${legend}</div>`;
}

function axisLabel(epochSec, spanSec) {
  const d = new Date(epochSec * 1000);
  if (spanSec <= 48 * 3600) {
    return d.toLocaleString(undefined, { weekday: "short", hour: "2-digit", minute: "2-digit" });
  }
  return d.toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

function luminance(hex) {
  const h = String(hex || "").replace("#", "");
  if (h.length < 6) return 0.5;
  const r = parseInt(h.slice(0, 2), 16) / 255;
  const g = parseInt(h.slice(2, 4), 16) / 255;
  const b = parseInt(h.slice(4, 6), 16) / 255;
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

function colMatch(cols, name) {
  if (!name) return null;
  return (cols || []).find((c) => String(c).toLowerCase() === String(name).toLowerCase()) || null;
}

function isClockDuration(v) {
  return typeof v === "string" && /^\d{1,2}:\d{2}(:\d{2})?$/.test(String(v).trim());
}

function isTimeColumn(col, rows) {
  if (!col) return false;
  const sample = (rows || []).find((r) => r[col] != null && r[col] !== "")?.[col];
  if (isClockDuration(sample)) return false;
  if (typeof sample === "number") return sample > 1e9;
  if (/^(time|date)$/i.test(col) || /(_ts|_date|_at)$/i.test(col)) {
    if (typeof sample === "string" && /^\d{4}-\d{2}/.test(sample)) return true;
    if (typeof sample === "number") return sample > 1e9;
    return false;
  }
  if (typeof sample === "string") {
    const t = Date.parse(sample.includes("T") || sample.includes(" ") ? sample.replace(" ", "T") + (/Z|[+-]\d\d/.test(sample) ? "" : "Z") : sample);
    return Number.isFinite(t) && t > 1e11;
  }
  return false;
}

function xySeriesSpec(panel, frameIndex, cols) {
  const spec = (panel.options?.series || [])[frameIndex] || (panel.options?.series || [])[0] || {};
  const xWant = spec.x?.matcher?.options;
  const yWant = spec.y?.matcher?.options;
  const x = colMatch(cols, xWant) || cols.find((c) => /^(odometer|mileage|soc)$/i.test(c)) || cols[0];
  const y =
    colMatch(cols, yWant) ||
    cols.find((c) => c !== x && !isTimeColumn(c, []) && !/^date$|^title$/i.test(c) && !/id$/i.test(c)) ||
    cols[1] ||
    cols[0];
  return { x, y };
}

function drawXyChart(el, panel, results) {
  const frames = (results || []).filter((r) => r && r.ok !== false && (r.rows || []).length);
  if (!frames.length) {
    el.innerHTML = '<div class="err">no data</div>';
    return;
  }
  const parsed = frames.map((f, i) => {
    const cols = f.columns || [];
    const { x, y } = xySeriesSpec(panel, i, cols);
    const pts = (f.rows || [])
      .map((r) => [num(r[x]), num(r[y])])
      .filter(([a, b]) => a != null && b != null)
      .sort((a, b) => a[0] - b[0]);
    return { x, y, pts, label: headerLabel(panel, y) };
  }).filter((s) => s.pts.length);
  if (!parsed.length) {
    el.innerHTML = '<div class="err">no data</div>';
    return;
  }
  const xs = [...new Set(parsed.flatMap((s) => s.pts.map((p) => p[0])))].sort((a, b) => a - b);
  const series = [{ label: headerLabel(panel, parsed[0].x) }];
  const data = [xs];
  parsed.forEach((s, i) => {
    const lookup = new Map(s.pts);
    data.push(xs.map((x) => (lookup.has(x) ? lookup.get(x) : null)));
    const line = panel.fieldConfig?.overrides?.some((o) => o.properties?.some((p) => p.id === "custom.show" && p.value === "lines")) && i > 0;
    series.push({
      label: s.label,
      stroke: seriesStroke(panel, s.y, i),
      width: line || parsed.length === 1 ? 1.5 : i === 0 ? 0 : 1.5,
      points: { show: i === 0, size: 6 },
    });
  });
  el.innerHTML = "";
  const plot = document.createElement("div");
  el.appendChild(plot);
  new uPlot(
    {
      width: el.clientWidth || 400,
      height: Math.max(120, el.clientHeight - 8),
      series,
      axes: [
        { stroke: "#8b93a7", values: (_u, splits) => splits.map((v) => formatNum(v)) },
        { stroke: "#8b93a7" },
      ],
      scales: { x: { time: false } },
    },
    data,
    plot
  );
}

function drawBarGauge(el, panel, cols, rows) {
  if (!rows.length) {
    el.innerHTML = '<div class="err">no data</div>';
    return;
  }
  const show = prettyCols(cols, panel);
  const nameCol = show.find((c) => rows.some((r) => typeof r[c] === "string" && r[c] !== "")) || show[0];
  const valCol = show.find((c) => c !== nameCol && rows.some((r) => num(r[c]) != null)) || show.find((c) => c !== nameCol) || show[0];
  const nums = rows.map((r) => num(r[valCol])).filter((n) => n != null);
  const maxCfg = Number(panel.fieldConfig?.defaults?.max);
  const max = Number.isFinite(maxCfg) && maxCfg > 0 ? maxCfg : Math.max(...nums, 1);
  const slice = rows.slice(0, 25);
  el.innerHTML = `<div class="bargauge">${slice
    .map((r) => {
      const n = num(r[valCol]) ?? show.map((c) => (c === nameCol ? null : num(r[c]))).find((x) => x != null) ?? null;
      const pct = n == null ? 0 : Math.max(0, Math.min(100, (n / max) * 100));
      const label = r[nameCol] == null ? "" : String(r[nameCol]);
      const color = thresholdColor(panel.fieldConfig?.defaults?.thresholds?.steps, n) || "var(--accent)";
      const bg = styleBg(color);
      return `<div class="bg-row"><span class="bg-name">${escapeHtml(label)}</span><div class="bg-track"><span style="width:${pct}%;${bg}"></span></div><span class="bg-val">${escapeHtml(n == null ? "" : formatNum(n))}</span></div>`;
    })
    .join("")}</div>`;
}

const SERIES_COLORS = ["#73BF69", "#FF9830", "#3274D9", "#e85d04", "#8F3BB8", "#6ED0E0", "#E02F44", "#FADE2A"];

function seriesStroke(panel, name, i) {
  return sliceColor(panel, name, i);
}

function sliceColor(panel, name, i) {
  const props = fieldOverride(panel, name);
  const raw = typeof props.color === "string" ? props.color : props.color?.fixedColor;
  if (raw) return namedColor(raw) || raw;
  if (/dc/i.test(name) && !/ac/i.test(name)) return "#FF9830";
  if (/\bac\b/i.test(name)) return "#73BF69";
  return SERIES_COLORS[i % SERIES_COLORS.length];
}

function formatPieValue(panel, n, col) {
  const unit = fieldOverride(panel, col).unit || panel.fieldConfig?.defaults?.unit || "";
  if (unit === "s" || unit === "dtdurations") return formatDuration(n);
  if (isPercentUnit(unit, col, n) || unit === "percent") {
    const pct = unit === "percent" || n > 1.5 ? n : n * 100;
    return `${pct.toFixed(1)}%`;
  }
  const text = formatNum(n);
  return withUnit(text, unit, col);
}

function polar(cx, cy, r, a) {
  return [cx + r * Math.cos(a), cy + r * Math.sin(a)];
}

function piePath(cx, cy, r, a0, a1) {
  if (a1 - a0 >= Math.PI * 2 - 1e-6) {
    return `M ${cx - r} ${cy} A ${r} ${r} 0 1 1 ${cx + r} ${cy} A ${r} ${r} 0 1 1 ${cx - r} ${cy}`;
  }
  const large = a1 - a0 > Math.PI ? 1 : 0;
  const [x0, y0] = polar(cx, cy, r, a0);
  const [x1, y1] = polar(cx, cy, r, a1);
  return `M ${cx} ${cy} L ${x0} ${y0} A ${r} ${r} 0 ${large} 1 ${x1} ${y1} Z`;
}

function drawPieChart(el, panel, cols, rows) {
  if (!rows.length) {
    el.innerHTML = '<div class="err">no data</div>';
    return;
  }
  const nameCol =
    cols.find((c) => /^(metric|current|name)$/i.test(c)) ||
    cols.find((c) => !isTimeColumn(c, rows) && rows.some((r) => typeof r[c] === "string" && r[c] !== ""));
  const valCol =
    cols.find((c) => /^(value|duration_sec)$/i.test(c)) ||
    cols.find((c) => c !== nameCol && !isTimeColumn(c, rows) && rows.some((r) => num(r[c]) != null));
  const grouped = new Map();
  for (const r of rows) {
    const name = nameCol ? String(r[nameCol] ?? "–") : headerLabel(panel, valCol);
    const v = num(r[valCol]);
    if (v == null || v <= 0) continue;
    grouped.set(name, (grouped.get(name) || 0) + v);
  }
  const slices = [...grouped.entries()].map(([name, v]) => ({ name, v })).sort((a, b) => b.v - a.v);
  const total = slices.reduce((s, x) => s + x.v, 0);
  if (!slices.length || total <= 0) {
    el.innerHTML = '<div class="err">no data</div>';
    return;
  }
  const labelBits = panel.options?.displayLabels || [];
  const showLegend = panel.options?.legend?.showLegend !== false;
  const cx = 100;
  const cy = 100;
  const r = 86;
  let a = -Math.PI / 2;
  const paths = [];
  const labels = [];
  slices.forEach((s, i) => {
    const sweep = (s.v / total) * Math.PI * 2;
    const a1 = a + sweep;
    const color = safeCssColor(sliceColor(panel, s.name, i)) || "#8b93a7";
    const pct = (s.v / total) * 100;
    const tip = `${s.name}: ${formatPieValue(panel, s.v, valCol)} (${pct.toFixed(1)}%)`;
    paths.push(`<path d="${piePath(cx, cy, r, a, a1)}" fill="${color}"><title>${escapeHtml(tip)}</title></path>`);
    if (labelBits.length && sweep > 0.28) {
      const parts = [];
      if (labelBits.includes("name")) parts.push(s.name);
      if (labelBits.includes("value")) parts.push(formatPieValue(panel, s.v, valCol));
      if (labelBits.includes("percent")) parts.push(`${pct.toFixed(0)}%`);
      const mid = a + sweep / 2;
      const [lx, ly] = polar(cx, cy, r * 0.55, mid);
      labels.push(
        `<text x="${lx.toFixed(1)}" y="${ly.toFixed(1)}" text-anchor="middle" dominant-baseline="middle">${escapeHtml(parts.join(" · "))}</text>`
      );
    }
    a = a1;
  });
  const legend = showLegend
    ? `<div class="pie-legend">${slices
        .map((s, i) => {
          const pct = (s.v / total) * 100;
          const legBg = styleBg(sliceColor(panel, s.name, i));
          return `<div class="pie-leg"><i${legBg ? ` style="${legBg}"` : ""}></i><b>${escapeHtml(s.name)}</b><span>${escapeHtml(formatPieValue(panel, s.v, valCol))}</span><span>${pct.toFixed(1)}%</span></div>`;
        })
        .join("")}</div>`
    : "";
  el.innerHTML = `<div class="pie${showLegend ? "" : " pie-only"}"><svg viewBox="0 0 200 200" aria-label="pie chart">${paths.join("")}${labels.join("")}</svg>${legend}</div>`;
}

function drawBarChart(el, panel, cols, rows) {
  if (!rows.length) {
    el.innerHTML = '<div class="err">no data</div>';
    return;
  }
  const xCol = colMatch(cols, panel.options?.xField) || cols.find((c) => /^speed$/i.test(c)) || cols[0];
  const yCol =
    cols.find((c) => c !== xCol && rows.some((r) => num(r[c]) != null) && !isClockDuration(rows.find((r) => r[c] != null)?.[c])) ||
    cols[1];
  const timeCol = cols.find((c) => /^time$/i.test(c) && c !== xCol && c !== yCol);
  const raw = rows
    .map((r) => ({ x: num(r[xCol]), y: num(r[yCol]), time: timeCol ? r[timeCol] : null }))
    .filter((p) => p.x != null && p.y != null)
    .sort((a, b) => a.x - b.x);
  if (!raw.length) {
    el.innerHTML = '<div class="err">no data</div>';
    return;
  }
  const by = new Map(raw.map((p) => [p.x, p]));
  const xs = raw.map((p) => p.x);
  const minX = xs[0];
  const maxX = xs[xs.length - 1];
  const diffs = xs.slice(1).map((x, i) => x - xs[i]).filter((d) => d > 0);
  const step = diffs.length ? Math.min(...diffs) : 10;
  const pts = [];
  for (let x = minX; x <= maxX + step / 2; x += step) {
    const hit = by.get(x) || by.get(Number(x.toFixed(6)));
    pts.push(hit || { x, y: 0, time: null });
  }
  const max = Math.max(...pts.map((p) => p.y), 0.001);
  const yUnit = grafanaUnit(fieldOverride(panel, yCol).unit || "", yCol);
  el.innerHTML =
    `<div class="hist">${pts
      .map((p) => {
        const h = Math.max(0, (p.y / max) * 100);
        const val = yUnit === "%" ? `${Number(p.y).toFixed(1)}%` : formatNum(p.y);
        const tip = [headerLabel(panel, xCol) + " " + p.x, val, p.time].filter(Boolean).join(" · ");
        return `<div class="hist-col" title="${escapeHtml(tip)}"><div class="hist-bar"><span style="height:${h}%"></span></div><label>${escapeHtml(String(p.x))}</label></div>`;
      })
      .join("")}</div>`;
}

function osmTiles() {
  return L.tileLayer("https://{s}.tile.openstreetmap.org/{z}/{x}/{y}.png", {
    attribution: "&copy; OSM",
    maxZoom: 19,
    errorTileUrl: "data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7",
  });
}

function ensureRouteMap(el) {
  if (el._tmMap) return el._tmMap;
  el.innerHTML = "";
  const map = L.map(el).setView([54, -2], 6);
  osmTiles().addTo(map);
  const g = { map, line: null, fitted: false };
  el._tmMap = g;
  return g;
}

function drawGeomap(el, panel, cols, rows, opts = {}) {
  const preview = !!opts.preview;
  const layerType = (panel.options?.layers || []).map((l) => l.type).find(Boolean) || "route";
  if (layerType === "markers") {
    if (el._tmMap) {
      el._tmMap.map.remove();
      el._tmMap = null;
    }
    const map = L.map(el).setView([54, -2], 6);
    osmTiles().addTo(map);
    const latKey = cols.find((c) => /lat/i.test(c));
    const lonKey = cols.find((c) => /lon|lng/i.test(c));
    const pts = [];
    const nameKey = cols.find((c) => /loc|name|address/i.test(c));
    const sizeKey = cols.find((c) => /chg_total|charges|energy/i.test(c));
    const sizes = rows.map((r) => Number(r[sizeKey])).filter((n) => Number.isFinite(n));
    const maxS = Math.max(...sizes, 1);
    for (const r of rows) {
      const la = Number(r[latKey]);
      const lo = Number(r[lonKey]);
      if (!Number.isFinite(la) || !Number.isFinite(lo)) continue;
      pts.push([la, lo]);
      const n = Number(r[sizeKey]);
      const rad = 6 + (Number.isFinite(n) ? (n / maxS) * 14 : 0);
      const m = L.circleMarker([la, lo], { radius: rad, color: "#e85d04", fillOpacity: 0.7, weight: 1 }).addTo(map);
      const label = [r[nameKey], Number.isFinite(n) ? formatNum(n) : null].filter(Boolean).join(" · ");
      if (label) m.bindTooltip(label);
    }
    if (pts.length) map.fitBounds(pts, { padding: [16, 16] });
    return;
  }
  rows = downsampleRows(rows, preview ? 1500 : 8000);
  const latKey = cols.find((c) => /lat/i.test(c));
  const lonKey = cols.find((c) => /lon|lng/i.test(c));
  const pts = [];
  for (const r of rows) {
    const la = Number(r[latKey]);
    const lo = Number(r[lonKey]);
    if (Number.isFinite(la) && Number.isFinite(lo)) pts.push([la, lo]);
  }
  const g = ensureRouteMap(el);
  if (g.line) {
    g.line.setLatLngs(pts);
    g.line.setStyle({
      weight: preview ? 2 : 3,
      opacity: preview ? 0.55 : 1,
      dashArray: preview ? "6 8" : null,
    });
  } else if (pts.length) {
    g.line = L.polyline(pts, {
      color: "#e85d04",
      weight: preview ? 2 : 3,
      opacity: preview ? 0.55 : 1,
      dashArray: preview ? "6 8" : null,
    }).addTo(g.map);
  }
  if (pts.length && (preview || !g.fitted)) {
    g.map.fitBounds(pts, { padding: [16, 16] });
    g.fitted = true;
  }
  requestAnimationFrame(() => g.map.invalidateSize());
}

function drawHeatmap(el, panel, cols, rows) {
  if (!rows.length) {
    el.innerHTML = '<div class="err">no data</div>';
    return;
  }
  const timeCol = cols.find((c) => c === "time" || isTimeColumn(c, rows)) || cols[0];
  const startCol =
    cols.find((c) => /start_battery/i.test(c)) || cols.find((c) => /start/i.test(c) && c !== timeCol);
  const endCol = cols.find((c) => /end_battery/i.test(c)) || cols.find((c) => /end/i.test(c) && c !== timeCol);
  const y0 = startCol || cols.find((c) => c !== timeCol);
  const y1 = endCol || y0;
  const step = 5;
  const bands = [];
  for (let s = 100; s >= 0; s -= step) bands.push(s);
  const slice = rows.slice(0, 48);
  el.innerHTML = `<div class="heatmap"><div class="hm-grid" style="grid-template-columns: 2.2em repeat(${slice.length}, minmax(8px, 1fr))">${bands
    .map((b) => {
      const cells = slice
        .map((r) => {
          const a = Number(r[y0]);
          const c = Number(r[y1]);
          if (!Number.isFinite(a) && !Number.isFinite(c)) return '<i></i>';
          const lo = Math.min(Number.isFinite(a) ? a : c, Number.isFinite(c) ? c : a);
          const hi = Math.max(Number.isFinite(a) ? a : c, Number.isFinite(c) ? c : a);
          const on = b <= hi && b + step > lo;
          const t = formatTime(r[timeCol]);
          return `<i class="${on ? "on" : ""}" title="${escapeHtml(`${t} · ${lo.toFixed(0)}–${hi.toFixed(0)}%`)}"></i>`;
        })
        .join("");
      return `<b>${b}</b>${cells}`;
    })
    .join("")}</div></div>`;
}

function chartTimeCol(cols, rows) {
  return (
    (cols || []).find((c) => c === "time" || c === "series_id") ||
    (cols || []).find((c) => isTimeColumn(c, rows)) ||
    null
  );
}

function timeSeriesFrames(results, cols, rows) {
  const frames = (results || []).filter((f) => f && f.ok !== false && (f.rows || []).length);
  const timed = frames.filter((f) => chartTimeCol(f.columns, f.rows));
  if (timed.length) return timed;
  if ((rows || []).length && chartTimeCol(cols, rows)) return [{ columns: cols, rows }];
  return [];
}

function drawChart(el, panel, cols, rows, results) {
  if (panel?.type === "xychart") {
    drawXyChart(el, panel, results && results.length ? results : [{ ok: true, columns: cols, rows }]);
    return;
  }
  const frames = timeSeriesFrames(results, cols, rows);
  const points = new Map();
  const ys = [];
  const ySeen = new Set();
  for (const f of frames) {
    const tcol = chartTimeCol(f.columns, f.rows);
    for (const c of f.columns || []) {
      if (c === tcol || hiddenCol(c, panel) || /^(lower|upper)$/i.test(c)) continue;
      if (!f.rows.some((r) => num(r[c]) != null)) continue;
      if (!ySeen.has(c)) {
        ySeen.add(c);
        ys.push(c);
      }
    }
    for (const r of downsampleRows(f.rows || [], 2500)) {
      const x = toEpoch(r[tcol]) ?? num(r[tcol]);
      if (x == null) continue;
      const rec = points.get(x) || {};
      for (const c of ys) {
        const n = num(r[c]);
        if (n != null) rec[c] = n;
      }
      points.set(x, rec);
    }
  }
  const xs = [...points.keys()].sort((a, b) => a - b);
  if (!xs.length || !ys.length) {
    el.innerHTML = '<div class="err">no data</div>';
    return;
  }
  const series = [{ label: "time" }].concat(
    ys.map((y, i) => ({ label: headerLabel(panel, y), stroke: seriesStroke(panel, y, i), width: 1.5 }))
  );
  const data = [xs].concat(ys.map((y) => xs.map((x) => points.get(x)[y] ?? null)));
  const yMin = panel.fieldConfig?.defaults?.min;
  const yMax = panel.fieldConfig?.defaults?.max;
  const scales = { x: { time: true } };
  if (yMin != null || yMax != null) scales.y = { auto: false, min: yMin ?? 0, max: yMax ?? 100 };
  el.innerHTML = "";
  const plot = document.createElement("div");
  el.appendChild(plot);
  new uPlot(
    {
      width: el.clientWidth || 400,
      height: Math.max(120, el.clientHeight - 8),
      series,
      axes: [{ stroke: "#8b93a7" }, { stroke: "#8b93a7" }],
      scales,
    },
    data,
    plot
  );
}

function toEpoch(v) {
  if (v == null) return null;
  if (typeof v === "number") return v > 1e12 ? v / 1000 : v;
  const t = Date.parse(String(v).replace(" ", "T") + "Z");
  return Number.isFinite(t) ? t / 1000 : null;
}
function num(v) {
  if (v == null || v === "") return null;
  const n = Number(v);
  return Number.isFinite(n) ? n : null;
}
function formatNum(n) {
  if (!Number.isFinite(n)) return "–";
  if (Math.abs(n) >= 100) return n.toFixed(0);
  if (Math.abs(n) >= 10) return n.toFixed(1);
  return n.toFixed(2);
}
function fmt(v) {
  if (v == null) return "";
  if (typeof v === "number") return formatNum(v);
  return String(v);
}

const META = [
  {
    id: "vehicle",
    title: "Vehicle",
    lead: "Where the car is, and what it is doing — locks, sentry, tires, and climate — from the last Tesla API poll.",
    live: true,
    sections: [
      {
        title: "Awake and asleep",
        panels: [
          ["states.json", 2],
          ["states.json", 6],
          ["states.json", 8],
          ["states.json", 14],
        ],
      },
    ],
  },
  {
    id: "battery",
    title: "Battery",
    lead: "Level, health, and every charge. Open a row to see the place and how the power came in.",
    sections: [
      { title: "Level", panels: [["charge-level.json", 2]] },
      {
        title: "Health",
        panels: [
          ["battery-health.json", 13],
          ["battery-health.json", 14],
          ["battery-health.json", 17],
          ["battery-health.json", 12],
          ["battery-health.json", 27],
          ["battery-health.json", 28],
        ],
      },
      {
        title: "Charging",
        panels: [
          ["charging-stats.json", 8],
          ["charging-stats.json", 10],
          ["charging-stats.json", 14],
          ["charging-stats.json", 27],
          ["charging-stats.json", 26],
          ["charging-stats.json", 31],
          ["charging-stats.json", 32],
          ["charging-stats.json", 33],
          ["charging-stats.json", 15],
          ["charging-stats.json", 16],
          ["charging-stats.json", 18],
          ["charging-stats.json", 24],
          ["charging-stats.json", 20],
          ["charging-stats.json", 29],
          ["charging-stats.json", 2],
          ["charging-stats.json", 13],
          ["charging-stats.json", 4],
          ["charging-stats.json", 6],
          ["charges.json", 10],
          ["charges.json", 20],
          ["charges.json", 14],
          ["charges.json", 15],
          ["charges.json", 6],
          ["charges.json", 17],
        ],
      },
      { title: "While parked", panels: [["vampire-drain.json", 2]] },
      {
        title: "Projected range",
        panels: [
          ["projected-range.json", 2],
          ["projected-range.json", 6],
          ["projected-range.json", 5],
        ],
      },
    ],
  },
  {
    id: "trips",
    title: "Trips",
    lead: "Distance, efficiency, and places. Open a drive to follow the route.",
    sections: [
      {
        title: "This period",
        panels: [
          ["drive-stats.json", 20],
          ["drive-stats.json", 16],
          ["drive-stats.json", 22],
          ["drive-stats.json", 26],
          ["drive-stats.json", 8],
          ["drive-stats.json", 14],
          ["drive-stats.json", 33],
          ["drive-stats.json", 35],
          ["drive-stats.json", 34],
          ["drive-stats.json", 36],
          ["drive-stats.json", 32],
          ["drive-stats.json", 30],
          ["drive-stats.json", 24],
        ],
      },
      {
        title: "Efficiency",
        panels: [
          ["efficiency.json", 4],
          ["efficiency.json", 8],
          ["efficiency.json", 6],
          ["efficiency.json", 2],
          ["efficiency.json", 14],
          ["efficiency.json", 12],
          ["efficiency.json", 15],
        ],
      },
      {
        title: "Drives",
        panels: [
          ["drives.json", 4],
          ["drives.json", 5],
          ["drives.json", 6],
          ["drives.json", 7],
          ["drives.json", 2],
          ["drives.json", 9],
        ],
      },
      { title: "Mileage", panels: [["mileage.json", 2]] },
      { title: "Timeline", panels: [["timeline.json", 2]] },
      { title: "By period", panels: [["statistics.json", 2]] },
      {
        title: "Selected trip",
        when: "trip",
        panels: [
          ["trip.json", 6],
          ["trip.json", 10],
          ["trip.json", 38],
          ["trip.json", 26],
          ["trip.json", 28],
          ["trip.json", 30],
          ["trip.json", 32],
          ["trip.json", 22],
          ["trip.json", 43],
          ["trip.json", 40],
          ["trip.json", 20],
          ["trip.json", 42],
          ["trip.json", 8],
        ],
      },
      {
        title: "Places",
        panels: [
          ["visited.json", 2],
          ["visited.json", 5],
          ["visited.json", 6],
          ["visited.json", 7],
          ["locations.json", 12],
          ["locations.json", 20],
          ["locations.json", 18],
          ["locations.json", 16],
          ["locations.json", 10],
          ["locations.json", 14],
          ["locations.json", 22],
          ["locations.json", 2],
          ["locations.json", 6],
        ],
      },
      { title: "Dutch tax", panels: [["reports/dutch-tax.json", 2]] },
    ],
  },
  {
    id: "software",
    title: "Software",
    lead: "Firmware on the car, then this installation. The version you are running is the update history, not a second copy of the overview tile.",
    sections: [
      {
        title: "Car",
        panels: [
          ["updates.json", 8],
          ["updates.json", 6],
          ["updates.json", 2],
        ],
      },
      {
        title: "This installation",
        panels: [
          ["database-info.json", 32],
          ["database-info.json", 36],
          ["database-info.json", 39],
          ["database-info.json", 42],
          ["database-info.json", 51],
          ["database-info.json", 33],
          ["database-info.json", 38],
          ["database-info.json", 41],
          ["database-info.json", 35],
          ["database-info.json", 52],
          ["database-info.json", 50],
          ["database-info.json", 48],
          ["database-info.json", 49],
          ["database-info.json", 47],
          ["database-info.json", 45],
          ["database-info.json", 46],
        ],
      },
    ],
  },
];

function clearLive() {
  if (liveTimer) clearTimeout(liveTimer);
  liveTimer = null;
}

function reloadView() {
  if (uiLayout === "grouped") return loadGrouped(currentMeta);
  return loadDashboard(currentPath);
}

function flowSpan(panel) {
  const w = panel.gridPos?.w || 24;
  if (w >= 20) return 12;
  if (w >= 12) return 6;
  if (w >= 8) return 4;
  if (w >= 4) return 3;
  return 2;
}

function flowBodyHeight(panel) {
  const h = panel.gridPos?.h || 8;
  if (panel.type === "stat" || panel.type === "gauge") return h <= 3 ? 88 : 120;
  if (panel.type === "table") return Math.min(480, Math.max(220, h * 16));
  if (panel.type === "geomap") return 360;
  if (panel.type === "piechart" || panel.type === "bargauge") return 260;
  if (panel.type === "text") return 96;
  return Math.min(420, Math.max(200, h * 14));
}

async function dashboardByPath(path, signal) {
  const cached = dashCache.get(path);
  if (cached) return cached;
  const pending = api("/api/dashboards/" + path, { signal }).catch((e) => {
    dashCache.delete(path);
    throw e;
  });
  dashCache.set(path, pending);
  return pending;
}

function n1(v) {
  const n = Number(v);
  if (!Number.isFinite(n)) return "–";
  return Math.abs(n) >= 100 ? n.toFixed(0) : n.toFixed(1);
}

function hoursLabel(hours, minutes) {
  const m = Number.isFinite(Number(minutes))
    ? Number(minutes)
    : Number.isFinite(Number(hours))
      ? Math.round(Number(hours) * 60)
      : null;
  if (m == null || m <= 0) return null;
  const h = Math.floor(m / 60);
  const rem = m % 60;
  if (h <= 0) return `${rem} min`;
  return rem ? `${h} h ${rem} min` : `${h} h`;
}

function chip(text, kind) {
  return `<span class="chip${kind ? " " + kind : ""}">${escapeHtml(text)}</span>`;
}

function rangeApart(a, b) {
  const x = Number(a);
  const y = Number(b);
  if (!Number.isFinite(x)) return false;
  if (!Number.isFinite(y)) return true;
  return Math.abs(x - y) > 1;
}

function climateLine(c, temp) {
  const bits = [];
  if (c.inside != null) bits.push(`Inside ${n1(c.inside)}°${temp}`);
  if (c.outside != null) bits.push(`Outside ${n1(c.outside)}°${temp}`);
  if (c.setpoint != null) bits.push(`Set ${n1(c.setpoint)}°${temp}`);
  const set = Number(c.setpoint);
  const pass = Number(c.passenger);
  if (c.passenger != null && Number.isFinite(pass) && (!Number.isFinite(set) || Math.abs(pass - set) >= 0.5)) {
    bits.push(`Passenger ${n1(pass)}°${temp}`);
  }
  if (c.defrostFront || c.defrostRear) bits.push("defrost");
  return bits.join(" · ") || "–";
}

function paintLive(host, data) {
  dropMaps(host);
  const car = data.car || {};
  const b = data.battery || {};
  const d = data.drive || {};
  const c = data.climate || {};
  const body = data.body || {};
  const tires = data.tires || {};
  const sw = data.software || {};
  const unit = data.lengthUnit || "km";
  const temp = data.tempUnit || "C";
  const pres = data.pressureUnit || "bar";
  const name = car.name || car.marketingName || "Car";
  const sub = [car.marketingName || (car.model ? "Model " + car.model : ""), car.trim, car.color]
    .filter(Boolean)
    .filter((v, i, a) => a.indexOf(v) === i)
    .join(" · ");
  const level = Number(b.level);
  const limit = Number(b.limit);
  const charging = b.chargingState && !/disconnected|complete|stopped/i.test(b.chargingState);
  const driving = d.shift && /^(D|R|N)$/.test(d.shift);
  const chips = [];
  if (body.locked === true) chips.push(chip("Locked", "on"));
  else if (body.locked === false) chips.push(chip("Unlocked", "warn"));
  if (body.sentry === true) chips.push(chip("Sentry", "hot"));
  else if (body.sentry === false) chips.push(chip("Sentry off"));
  if ((body.doorsOpen || []).length) chips.push(chip("Doors " + body.doorsOpen.join(", "), "warn"));
  else if (data.hasDetail) chips.push(chip("Doors closed"));
  if ((body.windowsOpen || []).length) chips.push(chip("Windows " + body.windowsOpen.join(", "), "warn"));
  else if (data.hasDetail) chips.push(chip("Windows closed"));
  if (body.frunkOpen) chips.push(chip("Frunk open", "warn"));
  if (body.trunkOpen) chips.push(chip("Trunk open", "warn"));
  if (c.on === true) chips.push(chip("Climate on", "on"));
  else if (c.on === false) chips.push(chip("Climate off"));
  if (sw.updateStatus) chips.push(chip("Update " + sw.updateStatus, "hot"));
  const eta = hoursLabel(b.hoursToFull, b.minutesToFull);
  let motion = "";
  if (charging) {
    const bits = [
      b.powerKw != null ? `<b>${escapeHtml(String(b.powerKw))} kW</b>` : "",
      b.voltage ? `${escapeHtml(String(b.voltage))} V` : "",
      b.current ? `${escapeHtml(String(b.current))} A` : "",
      b.energyAddedKwh != null ? `${n1(b.energyAddedKwh)} kWh added` : "",
      eta ? eta + " to limit" : "",
    ].filter(Boolean);
    motion = `<div class="live-charge">${bits.join(" · ")}</div>`;
  } else if (driving) {
    const bits = [
      `<b>${escapeHtml(d.shift)}</b>`,
      d.speed != null ? `${n1(d.speed)} ${escapeHtml(d.speedUnit || "")}` : "",
      d.powerKw != null ? `${escapeHtml(String(d.powerKw))} kW` : "",
      d.heading != null ? `${escapeHtml(String(d.heading))}°` : "",
      d.destination ? `to ${escapeHtml(d.destination)}` : "",
      d.distanceToArrival != null ? `${n1(d.distanceToArrival)} ${escapeHtml(unit)}` : "",
      d.minutesToArrival != null ? `${n1(d.minutesToArrival)} min` : "",
    ].filter(Boolean);
    motion = `<div class="live-drive">${bits.join(" · ")}</div>`;
  }
  const since = data.since ? `Since ${data.since}` : "";
  const detail = data.detailAt ? `Last full read ${data.detailAt}` : data.hasDetail ? "" : "No full vehicle poll yet — lock, sentry, and software update show up after the logger reads the car.";
  const tire = (key, label) => {
    const t = tires[key] || {};
    const cls = t.warning ? "warn" : "";
    return `<div><span>${label}</span><span class="${cls}">${t.pressure == null ? "–" : n1(t.pressure) + " " + escapeHtml(pres)}</span></div>`;
  };
  const prefer = data.preferredRange === "ideal" ? "ideal" : "rated";
  const alts = [];
  if (prefer !== "rated" && rangeApart(b.rated, b.range)) alts.push(`${n1(b.rated)} ${unit} rated`);
  if (prefer !== "ideal" && rangeApart(b.ideal, b.range)) alts.push(`${n1(b.ideal)} ${unit} ideal`);
  if (rangeApart(b.est, b.range)) alts.push(`${n1(b.est)} ${unit} estimated`);
  const where = [
    data.place || "",
    data.elevation != null ? `${n1(data.elevation)} ${data.elevationUnit || "m"}` : "",
  ]
    .filter(Boolean)
    .join(" · ");
  const facts = [
    ["Climate", climateLine(c, temp)],
    ["Tires", ""],
    ["Odometer", data.odometer == null ? "–" : `${n1(data.odometer)} ${unit}`],
    ["Software", sw.version || "–"],
  ];
  const extras = (data.extras || [])
    .map((e) => `<div><dt>${escapeHtml(e.label || "")}</dt><dd>${escapeHtml(e.value || "")}</dd></div>`)
    .join("");
  host.innerHTML = `<div class="live">
    <div class="live-head">
      <div><div class="live-name">${escapeHtml(name)}</div><div class="live-sub">${escapeHtml(sub)}</div></div>
      <div class="live-state ${escapeHtml(String(data.state || "").toLowerCase())}">${escapeHtml(data.state || "unknown")}</div>
    </div>
    <p class="live-since">${escapeHtml([since, detail].filter(Boolean).join(" · "))}</p>
    <div class="chips">${chips.join("")}</div>
    <div class="live-split">
      <div class="live-battery">
        <div class="soc">${Number.isFinite(level) ? escapeHtml(String(level)) : "–"}<span>%</span></div>
        <div class="soc-bar"><span style="width:${Number.isFinite(level) ? Math.max(0, Math.min(100, level)) : 0}%"></span>${Number.isFinite(limit) ? `<i style="left:${Math.max(0, Math.min(100, limit))}%"></i>` : ""}</div>
        <div class="soc-meta">${b.range != null ? n1(b.range) + " " + escapeHtml(unit) + " " + escapeHtml(data.preferredRange || "rated") : "Range –"}${Number.isFinite(limit) ? " · limit " + limit + "%" : ""}${b.usable != null && b.usable !== b.level ? " · usable " + b.usable + "%" : ""}</div>
        ${alts.length ? `<p class="soc-alt">${escapeHtml(alts.join(" · "))}</p>` : ""}
        ${motion}
      </div>
      <div class="live-place">
        <div class="live-map" id="live-map"></div>
        ${where ? `<p class="live-where">${escapeHtml(where)}</p>` : ""}
      </div>
    </div>
    <div class="live-facts">
      <div class="fact"><h3>Climate</h3><p>${escapeHtml(facts[0][1])}</p></div>
      <div class="fact"><h3>Tires</h3><div class="tires">${tire("fl", "FL")}${tire("fr", "FR")}${tire("rl", "RL")}${tire("rr", "RR")}</div></div>
      <div class="fact"><h3>Odometer</h3><p>${escapeHtml(facts[2][1])}</p></div>
      <div class="fact"><h3>Software</h3><p>${escapeHtml(facts[3][1])}</p></div>
    </div>
    ${extras ? `<dl class="live-extra">${extras}</dl>` : ""}
  </div>`;
  const mapEl = host.querySelector(".live-map");
  const lat = Number(d.lat);
  const lon = Number(d.lon);
  if (mapEl && Number.isFinite(lat) && Number.isFinite(lon) && !(lat === 0 && lon === 0)) {
    const map = L.map(mapEl, { zoomControl: false }).setView([lat, lon], 13);
    osmTiles().addTo(map);
    L.circleMarker([lat, lon], { radius: 8, color: "#e85d04", fillOpacity: 0.85, weight: 1 }).addTo(map);
    mapEl._map = map;
    requestAnimationFrame(() => map.invalidateSize());
  } else if (mapEl) {
    mapEl.textContent = "No position yet";
  }
}

async function fillLive(host, ctl) {
  try {
    const data = await api("/api/cars/" + vars().car_id + "/live", { signal: ctl.signal });
    if (stale(ctl) || !host.isConnected) return;
    paintLive(host, data);
  } catch (e) {
    if (isAbort(e) || stale(ctl)) return;
    if (host.isConnected) host.innerHTML = `<div class="err">${escapeHtml(e.message)}</div>`;
  }
  if (!stale(ctl)) liveTimer = setTimeout(() => fillLive(host, ctl), 30000);
}

let storyGen = 0;

function dropMaps(root) {
  if (!root) return;
  const seen = new Set();
  const nodes = [root, ...root.querySelectorAll("*")];
  for (const el of nodes) {
    const map = el._map;
    el._map = null;
    if (!map || seen.has(map)) continue;
    seen.add(map);
    try {
      map.remove();
    } catch {
      /* already torn down */
    }
  }
}

function storyWhen(s) {
  const m = String(s || "").match(/^(\d{4})-(\d{2})-(\d{2})[ T](\d{2}):(\d{2})/);
  if (!m) return s ? String(s) : "";
  const months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
  return `${Number(m[3])} ${months[Number(m[2]) - 1]} · ${m[4]}:${m[5]}`;
}

function storyLine(data, kind) {
  if (kind === "charge") {
    const power = (data.curve || []).map((p) => Number(p.power)).filter(Number.isFinite);
    const soc = (data.curve || []).map((p) => p.soc);
    if (power.length >= 2 && Math.max(...power) - Math.min(...power) > 1) return { values: power, label: "Charger power" };
    const finite = soc.map(Number).filter(Number.isFinite);
    if (finite.length >= 2 && Math.max(...finite) - Math.min(...finite) >= 1) return { values: soc, label: "State of charge" };
    return null;
  }
  const soc = data.soc || [];
  const finite = soc.map(Number).filter(Number.isFinite);
  if (finite.length >= 2 && Math.max(...finite) - Math.min(...finite) >= 1) return { values: soc, label: "State of charge" };
  const elev = data.elevation || [];
  if (elev.map(Number).filter(Number.isFinite).length >= 2) {
    return { values: elev, label: data.lengthUnit === "mi" ? "Elevation, ft" : "Elevation, m" };
  }
  if (finite.length >= 2) return { values: soc, label: "State of charge" };
  return null;
}

function sparkSvg(spec) {
  if (!spec) return "";
  const nums = spec.values.map(Number).filter(Number.isFinite);
  if (nums.length < 2) return "";
  const w = 640;
  const h = 64;
  const pad = 6;
  const min = Math.min(...nums);
  const max = Math.max(...nums);
  const span = max - min || 1;
  const pts = nums
    .map((v, i) => {
      const x = pad + (i / (nums.length - 1)) * (w - pad * 2);
      const y = pad + (1 - (v - min) / span) * (h - pad * 2);
      return `${x.toFixed(1)},${y.toFixed(1)}`;
    })
    .join(" ");
  return `<div class="story-spark-wrap"><svg viewBox="0 0 ${w} ${h}" class="story-spark" preserveAspectRatio="none" aria-hidden="true"><polyline points="${pts}" /></svg><span>${escapeHtml(spec.label)}</span></div>`;
}

function mountPath(el, path) {
  const pts = (path || []).filter((p) => Array.isArray(p) && Number.isFinite(Number(p[0])) && Number.isFinite(Number(p[1])));
  if (!pts.length || typeof L === "undefined") {
    el.remove();
    return;
  }
  const map = L.map(el, { zoomControl: false, attributionControl: false });
  osmTiles().addTo(map);
  let line = null;
  if (pts.length === 1) {
    L.circleMarker(pts[0], { radius: 7, color: "#e85d04", fillColor: "#e85d04", fillOpacity: 0.9, weight: 1 }).addTo(map);
  } else {
    L.polyline(pts, { color: "#111217", weight: 7, opacity: 0.9 }).addTo(map);
    line = L.polyline(pts, { color: "#e85d04", weight: 3, opacity: 1 }).addTo(map);
    L.circleMarker(pts[0], { radius: 4, color: "#d8dde8", fillColor: "#d8dde8", fillOpacity: 1, weight: 0 }).addTo(map);
    L.circleMarker(pts[pts.length - 1], { radius: 6, color: "#e85d04", fillColor: "#e85d04", fillOpacity: 1, weight: 0 }).addTo(map);
  }
  const fit = () => {
    if (line) map.fitBounds(line.getBounds(), { padding: [18, 18] });
    else map.setView(pts[0], 14);
  };
  el._map = map;
  fit();
  requestAnimationFrame(() => {
    map.invalidateSize();
    fit();
  });
}

function paintStory(card, kind, data) {
  const unit = data.lengthUnit || "km";
  const when = storyWhen(data.start);
  let title;
  const bits = [when];
  if (kind === "drive") {
    const from = data.from || "Start";
    const to = data.to || "End";
    title = from === to ? from : `${from} → ${to}`;
    if (data.distance != null) bits.push(`${n1(data.distance)} ${unit}`);
    if (data.durationMin != null) {
      const dur = hoursLabel(null, data.durationMin);
      if (dur) bits.push(dur);
    }
    if (data.socStart != null && data.socEnd != null) bits.push(`${data.socStart}% → ${data.socEnd}%`);
    if (data.consumption != null) bits.push(`${n1(data.consumption)} Wh/${unit}`);
    if (data.open) bits.push("still driving");
  } else {
    title = data.place || "Charge";
    if (data.energyAddedKwh != null) bits.push(`${n1(data.energyAddedKwh)} kWh`);
    if (data.durationMin != null) {
      const dur = hoursLabel(null, data.durationMin);
      if (dur) bits.push(dur);
    }
    if (data.socStart != null && data.socEnd != null) bits.push(`${data.socStart}% → ${data.socEnd}%`);
    const powers = (data.curve || []).map((p) => Number(p.power)).filter(Number.isFinite);
    const vary = powers.length >= 2 && Math.max(...powers) - Math.min(...powers) > 1;
    if (data.powerMax != null) bits.push(vary ? `up to ${n1(data.powerMax)} kW` : `${n1(data.powerMax)} kW`);
    if (data.cost != null && Number(data.cost) > 0) bits.push(`cost ${Number(data.cost).toFixed(2)}`);
    if (data.open) bits.push("still charging");
  }
  card.innerHTML = `<div class="story-head">
      <div><h3 class="story-title">${escapeHtml(title)}</h3><p class="story-sub">${escapeHtml(bits.filter(Boolean).join(" · "))}</p></div>
      <button type="button" class="story-close" aria-label="Close">Close</button>
    </div>
    <div class="story-map"></div>
    ${sparkSvg(storyLine(data, kind))}`;
  const mapEl = card.querySelector(".story-map");
  if (mapEl) mountPath(mapEl, data.path);
}

function dismissStory(story) {
  if (!story) return;
  storyGen += 1;
  const map = story.querySelector(".story-map");
  if (map?._map) {
    map._map.remove();
    map._map = null;
  }
  story.remove();
}

async function openStory(panel, id) {
  const kind = panel.dataset.open;
  const gen = ++storyGen;
  const card = document.createElement("article");
  card.className = "story";
  card.dataset.storyId = String(id);
  card.dataset.storyKind = kind;
  card.innerHTML = `<p class="story-wait">Opening…</p>`;
  panel.before(card);
  card.scrollIntoView({ block: "nearest", behavior: "smooth" });
  try {
    const car = vars().car_id;
    const url = kind === "drive" ? `/api/cars/${car}/drives/${encodeURIComponent(id)}` : `/api/cars/${car}/charges/${encodeURIComponent(id)}`;
    const data = await api(url);
    if (gen !== storyGen || !card.isConnected) return;
    paintStory(card, kind, data);
  } catch (e) {
    if (gen !== storyGen || !card.isConnected) return;
    card.innerHTML = `<p class="err">${escapeHtml(e.message)}</p>`;
  }
}

function onBoardActivate(ev) {
  const board = $("board");
  if (!board) return;
  const close = ev.type === "click" ? ev.target.closest?.(".story-close") : null;
  if (close && board.contains(close)) {
    const story = close.closest(".story");
    story?.parentElement?.querySelectorAll("tr.is-open").forEach((tr) => tr.classList.remove("is-open"));
    dismissStory(story);
    return;
  }
  const tr = ev.target.closest?.("tr.row-open");
  if (!tr || !board.contains(tr)) return;
  if (ev.type === "keydown" && ev.key !== "Enter" && ev.key !== " ") return;
  if (ev.type === "keydown") ev.preventDefault();
  const panel = tr.closest(".panel");
  const id = tr.dataset.openId;
  if (!panel || !id) return;
  const same = tr.classList.contains("is-open");
  board.querySelectorAll("tr.is-open").forEach((row) => row.classList.remove("is-open"));
  board.querySelectorAll(".story").forEach((s) => dismissStory(s));
  if (same) return;
  tr.classList.add("is-open");
  openStory(panel, id);
}

async function loadGrouped(id) {
  const page = META.find((p) => p.id === id) || META[0];
  const gen = ++dashGen;
  dashAbort?.abort();
  dashAbort = new AbortController();
  const { signal } = dashAbort;
  clearLive();
  currentMeta = page.id;
  renderNav();
  $("title").textContent = page.title;
  const board = $("board");
  dropMaps(board);
  board.classList.add("grouped");
  board.style.height = "auto";
  board.innerHTML = `<p class="meta-lead">${escapeHtml(page.lead)}</p>`;
  const ctl = { gen, signal };
  try {
    let liveHost = null;
    if (page.live) {
      liveHost = document.createElement("div");
      board.appendChild(liveHost);
      fillLive(liveHost, ctl);
    }
    const paths = [...new Set(page.sections.flatMap((s) => s.panels.map((p) => p[0])))];
    const dashes = {};
    await Promise.all(
      paths.map(async (path) => {
        dashes[path] = await dashboardByPath(path, signal);
      })
    );
    if (gen !== dashGen) return;
    const v = vars();
    for (const section of page.sections) {
      if (section.when === "trip") {
        const q = new URLSearchParams(location.search);
        if (!q.has("drive_id") && !q.has("charging_process_id")) continue;
      }
      const sec = document.createElement("section");
      sec.className = "meta-section";
      sec.innerHTML = `<h2>${escapeHtml(section.title)}</h2><div class="meta-grid"></div>`;
      const grid = sec.querySelector(".meta-grid");
      let any = false;
      for (const [path, id] of section.panels) {
        const dash = dashes[path];
        const panel = flattenPanels(dash?.panels).find((p) => p.id === id);
        if (!panel || panel.type === "row" || panel.type === "dashlist") continue;
        any = true;
        const el = document.createElement("div");
        const g = panel.gridPos || { w: 12, h: 8 };
        el.className =
          "panel flow" +
          (g.h <= 3 ? " compact" : "") +
          (panel.type === "stat" || panel.type === "gauge" ? " panel-kpi" : "");
        el.style.gridColumn = `span ${flowSpan(panel)}`;
        if ((path === "drives.json" && (id === 2 || id === 9)) || (path === "charges.json" && (id === 6 || id === 17))) {
          el.dataset.open = path.startsWith("drives") ? "drive" : "charge";
        }
        const bodyH = flowBodyHeight(panel);
        el.innerHTML = `<h3>${escapeHtml(interpTitle(panel.title || "", v, panel))}</h3><div class="body"></div>`;
        el.querySelector(".body").style.height = bodyH + "px";
        grid.appendChild(el);
        fillPanel(el.querySelector(".body"), panel, v, ctl, dash);
      }
      if (any) board.appendChild(sec);
    }
  } catch (e) {
    if (isAbort(e) || gen !== dashGen) return;
    board.insertAdjacentHTML("beforeend", `<div class="err">${escapeHtml(e.message)}</div>`);
  }
}

function syncHashToLayout() {
  const hash = location.hash.replace(/^#/, "");
  if (uiLayout === "grouped") {
    currentMeta = META_IDS.has(hash) ? hash : "vehicle";
    if (hash !== currentMeta) history.replaceState(null, "", "#" + currentMeta);
  } else if (hash.endsWith(".json")) {
    currentPath = hash;
  } else if (META_IDS.has(hash)) {
    history.replaceState(null, "", "#" + (currentPath || "overview.json"));
  }
}

async function chooseLayout(next) {
  const layout = next === "grouped" ? "grouped" : "classic";
  await api("/api/account/layout", {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ uiLayout: layout }),
  });
  uiLayout = layout;
  const target = layout === "grouped" ? currentMeta || "vehicle" : currentPath || "overview.json";
  if (location.hash.replace(/^#/, "") === target) await reloadView();
  else location.hash = target;
}

async function boot() {
  const st = await tmAuth.status();
  if (!(await tmAuth.route(st))) return;
  uiLayout = st.uiLayout === "grouped" ? "grouped" : "classic";
  const layoutSel = $("ui-layout");
  if (layoutSel) layoutSel.value = uiLayout;
  applyRange("30d");
  settings = await api("/api/settings");
  cars = await api("/api/cars");
  dashboards = await api("/api/dashboards");
  const carSel = $("car");
  carSel.replaceChildren();
  for (const c of cars) {
    const opt = document.createElement("option");
    opt.value = String(c.id);
    opt.textContent = c.name || "car " + c.id;
    carSel.appendChild(opt);
  }
  syncHashToLayout();
  renderNav();
  document.querySelectorAll(".presets button").forEach((b) =>
    b.addEventListener("click", () => {
      applyRange(b.dataset.range);
      if (window.matchMedia("(max-width: 800px)").matches) setRangeOpen(false);
      reloadView();
    })
  );
  $("range-toggle")?.addEventListener("click", () => {
    setRangeOpen(!$("range").classList.contains("open"));
  });
  $("car").addEventListener("change", () => reloadView());
  const noteCustomRange = () => {
    currentRange = "";
    paintRange();
    reloadView();
  };
  $("from").addEventListener("change", noteCustomRange);
  $("to").addEventListener("change", noteCustomRange);
  if (layoutSel) {
    layoutSel.addEventListener("change", () => {
      chooseLayout(layoutSel.value).catch((e) => {
        layoutSel.value = uiLayout;
        $("invite-url").textContent = e.message || String(e);
      });
    });
  }
  const board = $("board");
  if (board && !board.dataset.storyBound) {
    board.dataset.storyBound = "1";
    board.addEventListener("click", onBoardActivate);
    board.addEventListener("keydown", onBoardActivate);
  }
  window.addEventListener("hashchange", () => {
    const hash = location.hash.replace(/^#/, "");
    if (uiLayout === "grouped") {
      if (!META_IDS.has(hash)) {
        location.hash = currentMeta || "vehicle";
        return;
      }
      currentMeta = hash;
      loadGrouped(currentMeta);
      return;
    }
    if (META_IDS.has(hash)) {
      location.hash = currentPath || "overview.json";
      return;
    }
    currentPath = hash || "overview.json";
    loadDashboard(currentPath);
  });
  $("nav-list").addEventListener("click", (e) => {
    const a = e.target.closest("a[data-path]");
    if (!a) return;
    e.preventDefault();
    location.hash = a.dataset.path;
  });
  await reloadView();
}

boot().catch((e) => {
  $("board").innerHTML = `<div class="err">${escapeHtml(e.message)}</div>`;
});
