const PALETTE = [
  "#c45c26", "#2f6b4e", "#3d6b8c", "#8a5a3b", "#6d4c7d", "#a6842f",
  "#4e6b62", "#8f4e4e", "#5d6b38", "#3e5f6e", "#7a5b45", "#6a4a55",
  "#3f6f6a", "#9a5b3c",
];

const state = {
  preset: "7d",
  device: "",
  filterApp: "",
  titleScope: "day",
  selectedDate: null,
  data: null,
  status: null,
};

const $ = (id) => document.getElementById(id);
const tip = $("tip");

boot();

function boot() {
  const params = new URLSearchParams(location.search);
  const incoming = params.get("token");
  if (incoming) {
    localStorage.setItem("awm-token", incoming);
    params.delete("token");
    const next = params.toString();
    history.replaceState({}, "", location.pathname + (next ? "?" + next : ""));
  }
  const theme = document.documentElement.getAttribute("data-theme") || "light";
  $("theme").textContent = theme === "dark" ? "Light" : "Dark";
  syncLock();
  const range = rangeFor("7d");
  setDateInputs(range.start, range.end);
  $("presets").addEventListener("click", onPreset);
  $("from").addEventListener("change", onCustomRange);
  $("to").addEventListener("change", onCustomRange);
  $("theme").addEventListener("click", toggleTheme);
  $("lock").addEventListener("click", signOut);
  $("gate-form").addEventListener("submit", onUnlock);
  $("title-scope").addEventListener("click", onScope);
  document.addEventListener("keydown", onKey);
  load();
  loadStatus();
  fetch("/api/v1/health")
    .then((res) => res.json())
    .then((body) => {
      $("foot-meta").textContent = "awm " + body.version;
    })
    .catch(() => {});
}

function syncLock() {
  $("lock").hidden = !localStorage.getItem("awm-token");
}

function toggleTheme() {
  const next = document.documentElement.getAttribute("data-theme") === "dark" ? "light" : "dark";
  document.documentElement.setAttribute("data-theme", next);
  localStorage.setItem("awm-theme", next);
  $("theme").textContent = next === "dark" ? "Light" : "Dark";
}

function signOut() {
  localStorage.removeItem("awm-token");
  syncLock();
  load();
}

function onUnlock(event) {
  event.preventDefault();
  const token = $("token-input").value.trim();
  if (!token) return;
  localStorage.setItem("awm-token", token);
  syncLock();
  $("gate-error").hidden = true;
  load();
}

function onPreset(event) {
  const button = event.target.closest("button[data-preset]");
  if (!button) return;
  state.preset = button.dataset.preset;
  document.querySelectorAll("#presets button").forEach((el) => {
    el.classList.toggle("on", el === button);
  });
  const range = rangeFor(state.preset);
  setDateInputs(range.start, range.end);
  load();
}

function onCustomRange() {
  state.preset = "custom";
  document.querySelectorAll("#presets button").forEach((el) => el.classList.remove("on"));
  load();
}

function onScope(event) {
  const button = event.target.closest("button[data-scope]");
  if (!button) return;
  state.titleScope = button.dataset.scope;
  document.querySelectorAll("#title-scope button").forEach((el) => {
    el.classList.toggle("on", el === button);
  });
  renderTitles();
  renderSites();
}

function onKey(event) {
  if (event.target.matches("input, textarea")) return;
  if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") return;
  if (!state.data) return;
  const days = state.data.days;
  const index = days.findIndex((day) => day.date === state.selectedDate);
  if (index < 0) return;
  const next = event.key === "ArrowLeft" ? index - 1 : index + 1;
  if (next < 0 || next >= days.length) return;
  state.selectedDate = days[next].date;
  renderDays();
  renderTimeline();
  renderTitles();
  renderSites();
}

let inflight = 0;

async function load() {
  const id = ++inflight;
  $("app").classList.add("is-loading");
  hideError();
  try {
    const range = currentRange();
    const tz = Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC";
    const query = new URLSearchParams({
      start: range.start.toISOString(),
      end: range.end.toISOString(),
      tz,
    });
    if (state.device) query.set("device", state.device);
    const data = await api("/api/v1/summary?" + query.toString());
    if (id !== inflight) return;
    state.data = data;
    if (!data.days.some((day) => day.date === state.selectedDate)) {
      state.selectedDate = data.days.length ? data.days[data.days.length - 1].date : null;
    }
    $("gate").classList.add("hidden");
    render();
  } catch (err) {
    if (id !== inflight) return;
    if (err.status === 401) {
      $("gate").classList.remove("hidden");
      const hadToken = Boolean(localStorage.getItem("awm-token"));
      $("gate-error").hidden = !hadToken;
      if (hadToken) $("gate-error").textContent = "That token was rejected.";
      return;
    }
    showError(err.message || "Could not load activity.");
  } finally {
    if (id === inflight) $("app").classList.remove("is-loading");
  }
}

async function loadStatus() {
  try {
    state.status = await api("/api/v1/status");
    renderStorage();
  } catch {
    // Summary's 401 path owns the login gate.
  }
}

async function api(path) {
  const headers = {};
  const token = localStorage.getItem("awm-token");
  if (token) headers.Authorization = "Bearer " + token;
  const res = await fetch(path, { headers });
  if (res.status === 401) {
    const error = new Error("unauthorized");
    error.status = 401;
    throw error;
  }
  if (!res.ok) {
    let message = res.statusText;
    try {
      const body = await res.json();
      if (body.error) message = body.error;
    } catch {
      /* ignore */
    }
    const error = new Error(message);
    error.status = res.status;
    throw error;
  }
  return res.json();
}

function render() {
  const data = state.data;
  if (!data) return;
  renderMachines();
  renderStats();
  renderDays();
  renderApps();
  renderCategories();
  renderTimeline();
  renderTitles();
  renderSites();
  if (state.status) renderStorage();
  $("range-label").textContent = data.databases.length
    ? data.databases.length + " period " + (data.databases.length === 1 ? "database" : "databases")
    : "no period files in this range";
}

function renderMachines() {
  const root = $("machines");
  root.replaceChildren();
  const data = state.data;
  const all = document.createElement("button");
  all.type = "button";
  all.className = "chip" + (state.device ? "" : " on");
  all.textContent = "All machines";
  all.addEventListener("click", () => selectDevice(""));
  root.appendChild(all);
  for (const machine of data.machines) {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "chip" + (state.device === machine.device_id ? " on" : "");
    const dot = document.createElement("i");
    dot.className = "dot " + freshness(machine.last_seen_ms);
    const name = document.createElement("span");
    name.textContent = machine.hostname || machine.device_id;
    const time = document.createElement("small");
    time.textContent = formatDuration(machine.active_secs);
    button.append(dot, name, time);
    button.addEventListener("click", () => selectDevice(machine.device_id));
    root.appendChild(button);
  }
}

function selectDevice(id) {
  state.device = id;
  state.filterApp = "";
  load();
}

function renderStats() {
  const data = state.data;
  const machines = state.device ? 1 : data.machines.length;
  const root = $("stats");
  root.replaceChildren();
  const cards = [
    ["Active", formatDuration(data.active_secs), machines > 1 ? "sum across machines" : "focused time"],
    ["Idle", formatDuration(data.afk_secs), "afk watcher"],
    ["Apps", String(data.apps.length), "in this range"],
    ["Last report", data.last_ingest_ms ? formatAgo(data.last_ingest_ms) : "never", formatCount(data.event_count) + " events read"],
  ];
  for (const [label, value, note] of cards) {
    const card = document.createElement("article");
    card.className = "stat";
    const k = document.createElement("span");
    k.textContent = label;
    const b = document.createElement("b");
    b.textContent = value;
    const em = document.createElement("em");
    em.textContent = note;
    card.append(k, b, em);
    root.appendChild(card);
  }
}

function renderDays() {
  const root = $("days");
  root.replaceChildren();
  const data = state.data;
  if (!data.days.length) {
    root.append(quiet("No days in this range."));
    return;
  }
  if (data.machines.length === 0 && data.event_count === 0) {
    const empty = document.createElement("p");
    empty.className = "empty";
    empty.textContent = "No machines have reported yet. Install the collector on a Mac running ActivityWatch. The first sync usually lands within a minute.";
    root.append(empty);
    return;
  }
  for (const day of data.days) {
    const row = document.createElement("div");
    row.className = "day" + (day.date === state.selectedDate ? " on" : "");
    const name = document.createElement("button");
    name.type = "button";
    name.className = "day-name";
    name.textContent = formatDay(day.date);
    name.addEventListener("click", () => selectDay(day.date));
    const track = document.createElement("div");
    track.className = "track";
    track.addEventListener("click", () => selectDay(day.date));
    paintSegments(track, day, day.segments, false);
    const time = document.createElement("span");
    time.className = "day-time";
    time.textContent = formatDuration(day.active_secs);
    row.append(name, track, time);
    root.append(row);
  }
}

function selectDay(date) {
  state.selectedDate = date;
  renderDays();
  renderTimeline();
  renderTitles();
  renderSites();
}

function renderApps() {
  const root = $("apps");
  root.replaceChildren();
  const apps = state.data.apps;
  $("app-filter").textContent = state.filterApp ? "showing " + state.filterApp : "";
  if (!apps.length) {
    root.append(quiet("No focused apps in this range."));
    return;
  }
  const max = apps[0].secs || 1;
  for (const app of apps) {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "row" + (state.filterApp === app.app ? " on" : "");
    const swatch = document.createElement("i");
    swatch.className = "swatch";
    swatch.style.background = colorFor(app.app);
    const name = document.createElement("span");
    name.className = "name";
    name.textContent = app.app;
    if (app.category) {
      const meta = document.createElement("span");
      meta.className = "meta";
      meta.textContent = " · " + app.category;
      name.append(meta);
    }
    const time = document.createElement("span");
    time.className = "time";
    time.textContent = formatDuration(app.secs);
    const bar = document.createElement("span");
    bar.className = "bar";
    const fill = document.createElement("span");
    fill.style.width = Math.max(2, (app.secs / max) * 100) + "%";
    fill.style.background = colorFor(app.app);
    bar.append(fill);
    button.append(swatch, name, time, bar);
    button.addEventListener("click", () => {
      state.filterApp = state.filterApp === app.app ? "" : app.app;
      renderApps();
      renderTimeline();
      renderDays();
    });
    root.append(button);
  }
}

function renderCategories() {
  const root = $("categories");
  root.replaceChildren();
  const cats = state.data.categories;
  if (!cats.length) {
    root.append(quiet("No categories yet."));
    return;
  }
  const max = cats[0].secs || 1;
  for (const cat of cats) {
    const row = document.createElement("div");
    row.className = "row";
    const swatch = document.createElement("i");
    swatch.className = "swatch";
    swatch.style.background = cat.color;
    const name = document.createElement("span");
    name.className = "name";
    name.textContent = cat.name;
    const time = document.createElement("span");
    time.className = "time";
    time.textContent = formatDuration(cat.secs);
    const bar = document.createElement("span");
    bar.className = "bar";
    const fill = document.createElement("span");
    fill.style.width = Math.max(2, (cat.secs / max) * 100) + "%";
    fill.style.background = cat.color;
    bar.append(fill);
    row.append(swatch, name, time, bar);
    root.append(row);
  }
}

function renderTimeline() {
  const root = $("timeline");
  root.replaceChildren();
  const day = selectedDay();
  if (!day) {
    $("day-title").textContent = "Day";
    $("day-sub").textContent = "";
    root.append(quiet("Pick a day."));
    return;
  }
  $("day-title").textContent = formatDay(day.date);
  const laneNote = day.lanes.length > 1 ? day.lanes.length + " machines" : "one machine";
  $("day-sub").textContent = formatDuration(day.active_secs) + " active · " + formatDuration(day.afk_secs) + " idle · " + laneNote;
  root.append(hourScale());
  if (!day.lanes.length) {
    const track = document.createElement("div");
    track.className = "track timeline-track";
    markNow(track, day);
    root.append(track);
    root.append(quiet("Quiet day."));
    return;
  }
  for (const lane of day.lanes) {
    const wrap = document.createElement("div");
    wrap.className = "lane";
    if (day.lanes.length > 1) {
      const name = document.createElement("div");
      name.className = "lane-name";
      name.textContent = lane.hostname || lane.device_id;
      wrap.append(name);
    }
    const track = document.createElement("div");
    track.className = "track timeline-track";
    paintSegments(track, day, lane.segments, true);
    markNow(track, day);
    wrap.append(track);
    root.append(wrap);
  }
}

function renderTitles() {
  if (!state.data) return;
  const root = $("titles");
  root.replaceChildren();
  const rows = state.titleScope === "range" ? state.data.titles : (selectedDay()?.titles || []);
  if (!rows.length) {
    root.append(quiet("No window titles."));
    return;
  }
  const list = document.createElement("div");
  list.className = "list";
  for (const row of rows) {
    const item = document.createElement("div");
    item.className = "title-row";
    const text = document.createElement("div");
    const title = document.createElement("strong");
    title.textContent = row.title;
    const app = document.createElement("span");
    app.textContent = " " + row.app;
    text.append(title, app);
    const time = document.createElement("div");
    time.className = "time-col";
    time.textContent = formatDuration(row.secs);
    item.append(text, time);
    list.append(item);
  }
  root.append(list);
}

function renderSites() {
  if (!state.data) return;
  const root = $("sites");
  root.replaceChildren();
  const rows = state.titleScope === "range" ? state.data.sites : (selectedDay()?.sites || []);
  if (!rows.length) {
    root.append(quiet("No browser events in this view."));
    return;
  }
  const list = document.createElement("div");
  list.className = "list";
  for (const row of rows) {
    const item = document.createElement("div");
    item.className = "site-row";
    const name = document.createElement("strong");
    name.textContent = row.domain;
    const time = document.createElement("div");
    time.className = "time-col";
    time.textContent = formatDuration(row.secs);
    item.append(name, time);
    list.append(item);
  }
  root.append(list);
}

function renderStorage() {
  const report = state.status;
  const root = $("storage");
  if (!report) return;
  root.replaceChildren();
  $("storage-sub").textContent =
    "Each file holds " + report.period_days + " days. This range reads every file it touches in one query. " +
    formatCount(report.total_events) + " events · " + formatBytes(report.total_bytes) + " on disk.";
  if (!report.databases.length) {
    root.append(quiet("No period databases yet."));
    return;
  }
  const table = document.createElement("table");
  table.className = "storage-table";
  const head = document.createElement("tr");
  for (const label of ["File", "Starts", "Events", "Size"]) {
    const th = document.createElement("th");
    th.textContent = label;
    head.append(th);
  }
  const thead = document.createElement("thead");
  thead.append(head);
  const tbody = document.createElement("tbody");
  for (const db of report.databases) {
    const tr = document.createElement("tr");
    const queried = state.data && state.data.databases.includes(db.file);
    for (const value of [db.file + (queried ? "  · in view" : ""), db.start.slice(0, 10), formatCount(db.events), formatBytes(db.bytes)]) {
      const td = document.createElement("td");
      td.textContent = value;
      tr.append(td);
    }
    tbody.append(tr);
  }
  table.append(thead, tbody);
  root.append(table);
}

function paintSegments(track, day, segments, detailed) {
  const span = day.end_ms - day.start_ms;
  if (span <= 0) return;
  for (const seg of segments) {
    const el = document.createElement("i");
    const dim = state.filterApp && seg.kind === "app" && seg.app !== state.filterApp && seg.app !== "Active";
    el.className = "block " + (seg.kind === "afk" ? "afk" : "app") + (dim ? " dim" : "");
    const left = ((seg.start_ms - day.start_ms) / span) * 100;
    const width = ((seg.end_ms - seg.start_ms) / span) * 100;
    el.style.left = left + "%";
    el.style.width = Math.max(width, 0) + "%";
    if (seg.kind !== "afk") {
      el.style.background = seg.app === "Active" ? "var(--accent)" : colorFor(seg.app || "app");
    }
    if (detailed) {
      el.addEventListener("pointerenter", (event) => showTip(event, seg));
      el.addEventListener("pointermove", moveTip);
      el.addEventListener("pointerleave", hideTip);
    }
    track.append(el);
  }
}

function hourScale() {
  const scale = document.createElement("div");
  scale.className = "hours";
  for (let hour = 0; hour < 24; hour += 3) {
    const label = document.createElement("span");
    label.style.left = (hour / 24) * 100 + "%";
    label.textContent = hour === 0 ? "12a" : hour < 12 ? hour + "a" : hour === 12 ? "12p" : (hour - 12) + "p";
    scale.append(label);
  }
  return scale;
}

function markNow(track, day) {
  const now = Date.now();
  if (now < day.start_ms || now >= day.end_ms) return;
  const mark = document.createElement("i");
  mark.className = "now";
  mark.style.left = ((now - day.start_ms) / (day.end_ms - day.start_ms)) * 100 + "%";
  track.append(mark);
}

function showTip(event, seg) {
  tip.replaceChildren();
  const title = document.createElement("b");
  title.textContent = seg.kind === "afk" ? "Idle" : (seg.app || "Active");
  const detail = document.createElement("span");
  const bits = [formatClock(seg.start_ms) + "–" + formatClock(seg.end_ms), formatDuration((seg.end_ms - seg.start_ms) / 1000)];
  if (seg.title && seg.kind !== "afk") bits.unshift(seg.title);
  detail.textContent = bits.join(" · ");
  tip.append(title, detail);
  tip.classList.remove("hidden");
  moveTip(event);
}

function moveTip(event) {
  const pad = 14;
  let x = event.clientX + pad;
  let y = event.clientY + pad;
  const rect = tip.getBoundingClientRect();
  if (x + rect.width > window.innerWidth - 8) x = event.clientX - rect.width - pad;
  if (y + rect.height > window.innerHeight - 8) y = event.clientY - rect.height - pad;
  tip.style.left = x + "px";
  tip.style.top = y + "px";
}

function hideTip() {
  tip.classList.add("hidden");
}

function selectedDay() {
  return state.data?.days.find((day) => day.date === state.selectedDate) || null;
}

function currentRange() {
  if (state.preset !== "custom") return rangeFor(state.preset);
  const start = parseYmd($("from").value) || startOfDay(new Date());
  const to = parseYmd($("to").value) || start;
  const end = new Date(to);
  end.setDate(end.getDate() + 1);
  if (end <= start) {
    const fixed = new Date(start);
    fixed.setDate(fixed.getDate() + 1);
    return { start, end: fixed };
  }
  return { start, end };
}

function rangeFor(preset) {
  const today = startOfDay(new Date());
  const end = new Date(today);
  end.setDate(end.getDate() + 1);
  if (preset === "today") return { start: today, end };
  if (preset === "yesterday") {
    const start = new Date(today);
    start.setDate(start.getDate() - 1);
    return { start, end: today };
  }
  const days = preset === "30d" ? 29 : 6;
  const start = new Date(today);
  start.setDate(start.getDate() - days);
  return { start, end };
}

function setDateInputs(start, endExclusive) {
  $("from").value = ymd(start);
  const inclusive = new Date(endExclusive.getTime() - 1);
  $("to").value = ymd(inclusive);
}

function startOfDay(date) {
  const copy = new Date(date);
  copy.setHours(0, 0, 0, 0);
  return copy;
}

function parseYmd(value) {
  if (!value) return null;
  const [year, month, day] = value.split("-").map(Number);
  if (!year || !month || !day) return null;
  return new Date(year, month - 1, day);
}

function ymd(date) {
  const month = String(date.getMonth() + 1).padStart(2, "0");
  const day = String(date.getDate()).padStart(2, "0");
  return date.getFullYear() + "-" + month + "-" + day;
}

function formatDay(iso) {
  const date = parseYmd(iso);
  if (!date) return iso;
  return date.toLocaleDateString([], { weekday: "short", month: "short", day: "numeric" });
}

function formatClock(ms) {
  return new Date(ms).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
}

function formatDuration(secs) {
  secs = Math.max(0, Math.round(Number(secs) || 0));
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  if (h > 0 && m > 0) return h + "h " + m + "m";
  if (h > 0) return h + "h";
  if (m > 0) return m + "m";
  return secs + "s";
}

function formatAgo(ms) {
  const secs = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (secs < 45) return "just now";
  const mins = Math.round(secs / 60);
  if (mins < 60) return mins + "m ago";
  const hours = Math.round(mins / 60);
  if (hours < 48) return hours + "h ago";
  return Math.round(hours / 24) + "d ago";
}

function formatCount(n) {
  return new Intl.NumberFormat().format(n || 0);
}

function formatBytes(n) {
  n = Number(n) || 0;
  if (n < 1024) return n + " B";
  if (n < 1024 * 1024) return (n / 1024).toFixed(1) + " KB";
  return (n / 1024 / 1024).toFixed(1) + " MB";
}

function freshness(ms) {
  if (!ms) return "";
  const age = Date.now() - ms;
  if (age < 3 * 60 * 1000) return "fresh";
  if (age < 60 * 60 * 1000) return "warm";
  return "";
}

function colorFor(name) {
  let hash = 0;
  for (const char of name || "") hash = (hash * 33 + char.charCodeAt(0)) >>> 0;
  return PALETTE[hash % PALETTE.length];
}

function quiet(text) {
  const el = document.createElement("p");
  el.className = "quiet";
  el.textContent = text;
  return el;
}

function showError(message) {
  const el = $("error");
  el.hidden = false;
  el.classList.remove("hidden");
  el.textContent = message;
}

function hideError() {
  $("error").classList.add("hidden");
}
