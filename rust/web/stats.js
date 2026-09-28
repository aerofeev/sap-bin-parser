// sap-bin usage dashboard, for the operator of the service.
//
// The page holds no numbers of its own: it asks for the statistics token,
// keeps it for this browser tab only (sessionStorage), and reads api/stats
// with it. Every URL is relative, so it works under a path prefix too.
"use strict";

(() => {
  const KEY = "sap-bin-stats-token";
  const DAYS = 30;
  const $ = (id) => document.getElementById(id);
  const number = new Intl.NumberFormat();
  const compact = new Intl.NumberFormat(undefined, { notation: "compact", maximumFractionDigits: 1 });
  const percent = new Intl.NumberFormat(undefined, { style: "percent", maximumFractionDigits: 1 });
  const SVG = "http://www.w3.org/2000/svg";

  let data = null;
  let series = "records";

  function el(tag, attrs = {}, ...children) {
    const node = document.createElement(tag);
    for (const [key, value] of Object.entries(attrs)) {
      if (key === "class") node.className = value;
      else node.setAttribute(key, value);
    }
    node.append(...children);
    return node;
  }

  function svg(tag, attrs = {}, text) {
    const node = document.createElementNS(SVG, tag);
    for (const [key, value] of Object.entries(attrs)) node.setAttribute(key, value);
    if (text !== undefined) node.textContent = text;
    return node;
  }

  function bytes(n) {
    if (n < 1000) return `${n} B`;
    const units = ["KB", "MB", "GB", "TB", "PB"];
    let value = n;
    let unit = -1;
    do {
      value /= 1000;
      unit += 1;
    } while (value >= 1000 && unit < units.length - 1);
    return `${value.toFixed(value < 10 ? 1 : 0)} ${units[unit]}`;
  }

  function duration(seconds) {
    const d = Math.floor(seconds / 86400);
    const h = Math.floor((seconds % 86400) / 3600);
    const m = Math.floor((seconds % 3600) / 60);
    if (d) return `${d}d ${h}h`;
    if (h) return `${h}h ${m}m`;
    if (m) return `${m}m`;
    return `${seconds.toFixed(seconds < 10 ? 1 : 0)}s`;
  }

  function token() {
    try {
      return sessionStorage.getItem(KEY) || "";
    } catch {
      return "";
    }
  }

  function remember(value) {
    try {
      if (value) sessionStorage.setItem(KEY, value);
      else sessionStorage.removeItem(KEY);
    } catch {
      // Private windows may refuse storage; the token then lasts one load.
    }
  }

  function signedIn(on) {
    $("sign-in").hidden = on;
    $("dashboard").hidden = !on;
    $("refresh").hidden = !on;
    $("sign-out").hidden = !on;
    if (!on) $("token").focus();
  }

  async function load(given) {
    const value = given ?? token();
    if (!value) return signedIn(false);
    let response;
    try {
      response = await fetch("api/stats", { headers: { Authorization: `Bearer ${value}` }, cache: "no-store" });
    } catch {
      return showError("The service could not be reached.");
    }
    if (response.status === 401) {
      remember("");
      signedIn(false);
      if (given !== undefined) showError("That token is not right.");
      return;
    }
    if (!response.ok) return showError(`The service answered ${response.status}.`);
    remember(value);
    data = await response.json();
    signedIn(true);
    render();
  }

  function showError(message) {
    signedIn(false);
    $("sign-in-error").textContent = message;
    $("sign-in-error").hidden = false;
  }

  // The last DAYS days, oldest first, with zeros for quiet days.
  function lastDays() {
    const end = new Date(`${data.today}T00:00:00Z`);
    const days = [];
    for (let i = DAYS - 1; i >= 0; i -= 1) {
      const day = new Date(end.getTime() - i * 86400000).toISOString().slice(0, 10);
      days.push({ day, ...(data.days[day] || {}) });
    }
    return days;
  }

  function render() {
    const t = data.totals;
    const failed = t.failed_input + t.failed_server;
    const attempted = t.conversions + failed;
    const days = lastDays();
    const recent = days.reduce((sum, d) => sum + (d.records || 0), 0);

    $("version").textContent = data.service_version;
    $("since").textContent = `Counting since ${data.since}${data.saved ? "" : " · kept in memory, so it resets when the service restarts"}`;
    $("running").textContent = `${data.running} converting now`;
    $("updated").textContent = `Updated ${new Date().toLocaleTimeString()}`;

    $("kpi-records").textContent = number.format(t.records);
    $("kpi-records-note").textContent = `${compact.format(recent)} in the last ${DAYS} days`;
    $("kpi-conversions").textContent = number.format(t.conversions);
    $("kpi-conversions-note").textContent = `${number.format(t.inspections)} inspected · ${number.format(t.samples)} samples`;
    $("kpi-bytes").textContent = bytes(t.bytes_in);
    $("kpi-bytes-note").textContent = `${bytes(t.bytes_out)} written out`;
    $("kpi-success").textContent = attempted ? percent.format(t.conversions / attempted) : "–";
    $("kpi-success-note").textContent =
      `${number.format(t.failed_input)} input errors · ${number.format(t.failed_server)} server · ${number.format(t.busy)} busy · ${number.format(t.cancelled)} cancelled`;

    drawChart(days);
    shares($("formats"), data.formats, { csv: "CSV", excel: "CSV for Excel", tsv: "TSV", parquet: "Parquet", jsonl: "JSON Lines" });
    shares($("inputs"), data.inputs, { archive: "One file (zip or .BIN)", files: "A folder or several files" });
    shares($("clients"), data.clients, { browser: "The page, in the browser", page: "The page, on the server", api: "API (curl, scripts)" });
    tables();
    service(t);
  }

  function drawChart(days) {
    const chart = $("chart");
    // Drawn at the chart's real size, so text is never stretched.
    const width = Math.max(320, Math.round(chart.getBoundingClientRect().width));
    const height = 192;
    const top = 8;
    const left = 44;
    const bottom = 22;
    const plot = height - top - bottom;
    const values = days.map((d) =>
      series === "records" ? d.records || 0 : series === "conversions" ? d.conversions || 0 : d.page_views || 0,
    );
    const max = Math.max(1, ...values);
    const slot = (width - left) / days.length;
    chart.setAttribute("viewBox", `0 0 ${width} ${height}`);
    chart.replaceChildren();
    for (const fraction of [0, 0.5, 1]) {
      const y = Math.round(top + plot * (1 - fraction)) + 0.5;
      chart.append(svg("line", { class: "gridline", x1: left, x2: width, y1: y, y2: y }));
      chart.append(svg("text", { class: "axis", x: left - 6, y: y + 3, "text-anchor": "end" }, compact.format(max * fraction)));
    }
    days.forEach((d, i) => {
      const h = (plot * values[i]) / max;
      const bar = svg("rect", {
        class: "bar-rect",
        x: left + i * slot + slot * 0.18,
        y: height - bottom - h,
        width: slot * 0.64,
        height: Math.max(h, values[i] ? 1 : 0),
        rx: 2,
      });
      bar.append(svg("title", {}, `${d.day}: ${number.format(values[i])}`));
      chart.append(bar);
      const every = width < 560 ? 10 : 5;
      if ((days.length - 1 - i) % every === 0) {
        chart.append(
          svg("text", { class: "axis", x: left + i * slot + slot / 2, y: height - 6, "text-anchor": "middle" }, d.day.slice(5)),
        );
      }
    });
    for (const [id, name] of [["chart-records", "records"], ["chart-conversions", "conversions"], ["chart-views", "views"]]) {
      $(id).setAttribute("aria-selected", String(series === name));
    }
  }

  function shares(target, map, names) {
    const entries = Object.entries(map).sort((a, b) => b[1].records - a[1].records);
    const total = entries.reduce((sum, [, c]) => sum + c.records, 0) || 1;
    if (!entries.length) {
      target.replaceChildren(el("p", { class: "muted m-0 text-sm" }, "Nothing yet."));
      return;
    }
    target.replaceChildren(
      ...entries.map(([key, c]) => {
        const fill = el("div", { class: "share-fill" });
        fill.style.width = `${(100 * c.records) / total}%`;
        return el(
          "div",
          { class: "share" },
          el("span", { class: "w-40 shrink-0 truncate" }, names[key] || key),
          el("div", { class: "share-bar" }, fill),
          el("span", { class: "w-28 shrink-0 text-right tabular-nums text-muted-foreground" }, `${compact.format(c.records)} · ${number.format(c.conversions)}×`),
        );
      }),
    );
  }

  function tables() {
    const entries = Object.entries(data.tables).sort((a, b) => b[1].records - a[1].records);
    const total = entries.reduce((sum, [, c]) => sum + c.records, 0) || 1;
    const rows = entries.slice(0, 50).map(([name, c]) =>
      el(
        "tr",
        {},
        el("td", { class: "mono" }, name),
        el("td", { class: "num" }, number.format(c.conversions)),
        el("td", { class: "num" }, number.format(c.records)),
        el("td", { class: "num" }, percent.format(c.records / total)),
      ),
    );
    if (!rows.length) rows.push(el("tr", {}, el("td", { class: "null", colspan: "4" }, "No conversions yet.")));
    $("tables").replaceChildren(...rows);
  }

  function service(t) {
    const tiles = [
      [
        "Peak throughput",
        data.peak_records_per_second ? `${compact.format(data.peak_records_per_second)} rec/s` : "–",
        "fastest conversion of a second or more",
      ],
      ["Largest conversion", `${compact.format(data.largest.records)} records`, data.largest.day ? `${bytes(data.largest.bytes_in)} on ${data.largest.day}` : "–"],
      ["Time converting", duration(t.seconds), `${number.format(t.shards)} shards`],
      ["Page views", number.format(t.page_views), "loads of the page"],
      ["Uptime", duration(data.uptime_seconds), `version ${data.service_version}`],
    ];
    $("service").replaceChildren(
      ...tiles.map(([label, value, note]) =>
        el("div", { class: "stat" }, el("div", { class: "stat-label" }, label), el("div", { class: "stat-value" }, value), el("div", { class: "stat-note" }, note)),
      ),
    );
  }

  $("sign-in-form").addEventListener("submit", (event) => {
    event.preventDefault();
    $("sign-in-error").hidden = true;
    load($("token").value.trim());
  });
  $("refresh").addEventListener("click", () => load());
  $("sign-out").addEventListener("click", () => {
    remember("");
    data = null;
    $("token").value = "";
    signedIn(false);
  });
  for (const [id, name] of [["chart-records", "records"], ["chart-conversions", "conversions"], ["chart-views", "views"]]) {
    $(id).addEventListener("click", () => {
      series = name;
      if (data) drawChart(lastDays());
    });
  }
  let resizing = 0;
  window.addEventListener("resize", () => {
    clearTimeout(resizing);
    resizing = setTimeout(() => data && drawChart(lastDays()), 100);
  });
  setInterval(() => {
    if (data && !document.hidden) load();
  }, 60000);
  load();
})();
