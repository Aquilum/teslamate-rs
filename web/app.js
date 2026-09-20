const ROW_H = 54;
const COLS = 24;
let dashboards = [];
let currentPath = "overview.json";
let currentDash = null;
let settings = {};
let cars = [];

const $ = (id) => document.getElementById(id);

function toLocalInput(d) {
  const pad = (n) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

function applyRange(key) {
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
    interval: "1h",
    extras: Object.fromEntries(new URLSearchParams(location.search)),
  };
}

async function api(path, opts) {
  const r = await fetch(path, { credentials: "same-origin", ...opts });
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
  const folders = {};
  for (const d of dashboards) {
    (folders[d.folder] ||= []).push(d);
  }
  $("nav-list").innerHTML = Object.entries(folders)
    .map(
      ([folder, items]) =>
        `<h2>${folder}</h2>` +
        items
          .map(
            (d) =>
              `<a href="#${d.path}" data-path="${d.path}" class="${d.path === currentPath ? "active" : ""}">${d.title}</a>`
          )
          .join("")
    )
    .join("");
}

async function loadDashboard(path) {
  currentPath = path;
  renderNav();
  const dash = await api("/api/dashboards/" + path);
  currentDash = dash;
  $("title").textContent = dash.title || path;
  const panels = flattenPanels(dash.panels);
  const maxY = panels.reduce((m, p) => {
    const g = p.gridPos || { y: 0, h: 8 };
    return Math.max(m, g.y + g.h);
  }, 8);
  const board = $("board");
  board.style.height = maxY * ROW_H + 24 + "px";
  board.innerHTML = "";
  const v = vars();
  for (const panel of panels) {
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
    fillPanel(el.querySelector(".body"), panel, v);
  }
}

async function fillPanel(body, panel, v) {
  if (panel.type === "row" || panel.type === "text" || panel.type === "dashlist") {
    body.textContent = panel.options?.content || "";
    return;
  }
  const targets = [];
  for (const t of panel.targets || []) {
    let sql = t.rawSql;
    if (!sql && t.panelId != null && currentDash) {
      const src = flattenPanels(currentDash.panels).find((p) => p.id === t.panelId);
      sql = src?.targets?.find((x) => x.rawSql)?.rawSql;
    }
    if (sql) targets.push({ rawSql: sql });
  }
  if (!targets.length) {
    body.innerHTML = "";
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
        results.push(
          await api("/api/query", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify(q),
          })
        );
      } catch (e) {
        results.push({ ok: false, error: e.message || String(e) });
      }
    }
    const ok = results.filter((r) => r && r.ok !== false);
    if (!ok.length) {
      body.innerHTML = `<div class="err">${escapeHtml(results[0]?.error || "query failed")}</div>`;
      return;
    }
    draw(body, panel, results);
  } catch (e) {
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
  return String(s).replace(/[&<>]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;" }[c]));
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

function drawTable(body, panel, cols, rows) {
  const slice = sortRowsByDate(cols, rows).slice(0, 500);
  const show = cols.filter((c) => !hiddenCol(c, panel) && !emptyCol(slice, c));
  const headers = show.length ? show : cols.filter((c) => !hiddenCol(c, panel));
  const isVampire = headers.some((c) => /range_lost_per_hour|standby/.test(c));
  const stats = isVampire ? vampireStats(slice) : null;
  body.innerHTML =
    `<table><thead><tr>${headers.map((c) => `<th>${escapeHtml(headerLabel(panel, c))}</th>`).join("")}</tr></thead><tbody>` +
    slice
      .map((r) => {
        const sev = isVampire ? vampireRowClass(r, stats) : { cls: "", title: "" };
        const tr = ` class="${sev.cls}"` + (sev.title ? ` title="${escapeHtml(sev.title)}"` : "");
        return `<tr${tr}>${headers
          .map((c) => {
            const cell = formatCell(panel, c, r[c]);
            const st = cell.color && cell.color !== "transparent" ? ` style="color:${cell.color};font-weight:600"` : "";
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
    const color = it.mapped?.color ? ` style="color:${it.mapped.color}"` : "";
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
    body.innerHTML += `<div class="gauge-bar"><span style="width:${pct}%;background:${it.mapped.color || "var(--accent)"}"></span></div>`;
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
    const fill = m.color || "#8b93a7";
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
    .map(([name, color]) => `<span class="tl-swatch"><i style="background:${color}"></i>${escapeHtml(name)}</span>`)
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
      return `<div class="bg-row"><span class="bg-name">${escapeHtml(label)}</span><div class="bg-track"><span style="width:${pct}%;background:${color}"></span></div><span class="bg-val">${escapeHtml(n == null ? "" : formatNum(n))}</span></div>`;
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
    const color = sliceColor(panel, s.name, i);
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
          return `<div class="pie-leg"><i style="background:${sliceColor(panel, s.name, i)}"></i><b>${escapeHtml(s.name)}</b><span>${escapeHtml(formatPieValue(panel, s.v, valCol))}</span><span>${pct.toFixed(1)}%</span></div>`;
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

function drawGeomap(el, panel, cols, rows) {
  const map = L.map(el).setView([54, -2], 6);
  L.tileLayer("https://{s}.tile.openstreetmap.org/{z}/{x}/{y}.png", {
    attribution: "&copy; OSM",
    maxZoom: 19,
  }).addTo(map);
  const latKey = cols.find((c) => /lat/i.test(c));
  const lonKey = cols.find((c) => /lon|lng/i.test(c));
  const layerType = (panel.options?.layers || []).map((l) => l.type).find(Boolean) || "route";
  const pts = [];
  if (layerType === "markers") {
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
  } else {
    for (const r of rows) {
      const la = Number(r[latKey]);
      const lo = Number(r[lonKey]);
      if (Number.isFinite(la) && Number.isFinite(lo)) pts.push([la, lo]);
    }
    if (pts.length) L.polyline(pts, { color: "#e85d04", weight: 3 }).addTo(map);
  }
  if (pts.length) map.fitBounds(pts, { padding: [16, 16] });
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
    for (const r of f.rows || []) {
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

async function boot() {
  if (!(await tmAuth.route(await tmAuth.status()))) return;
  applyRange("30d");
  settings = await api("/api/settings");
  cars = await api("/api/cars");
  dashboards = await api("/api/dashboards");
  $("car").innerHTML = cars.map((c) => `<option value="${c.id}">${c.name || "car " + c.id}</option>`).join("");
  renderNav();
  const hash = location.hash.replace(/^#/, "");
  if (hash) currentPath = hash;
  document.querySelectorAll(".presets button").forEach((b) =>
    b.addEventListener("click", () => {
      applyRange(b.dataset.range);
      loadDashboard(currentPath);
    })
  );
  $("car").addEventListener("change", () => loadDashboard(currentPath));
  $("from").addEventListener("change", () => loadDashboard(currentPath));
  $("to").addEventListener("change", () => loadDashboard(currentPath));
  window.addEventListener("hashchange", () => {
    currentPath = location.hash.replace(/^#/, "") || "overview.json";
    loadDashboard(currentPath);
  });
  $("nav-list").addEventListener("click", (e) => {
    const a = e.target.closest("a[data-path]");
    if (!a) return;
    e.preventDefault();
    location.hash = a.dataset.path;
  });
  await loadDashboard(currentPath);
}

boot().catch((e) => {
  $("board").innerHTML = `<div class="err">${escapeHtml(e.message)}</div>`;
});
