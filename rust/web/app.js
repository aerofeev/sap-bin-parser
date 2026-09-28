// sap-bin web page. No framework and no third-party code: what you read
// here is exactly what runs. (Only the stylesheet is built, from Tailwind
// CSS in styles/app.css, and the result is committed as app.css.)
//
// Flow: choose an export (a .zip, an unzipped folder, or separate files) ->
// describe it from the first 4 MiB and last 256 KiB of its first file, for
// an instant preview -> optionally edit the schema -> convert.
//
// Where the browser allows it, all of that runs in the page itself: the
// engine, compiled to WebAssembly, in a worker, so the export never leaves
// the computer. Otherwise (and in the local app, where native threads are
// faster) the server does it: api/inspect, then a chunked job whose output
// the browser's own download manager streams to disk, with api/jobs/{id}
// for progress. Every URL is relative, so the page works at a domain root
// or under a path such as /sap-bin-parser/.
"use strict";

(() => {
  const HEAD_BYTES = 4 * 1024 * 1024;
  const TAIL_BYTES = 256 * 1024;
  const TYPES = ["C", "N", "D", "T", "P"];
  const TYPE_NAMES = { C: "C text", N: "N digits", D: "D date", T: "T time", P: "P packed" };
  const $ = (id) => document.getElementById(id);
  const number = new Intl.NumberFormat();

  const state = {
    config: { local: false, version: "" },
    files: [], // data files of the export, in shard order
    schema: null, // a DATA.0.TXT / DATA.0.zip, or an edited schema
    schemaEdited: false,
    exportName: null, // folder name, when a folder was chosen
    report: null,
    job: null,
    rows: [], // schema editor rows
    inBrowser: false, // convert with the engine in the page (see `engine`)
    inspected: null, // inspectOptions() of the latest inspection
  };
  const dataFile = () => state.files[0] || null;
  const multi = () => state.files.length > 1;

  // ---------- helpers ----------

  function el(tag, attrs = {}, ...children) {
    const node = document.createElement(tag);
    for (const [key, value] of Object.entries(attrs)) {
      if (value === undefined || value === null || value === false) continue;
      if (key === "class") node.className = value;
      else if (key.startsWith("on")) node.addEventListener(key.slice(2), value);
      else if (key === "value") node.value = value;
      else node.setAttribute(key, value === true ? "" : value);
    }
    for (const child of children.flat()) {
      if (child === null || child === undefined || child === false) continue;
      node.append(child instanceof Node ? child : document.createTextNode(String(child)));
    }
    return node;
  }

  function bytes(n) {
    if (n < 1024) return `${n} B`;
    const units = ["KB", "MB", "GB", "TB"];
    let value = n;
    let unit = -1;
    do {
      value /= 1024;
      unit += 1;
    } while (value >= 1024 && unit < units.length - 1);
    return `${value.toFixed(value < 10 ? 1 : 0)} ${units[unit]}`;
  }

  function roughly(n) {
    if (n < 1000) return number.format(n);
    const digits = Math.floor(Math.log10(n)) - 2;
    return number.format(Math.round(n / 10 ** digits) * 10 ** digits);
  }

  function duration(seconds) {
    if (!isFinite(seconds) || seconds < 0) return "";
    if (seconds < 1) return "under a second";
    if (seconds < 60) return `${Math.round(seconds)} s`;
    const m = Math.floor(seconds / 60);
    const s = Math.round(seconds % 60);
    return m < 60 ? `${m} min ${s} s` : `${Math.floor(m / 60)} h ${m % 60} min`;
  }

  function randomId() {
    if (window.crypto && crypto.randomUUID && window.isSecureContext) return crypto.randomUUID();
    const values = new Uint8Array(16);
    crypto.getRandomValues(values);
    return Array.from(values, (b) => b.toString(16).padStart(2, "0")).join("");
  }

  const baseName = (name) => name.split(/[\\/]/).pop();
  const isSidecarName = (name) => /^DATA\.0\.(TXT|ZIP)$/i.test(baseName(name));
  const shardIndex = (name) => {
    const match = /^DATA\.(\d+)\./i.exec(baseName(name));
    return match ? Number(match[1]) : Infinity;
  };
  const byShard = (a, b) => shardIndex(a.name) - shardIndex(b.name) || a.name.localeCompare(b.name);

  // ---------- the engine in this browser ----------

  // The same engine as the server's, compiled to WebAssembly, running in a
  // worker (assets/convert-worker.js). The export never leaves the computer;
  // afterwards the service hears the totals only, for its usage statistics.
  const engine = {
    worker: null,
    calls: new Map(),

    /**
     * A worker, WebAssembly, and a private file system to write to. (Whether
     * the worker may write to it synchronously, it checks for itself: that
     * is only visible from inside a worker.)
     */
    canRun() {
      return Boolean(window.Worker && window.WebAssembly && navigator.storage && navigator.storage.getDirectory);
    },

    call(type, message, onProgress) {
      if (!this.worker) {
        this.worker = new Worker("assets/convert-worker.js", { type: "module" });
        this.worker.onmessage = ({ data }) => {
          const call = this.calls.get(data.id);
          if (!call) return;
          if (data.progress) {
            if (call.onProgress) call.onProgress(data.progress);
            return;
          }
          this.calls.delete(data.id);
          if (data.error) call.reject(Object.assign(new Error(data.error), { hint: data.hint, unavailable: data.unavailable }));
          else call.resolve(data.result);
        };
        this.worker.onerror = (event) => {
          event.preventDefault();
          this.stop(Object.assign(new Error("The in-browser engine did not start."), { unavailable: true }));
        };
      }
      const id = randomId().replace(/-/g, "");
      return new Promise((resolve, reject) => {
        this.calls.set(id, { resolve, reject, onProgress });
        this.worker.postMessage({ id, type, ...message });
      });
    },

    /** Stop whatever it is doing, at once (a cancelled conversion). */
    stop(error = Object.assign(new Error("Cancelled."), { cancelled: true })) {
      if (this.worker) this.worker.terminate();
      this.worker = null;
      for (const call of this.calls.values()) call.reject(error);
      this.calls.clear();
    },
  };

  /**
   * Tell the service what this browser converted, for its usage statistics:
   * the counts, the format and the SAP table name. Never a file, a field or
   * a value.
   */
  function reportUsage(event) {
    if (state.config.local) return;
    fetch("api/usage", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(event),
      keepalive: true,
    }).catch(() => {});
  }

  function showWhere() {
    const badge = $("mode-badge");
    badge.hidden = false;
    if (state.config.local) {
      badge.textContent = "Running on this computer";
      $("where-text").textContent = "right here on your computer";
    } else if (state.inBrowser) {
      badge.textContent = "Converts in your browser";
      $("where-text").textContent = "right here in your browser, so the file never leaves your computer";
    } else {
      badge.textContent = "Stores nothing";
      $("where-text").textContent = "on the server";
    }
  }

  /** The in-browser engine is unavailable after all: use the server. */
  function fallBack() {
    state.inBrowser = false;
    engine.stop(Object.assign(new Error("The in-browser engine is unavailable."), { unavailable: true }));
    showWhere();
  }

  // ---------- startup ----------

  async function loadConfig() {
    try {
      const response = await fetch("api/config", { cache: "no-store" });
      state.config = await response.json();
    } catch {
      return;
    }
    $("version").textContent = state.config.version;
    // `?convert=server` keeps conversions on the server (and tests that path).
    const onServer = new URLSearchParams(location.search).get("convert") === "server";
    state.inBrowser = Boolean(state.config.browser) && !state.config.local && !onServer && engine.canRun();
    showWhere();
  }

  // ---------- choosing files ----------

  const DROP_HINT = $("drop-sub").innerHTML;

  function showDropMessage(text) {
    $("drop-sub").textContent = text;
  }

  /** Accept files from the picker, a drop, or a folder. */
  function acceptFiles(list, folder = null) {
    let files = Array.from(list || []).filter((f) => !baseName(f.name).startsWith("."));
    if (folder) {
      // In a folder only the export's own members count.
      files = files.filter((f) => /^DATA\.\d+\./i.test(baseName(f.name)));
      if (!files.length) {
        showDropMessage(`${folder} holds no DATA.N files. Choose the export folder, or the files inside it.`);
        return;
      }
    }
    if (!files.length) return;
    const sidecars = files.filter((f) => isSidecarName(f.name));
    const data = files.filter((f) => !isSidecarName(f.name)).sort(byShard);
    const archives = data.filter((f) => /\.zip$/i.test(f.name) && shardIndex(f.name) === Infinity);
    if (archives.length && data.length > 1) {
      showDropMessage("Choose one export at a time: a delivered .zip on its own, or the files of one export.");
      return;
    }

    // A sidecar chosen earlier is kept while its data is still missing.
    const waitingForData = state.report && state.report.kind === "sidecar";
    if (sidecars.length) {
      state.schema = sidecars[0];
      state.schemaEdited = false;
    } else if (!waitingForData) {
      state.schema = null;
      state.schemaEdited = false;
    }
    state.files = data;
    state.exportName = folder;
    if (!data.length && !state.schema) return;
    closeEditor();
    inspect();
  }

  function reset() {
    Object.assign(state, { files: [], schema: null, schemaEdited: false, exportName: null, report: null });
    for (const id of ["inspect", "convert", "result", "progress"]) $(id).hidden = true;
    $("drop").hidden = false;
    $("picker").value = "";
    $("folder-picker").value = "";
    $("record-size").value = "";
    $("drop-sub").innerHTML = DROP_HINT;
    closeEditor();
    $("choose").focus();
  }

  async function trySample() {
    const button = $("try-sample");
    button.disabled = true;
    button.textContent = "Making a sample…";
    try {
      let blob;
      if (state.inBrowser) {
        try {
          blob = new Blob([await engine.call("sample", { records: 20000, shards: 3 })]);
          reportUsage({ event: "sample" });
        } catch (error) {
          if (!error.unavailable) throw error;
          fallBack();
        }
      }
      if (!blob) blob = await (await fetch("api/sample?records=20000&shards=3")).blob();
      state.schema = null;
      state.schemaEdited = false;
      acceptFiles([new File([blob], "BSIS.QUERY.sample.zip", { type: "application/zip" })]);
    } finally {
      button.disabled = false;
      button.textContent = "Try a sample export";
    }
  }

  /** Every file under a dropped folder (Chrome, Edge, Firefox, Safari). */
  async function filesFromEntry(entry) {
    if (entry.isFile) return [await new Promise((resolve, reject) => entry.file(resolve, reject))];
    const reader = entry.createReader();
    const found = [];
    for (;;) {
      const batch = await new Promise((resolve, reject) => reader.readEntries(resolve, reject));
      if (!batch.length) break;
      for (const child of batch) found.push(...(await filesFromEntry(child)));
    }
    return found;
  }

  async function acceptDrop(transfer) {
    const entries = Array.from(transfer.items || [])
      .map((item) => (item.webkitGetAsEntry ? item.webkitGetAsEntry() : null))
      .filter(Boolean);
    const folders = entries.filter((e) => e.isDirectory);
    if (folders.length === 1 && entries.length === 1) {
      acceptFiles(await filesFromEntry(folders[0]), folders[0].name);
    } else if (folders.length) {
      showDropMessage("Drop one export folder at a time.");
    } else {
      acceptFiles(transfer.files);
    }
  }

  // ---------- inspecting ----------

  function describeSelection() {
    const first = dataFile();
    const total = state.files.reduce((sum, f) => sum + f.size, 0);
    let name;
    if (state.exportName) name = `${state.exportName} (folder, ${state.files.length} files)`;
    else if (multi()) name = `${state.files.length} files: ${state.files.map((f) => baseName(f.name)).join(", ")}`;
    else name = first ? first.name : state.schema.name;
    if (state.schema && first) name += state.schemaEdited ? " + edited schema" : ` + ${baseName(state.schema.name)}`;
    $("file-name").textContent = name;
    $("file-meta").textContent = bytes(first ? total : state.schema.size);
  }

  // The options an inspection depends on, to skip repeating one: a field's
  // change event also fires when it loses focus, such as on a click on a
  // suggested record size, which would otherwise re-inspect under the click.
  const inspectOptions = () => `${$("record-size").value.trim()}|${$("text-encoding").value}`;

  /** Describe an export from the start and end of its first file. */
  async function describe(target) {
    const request = {
      head: target.slice(0, HEAD_BYTES),
      tail: target.size > HEAD_BYTES ? target.slice(Math.max(0, target.size - TAIL_BYTES)) : target,
      size: target.size,
      name: state.exportName || target.name,
      schema: state.schema && dataFile() ? state.schema : null,
      recordSize: $("record-size").value.trim(),
      textEncoding: $("text-encoding").value,
    };
    if (state.inBrowser) {
      try {
        const described = await engine.call("inspect", { ...request, recordSize: Number(request.recordSize) || 0 });
        reportUsage({ event: "inspection" });
        return described;
      } catch (error) {
        if (!error.unavailable) throw error;
        fallBack();
      }
    }
    const form = new FormData();
    form.append("head", request.head, "head");
    form.append("tail", request.tail, "tail");
    form.append("size", String(request.size));
    form.append("name", request.name);
    if (request.schema) form.append("schema", request.schema, baseName(request.schema.name));
    if (request.recordSize) form.append("record_size", request.recordSize);
    form.append("text_encoding", request.textEncoding);
    const response = await fetch("api/inspect", { method: "POST", body: form });
    const body = await response.json();
    if (!response.ok) throw new Error(body.error || `The server answered ${response.status}.`);
    return body;
  }

  async function inspect() {
    const target = dataFile() || state.schema;
    if (!target) return;
    $("inspect").hidden = false;
    for (const id of ["convert", "result", "progress", "summary", "tabs", "decode-error"]) $(id).hidden = true;
    describeSelection();
    $("notes").replaceChildren();
    $("inspect-loading").hidden = false;

    state.inspected = inspectOptions();
    let report;
    try {
      report = await describe(target);
    } catch (error) {
      $("inspect-loading").hidden = true;
      $("notes").replaceChildren(el("div", { class: "alert error" }, error.message || String(error)));
      $("edit-schema").hidden = false;
      return;
    }
    $("inspect-loading").hidden = true;
    state.report = report;
    render(report);
  }

  function stat(label, value, note) {
    return el("div", { class: "stat" },
      el("div", { class: "stat-label" }, label),
      el("div", { class: "stat-value" }, value),
      note ? el("div", { class: "stat-note" }, note) : null);
  }

  /** Records and shards for the whole selection, not just its first file. */
  function totals(report) {
    const schema = report.schema;
    if (!multi()) {
      return {
        shards: report.shards ? report.shards.count : report.kind === "loose" ? 1 : null,
        records: report.estimated_records,
        exact: report.estimate_is_exact,
      };
    }
    const shards = state.files.length;
    if (report.kind === "loose" && report.format === "bin" && schema) {
      const all = state.files.every((f) => !/\.(zip|txt)$/i.test(f.name));
      if (all) {
        const sum = state.files.reduce((n, f) => n + Math.floor(f.size / schema.record_size), 0);
        return { shards, records: sum, exact: true };
      }
    }
    if (report.estimated_records != null) {
      const total = state.files.reduce((n, f) => n + f.size, 0);
      return { shards, records: Math.round(report.estimated_records * (total / dataFile().size)), exact: false };
    }
    return { shards, records: null, exact: false };
  }

  function render(report) {
    const schema = report.schema;
    const notes = $("notes");
    notes.replaceChildren();

    if (report.kind === "sidecar") {
      state.schema = state.schema || dataFile();
      state.files = [];
      $("drop").hidden = false;
      showDropMessage("Got the schema. Now add the data: the DATA.N.BIN files, or the export folder.");
    } else {
      $("drop").hidden = true;
    }

    const editButton = $("edit-schema");
    editButton.hidden = false;
    editButton.textContent = schema ? "Edit schema" : "Define the schema";

    if (schema) {
      const summary = $("summary");
      summary.hidden = false;
      const t = totals(report);
      summary.replaceChildren(
        stat("Table", report.table),
        t.records != null
          ? stat("Records", t.exact ? number.format(t.records) : `≈ ${roughly(t.records)}`, t.exact ? null : "estimated from the first shard")
          : null,
        t.shards != null ? stat("Shards", number.format(t.shards)) : null,
        stat("Record size", `${schema.record_size} bytes`,
          schema.padding_size ? `${schema.payload_size} of fields + ${schema.padding_size} pad byte` : `${schema.fields.length} fields`),
        report.format ? stat("Format", report.format === "bin" ? "Fixed-width binary" : "Tab-separated text") : null,
      );
      if (state.schemaEdited) notes.append(el("div", { class: "alert info" }, "Using your edited schema instead of the export's own."));
      for (const warning of schema.warnings) notes.append(el("div", { class: "alert warn" }, `Schema: ${warning}`));
    }
    for (const note of report.notes) notes.append(el("div", { class: "alert info" }, note));

    const failure = $("decode-error");
    if (report.error) {
      failure.hidden = false;
      $("decode-error-text").textContent = report.error.message;
      renderProbe(report.probe, schema);
    } else {
      failure.hidden = true;
    }

    if (schema) {
      $("tabs").hidden = false;
      if (report.preview) renderPreview(report.preview, schema);
      else $("panel-preview").replaceChildren(el("p", { class: "muted small pad" }, "No records to preview yet."));
      renderFields(schema);
    }

    const canConvert = schema && report.kind !== "sidecar" && dataFile();
    $("convert").hidden = !canConvert;
    const shards = totals(report).shards;
    $("split-row").hidden = !(shards && shards > 1);
    updateOutput();

    // A data file with no schema: go straight to defining one.
    if (!schema && report.kind === "loose") openEditor(true);
  }

  function renderProbe(candidates, schema) {
    const target = $("probe");
    target.replaceChildren();
    const useful = (candidates || []).filter((c) => c.clean_records > 0).slice(0, 3);
    const current = Number($("record-size").value) || (schema && schema.record_size);
    const others = useful.filter((c) => c.record_size !== current);
    if (!others.length) {
      target.append(el("p", { class: "small" },
        "No nearby record size decodes cleanly either. Check that the schema belongs to this file, or edit it."));
      return;
    }
    target.append(el("p", { class: "small" }, "Record sizes that decode better:"));
    target.append(el("div", { class: "drop-actions start" },
      others.map((c) =>
        el("button", {
          class: "button small",
          type: "button",
          onclick: () => {
            $("record-size").value = String(c.record_size);
            inspect();
          },
        }, `Use ${c.record_size} bytes (${c.clean_records} clean)`))));
  }

  function renderPreview(preview, schema) {
    const numeric = new Set(schema.fields.filter((f) => f.type === "P").map((f) => f.name));
    const table = el("table", {},
      el("thead", {}, el("tr", {}, preview.columns.map((c) => el("th", { scope: "col", class: numeric.has(c) ? "num" : null }, c)))),
      el("tbody", {}, preview.rows.map((row) =>
        el("tr", {}, row.map((value, i) =>
          value === null
            ? el("td", { class: "null" }, "empty")
            : el("td", { class: numeric.has(preview.columns[i]) ? "num" : null }, value))))));
    $("panel-preview").replaceChildren(table);
  }

  function renderFields(schema) {
    const table = el("table", {},
      el("thead", {}, el("tr", {}, ["Offset", "Field", "Type", "Length", "Decimals", "Bytes"].map((h) =>
        el("th", { scope: "col", class: ["Field", "Type"].includes(h) ? null : "num" }, h)))),
      el("tbody", {}, schema.fields.map((f) =>
        el("tr", {},
          el("td", { class: "num" }, f.offset),
          el("td", {}, f.name),
          el("td", {}, f.type),
          el("td", { class: "num" }, f.length),
          el("td", { class: "num" }, f.decimals),
          el("td", { class: "num" }, f.size)))));
    $("panel-fields").replaceChildren(table);
  }

  function selectTab(which) {
    for (const name of ["preview", "fields"]) {
      const on = name === which;
      $(`tab-${name}`).setAttribute("aria-selected", String(on));
      $(`panel-${name}`).hidden = !on;
    }
  }

  // ---------- schema editor ----------

  const autoSize = (row) => (row.type === "P" ? Number(row.length) || 0 : (Number(row.length) || 0) * 2);

  function openEditor(pasteFirst = false) {
    const schema = state.report && state.report.schema;
    state.rows = schema
      ? schema.fields.map((f) => ({ name: f.name, type: f.type, length: f.length, decimals: f.decimals, size: f.size, manual: f.size !== autoSize(f) }))
      : [{ name: "", type: "C", length: 10, decimals: 0, size: 20, manual: false }];
    $("editor").hidden = false;
    $("editor-error").hidden = true;
    $("paste-box").hidden = !pasteFirst;
    drawRows();
    if (pasteFirst) $("paste-text").focus();
    else $("editor").scrollIntoView({ block: "nearest", behavior: "smooth" });
  }

  function closeEditor() {
    $("editor").hidden = true;
  }

  /** Rebuild the editor table: only for structural changes (add, remove, move, paste). */
  function drawRows() {
    const body = $("editor-rows");
    body.replaceChildren(...state.rows.map((row, i) => {
      const update = (key, cast = (v) => v) => (event) => {
        row[key] = cast(event.target.value);
        if (key === "size") row.manual = Number(row.size) !== autoSize(row);
        if (key === "type" && row.type !== "P") row.decimals = 0;
        refreshRows();
      };
      return el("tr", {},
        el("td", { class: "num", "data-role": "offset" }),
        el("td", {}, el("input", { type: "text", value: row.name, "aria-label": `Field ${i + 1} name`, "data-cell": `${i}-name`, onchange: update("name", (v) => v.trim().toUpperCase()) })),
        el("td", {}, el("select", { "aria-label": `Field ${i + 1} type`, onchange: update("type") },
          TYPES.map((t) => el("option", { value: t, selected: t === row.type }, TYPE_NAMES[t])))),
        el("td", {}, el("input", { type: "number", min: "1", value: String(row.length), "aria-label": `Field ${i + 1} length`, onchange: update("length", Number) })),
        el("td", {}, el("input", { type: "number", min: "0", value: String(row.decimals), "data-role": "decimals", "aria-label": `Field ${i + 1} decimals`, onchange: update("decimals", Number) })),
        el("td", {}, el("input", { type: "number", min: "1", "data-role": "size", "aria-label": `Field ${i + 1} bytes`, onchange: update("size", Number) })),
        el("td", {},
          el("button", { class: "icon-button", type: "button", title: "Move up", "aria-label": `Move field ${i + 1} up`, disabled: i === 0, onclick: () => move(i, -1) }, "↑"),
          el("button", { class: "icon-button", type: "button", title: "Move down", "aria-label": `Move field ${i + 1} down`, disabled: i === state.rows.length - 1, onclick: () => move(i, 1) }, "↓"),
          el("button", { class: "icon-button", type: "button", title: "Remove", "aria-label": `Remove field ${i + 1}`, onclick: () => { state.rows.splice(i, 1); drawRows(); } }, "✕")));
    }));
    refreshRows();
  }

  /** Recompute offsets, automatic sizes and the total in place, keeping focus. */
  function refreshRows() {
    let offset = 0;
    const rows = $("editor-rows").children;
    state.rows.forEach((row, i) => {
      const tr = rows[i];
      if (!row.manual) row.size = autoSize(row);
      tr.querySelector('[data-role="offset"]').textContent = String(offset);
      const size = tr.querySelector('[data-role="size"]');
      if (document.activeElement !== size) size.value = String(row.size);
      size.classList.toggle("auto", !row.manual);
      const decimals = tr.querySelector('[data-role="decimals"]');
      decimals.disabled = row.type !== "P";
      if (row.type !== "P") decimals.value = "0";
      offset += Number(row.size) || 0;
    });
    const record = offset + (offset % 2);
    $("editor-sum").textContent = `${state.rows.length} field${state.rows.length === 1 ? "" : "s"} · ${offset} bytes of fields · ${record}-byte records`;
  }

  function move(i, step) {
    const j = i + step;
    [state.rows[i], state.rows[j]] = [state.rows[j], state.rows[i]];
    drawRows();
  }

  /** Read a DATA.0.TXT, or loose lines of NAME TYPE LENGTH [DECIMALS [BYTES]]. */
  function parseSchemaText(text) {
    const lines = text.replace(/^﻿/, "").split(/\r?\n/).filter((l) => l.trim());
    if (!lines.length) throw new Error("Paste at least one field.");
    const split = (line) => (line.includes("\t") ? line.split("\t") : line.trim().split(/\s+/)).map((c) => c.trim());
    let columns = { name: 0, type: 1, length: 2, decimals: 3, size: 4 };
    const header = split(lines[0]).map((c) => c.toUpperCase());
    let body = lines;
    if (header.includes("NAME") && header.includes("TYPE")) {
      columns = { name: header.indexOf("NAME"), type: header.indexOf("TYPE"), length: header.indexOf("LENG"), decimals: header.indexOf("DEC"), size: header.indexOf("SIZE") };
      body = lines.slice(1);
    }
    return body.map((line, n) => {
      const cells = split(line);
      const pick = (key) => (columns[key] >= 0 ? cells[columns[key]] : undefined);
      const name = (pick("name") || "").toUpperCase();
      const type = (pick("type") || "").toUpperCase();
      if (!name) return null;
      if (!TYPES.includes(type)) throw new Error(`Line ${n + 1}: type "${type}" is not one of ${TYPES.join(", ")}.`);
      const length = Number(pick("length"));
      if (!(length > 0)) throw new Error(`Line ${n + 1}: the length of ${name} is missing.`);
      const decimals = Number(pick("decimals")) || 0;
      const row = { name, type, length, decimals: type === "P" ? decimals : 0, manual: false, size: 0 };
      const size = Number(pick("size"));
      if (size > 0 && size !== autoSize(row)) {
        row.manual = true;
        row.size = size;
      }
      return row;
    }).filter(Boolean);
  }

  function sidecarText() {
    const lines = ["NAME\tTABLE\tTYPE\tLENG\tDEC\tSIZE\tROLL\tKEY"];
    for (const row of state.rows) lines.push([row.name, "", row.type, row.length, row.decimals, row.size, "", ""].join("\t"));
    return `${lines.join("\n")}\n`;
  }

  function validateRows() {
    if (!state.rows.length) return "Add at least one field.";
    const seen = new Set();
    for (const [i, row] of state.rows.entries()) {
      if (!row.name) return `Field ${i + 1} needs a name.`;
      if (seen.has(row.name)) return `${row.name} appears twice.`;
      seen.add(row.name);
      if (!(row.length > 0)) return `${row.name} needs a length.`;
      if (!(row.size > 0)) return `${row.name} needs a size in bytes.`;
    }
    return null;
  }

  function applySchema() {
    const problem = validateRows();
    if (problem) {
      $("editor-error").hidden = false;
      $("editor-error").textContent = problem;
      return;
    }
    state.schema = new File([sidecarText()], "DATA.0.TXT", { type: "text/plain" });
    state.schemaEdited = true;
    closeEditor();
    $("record-size").value = "";
    inspect();
  }

  function downloadSchema() {
    const url = URL.createObjectURL(new Blob([sidecarText()], { type: "text/plain" }));
    const link = el("a", { href: url, download: "DATA.0.TXT" });
    document.body.append(link);
    link.click();
    link.remove();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
  }

  // ---------- options ----------

  function options() {
    const choice = document.querySelector('input[name="format"]:checked').value;
    const opts = {
      format: choice === "excel" ? "csv" : choice,
      bom: choice === "excel",
      split: !$("split-row").hidden && $("split").checked,
      decimals: $("decimals").value,
      on_error: $("on-error").value,
      record_size: $("record-size").value.trim(),
      limit: $("limit").value.trim(),
      delimiter: $("delimiter").value,
      compression: $("compression").value,
      text_encoding: $("text-encoding").value,
    };
    // Excel in comma-decimal locales expects semicolons.
    if (choice === "excel" && opts.delimiter === "," && (1.5).toLocaleString().includes(",")) opts.delimiter = ";";
    return opts;
  }

  const extension = (opts) => ({ csv: "csv", parquet: "parquet", jsonl: "jsonl", tsv: "tsv" })[opts.format];

  function outputName(opts) {
    const table = (state.report && state.report.table) || "export";
    return opts.split ? `${table}.${extension(opts)}.zip` : `${table}.${extension(opts)}`;
  }

  /** The conversion's parameters by the HTTP API's names, which the engine in the page takes too. */
  function params(opts, job) {
    const p = { format: opts.format };
    if (opts.bom) p.bom = true;
    if (opts.split) p.split = true;
    if (opts.decimals !== "exact") p.decimals = opts.decimals;
    if (opts.on_error !== "stop") p.on_error = opts.on_error;
    if (opts.record_size) p.record_size = Number(opts.record_size);
    if (opts.limit) p.limit = Number(opts.limit);
    if (opts.format === "csv" && opts.delimiter !== ",") p.delimiter = opts.delimiter;
    if (opts.format === "parquet" && opts.compression !== "zstd") p.compression = opts.compression;
    if (state.report && state.report.format === "text") p.text_encoding = opts.text_encoding;
    if (multi()) p.multi = true;
    if (state.exportName) p.name = state.exportName;
    if (job) p.job = job;
    return p;
  }

  const query = (opts, job) =>
    new URLSearchParams(Object.entries(params(opts, job)).map(([key, value]) => [key, String(value)])).toString();

  function updateOutput() {
    if (!state.report) return;
    const opts = options();
    $("out-name").textContent = `→ ${outputName(opts)}`;
    const label = { csv: opts.bom ? "CSV for Excel" : "CSV", parquet: "Parquet", jsonl: "JSON Lines" }[opts.format];
    $("go").textContent = `Convert to ${label}`;
    renderSnippets(opts);
  }

  const shellQuote = (text) => (/^[\w./:@=-]+$/.test(text) ? text : `'${text.replace(/'/g, "'\\''")}'`);

  function renderSnippets(opts) {
    const names = state.files.map((f) => baseName(f.name));
    const inputs = state.exportName ? [`${state.exportName}/`] : names.length ? names : ["BSIS.QUERY.zip"];
    const out = outputName(opts);
    const schemaName = state.schema ? (state.schemaEdited ? "DATA.0.TXT" : baseName(state.schema.name)) : null;
    const cli = ["sap-bin", "convert", ...inputs.map(shellQuote), "-o", shellQuote(opts.split ? out.replace(/\.zip$/, "") : out)];
    if (opts.format !== "csv") cli.push("-f", opts.format);
    if (schemaName) cli.push("--schema", shellQuote(schemaName));
    if (opts.split) cli.push("--split");
    if (opts.bom) cli.push("--encoding", "utf-8-sig");
    if (opts.format === "csv" && opts.delimiter !== ",") cli.push("--delimiter", shellQuote(opts.delimiter === "tab" ? "\t" : opts.delimiter));
    if (opts.decimals === "float") cli.push("--float-decimals");
    if (opts.on_error === "skip") cli.push("--on-error", "skip");
    if (opts.record_size) cli.push("--record-size", opts.record_size);
    if (opts.limit) cli.push("--limit", opts.limit);
    if (opts.format === "parquet" && opts.compression !== "zstd") cli.push("--compression", opts.compression);

    const url = new URL(`api/convert?${query(opts)}`, document.baseURI).href;
    let curl;
    if (multi() || schemaName) {
      const parts = [];
      if (schemaName) parts.push(`-F schema=@${shellQuote(schemaName)}`);
      for (const name of names.length ? names : inputs) parts.push(`-F file=@${shellQuote(name)}`);
      curl = `curl -fsS ${parts.join(" ")} \\\n  '${url}' -o ${shellQuote(out)}`;
    } else {
      curl = `curl -fsS --data-binary @${shellQuote(inputs[0])} \\\n  '${url}' -o ${shellQuote(out)}`;
    }

    const snippets = [
      ["Command line (the downloadable app is also the CLI)", cli.join(" ")],
      [state.config.local ? "HTTP, against this local app" : "HTTP, against this server", curl],
    ];
    if (state.schemaEdited) snippets[0][0] += ". Save your edited schema with “Download as DATA.0.TXT” first";
    if (!schemaName && !multi() && inputs[0].toLowerCase().endsWith(".zip")) {
      snippets.push(["Python library", [
        "from sap_bin_parser import SapArchive, BinReader, write_csv",
        "",
        `with SapArchive(${JSON.stringify(inputs[0])}) as archive:`,
        "    schema = archive.schema()",
        "    for shard, stream in archive.iter_shard_streams():",
        "        write_csv(BinReader(stream, schema), f\"{shard.name}.csv\", schema)",
      ].join("\n")]);
    }
    $("snippets").replaceChildren(...snippets.map(([label, code]) => {
      const pre = el("pre", {}, code);
      return el("div", { class: "snippet" },
        el("div", { class: "snippet-label" }, label),
        pre,
        el("button", {
          class: "button small",
          type: "button",
          onclick: async (event) => {
            const button = event.currentTarget;
            try {
              await navigator.clipboard.writeText(code);
              button.textContent = "Copied";
            } catch {
              const range = document.createRange();
              range.selectNodeContents(pre);
              getSelection().removeAllRanges();
              getSelection().addRange(range);
              button.textContent = "Selected";
            }
            setTimeout(() => { button.textContent = "Copy"; }, 1500);
          },
        }, "Copy"));
    }));
  }

  // ---------- converting ----------

  /** Read an error from a JSON problem response. */
  async function problemText(response) {
    try {
      const body = await response.json();
      return { error: body.error || `The server answered ${response.status}.`, hint: body.hint };
    } catch {
      return { error: `The server answered ${response.status}.` };
    }
  }

  // A conversion is three kinds of request, so that neither the browser nor a
  // proxy has to send and receive on one request at the same time (most do
  // not, and a large conversion would stall):
  //   1. POST api/jobs                creates it; the body is the schema, if any
  //   2. GET  api/jobs/{id}/download  in a hidden frame: the browser's download
  //                                   manager streams the result to disk
  //   3. POST api/jobs/{id}/input     the files, 8 MiB at a time, in order; each
  //                                   is answered once the converter has taken it
  async function startConversion() {
    if (!dataFile() || state.job) return;
    const opts = options();
    if (state.inBrowser) {
      convertHere(opts);
      return;
    }
    const id = randomId();
    const size = state.files.reduce((n, f) => n + f.size, 0);
    const job = { id, started: Date.now(), size, name: outputName(opts), timer: null, uploaded: 0 };
    state.job = job;
    $("go").disabled = true;
    $("result").hidden = true;
    $("progress").hidden = false;
    $("meter-fill").style.width = "0";
    $("progress-text").textContent = "Starting…";

    let chunkBytes = 8 * 1024 * 1024;
    try {
      const created = await fetch(`api/jobs?${query(opts, id)}`, { method: "POST", body: state.schema || "" });
      if (!created.ok) {
        finish({ state: "failed", ...(await problemText(created)) });
        return;
      }
      chunkBytes = (await created.json()).chunk_bytes || chunkBytes;
    } catch (error) {
      finish({ state: "failed", error: `Could not reach the server: ${error.message}` });
      return;
    }
    $("sink").src = `api/jobs/${id}/download`;
    job.timer = setInterval(poll, 500);

    try {
      for (const file of state.files) {
        let first = true;
        for (let at = 0; first || at < file.size; at += chunkBytes) {
          if (state.job !== job) return; // cancelled
          const params = new URLSearchParams();
          if (first) {
            params.set("start", "true");
            params.set("name", baseName(file.name));
          }
          const piece = file.slice(at, at + chunkBytes);
          const response = await fetch(`api/jobs/${id}/input?${params}`, { method: "POST", body: piece });
          if (!response.ok) {
            if (state.job === job) finish({ state: "failed", ...(await problemText(response)) });
            return;
          }
          job.uploaded += piece.size;
          first = false;
        }
      }
      await fetch(`api/jobs/${id}/input?end=true`, { method: "POST" });
    } catch (error) {
      if (state.job === job) finish({ state: "failed", error: `The upload was interrupted: ${error.message}` });
    }
  }

  /** Convert with the engine in the page, then hand the file to the downloads. */
  async function convertHere(opts) {
    const size = state.files.reduce((n, f) => n + f.size, 0);
    const job = { id: randomId(), started: Date.now(), size, name: outputName(opts), timer: null, here: true };
    state.job = job;
    $("go").disabled = true;
    $("result").hidden = true;
    $("progress").hidden = false;
    $("meter-fill").style.width = "0";
    $("progress-text").textContent = "Starting…";
    try {
      const { stats, file } = await engine.call(
        "convert",
        { files: state.files, schema: state.schema, params: params(opts) },
        (p) => {
          if (state.job === job) showProgress(p.records, p.bytes, (Date.now() - job.started) / 1000, size);
        },
      );
      if (state.job !== job) return;
      job.name = stats.file_name;
      saveFile(file, stats.file_name);
      reportUsage({
        event: "conversion",
        format: opts.bom ? "excel" : opts.format,
        input: multi() ? "files" : "archive",
        table: stats.table,
        records: stats.records,
        shards: stats.shards,
        bytes_in: size,
        bytes_out: stats.bytes_out,
        seconds: stats.seconds,
      });
      finish({ state: "done", ...stats });
    } catch (error) {
      if (state.job !== job) return; // cancelled
      if (error.unavailable) {
        // Could not run here after all: convert on the server instead.
        state.job = null;
        fallBack();
        startConversion();
        return;
      }
      reportUsage({ event: "failure", failure: "input" });
      finish({ state: "failed", error: error.message, hint: error.hint });
    }
  }

  function saveFile(file, name) {
    const url = URL.createObjectURL(file);
    const link = el("a", { href: url, download: name });
    document.body.append(link);
    link.click();
    link.remove();
    // Long enough for the browser to copy even a large file into Downloads.
    setTimeout(() => URL.revokeObjectURL(url), 10 * 60 * 1000);
  }

  function showProgress(records, bytesIn, elapsed, size) {
    const fraction = Math.min(1, bytesIn / Math.max(1, size));
    $("meter-fill").style.width = `${(fraction * 100).toFixed(1)}%`;
    const rate = elapsed > 0 ? records / elapsed : 0;
    const left = fraction > 0.02 ? (elapsed / fraction) * (1 - fraction) : NaN;
    const parts = [`${number.format(records)} records`];
    if (rate > 0) parts.push(`${roughly(Math.round(rate))} per second`);
    if (isFinite(left) && fraction < 1) parts.push(`about ${duration(left)} left`);
    if (fraction >= 1) parts.push("finishing");
    $("progress-text").textContent = parts.join(" · ");
  }

  async function poll() {
    const job = state.job;
    if (!job) return;
    let status;
    try {
      const response = await fetch(`api/jobs/${job.id}`, { cache: "no-store" });
      if (response.status === 404) {
        if (Date.now() - job.started > 20000) {
          finish({ state: "failed", error: "The server did not start the conversion. It may be busy: try again in a minute." });
        }
        return;
      }
      status = await response.json();
    } catch {
      return;
    }
    if (status.state === "running") {
      if (!status.download_connected && Date.now() - job.started > 15000) {
        $("progress-text").textContent =
          "Waiting for the download to start. If your browser asks whether to allow downloads from this site, allow it.";
        return;
      }
      showProgress(status.records, status.bytes_in, status.elapsed, job.size);
    } else {
      finish(status);
    }
  }

  function finish(status) {
    const job = state.job;
    if (job) clearInterval(job.timer);
    state.job = null;
    $("go").disabled = false;
    $("progress").hidden = true;
    const result = $("result");
    result.hidden = false;
    if (status.state === "done") {
      const shards = status.shards === 1 ? "1 shard" : `${number.format(status.shards)} shards`;
      result.replaceChildren(
        el("div", { class: "alert ok" },
          el("strong", {}, `Converted ${number.format(status.records)} records from ${shards} in ${duration(status.seconds)}.`),
          el("p", {}, job && job.here
            ? `Converted in your browser: the file never left your computer. Saved as ${job.name}.`
            : `Your browser saved it as ${job ? job.name : "a download"}.`)),
        ...(status.warnings || []).map((w) => el("div", { class: "alert warn" }, w)));
    } else if (status.state === "cancelled") {
      result.replaceChildren(el("div", { class: "alert info" }, "Cancelled. Nothing was kept."));
    } else {
      result.replaceChildren(el("div", { class: "alert error" },
        el("strong", {}, "The conversion stopped."),
        el("p", { class: "mono small" }, status.error || "Unknown error"),
        status.hint ? el("p", { class: "small" }, status.hint) : null,
        el("p", { class: "small" }, "Any partial download is incomplete: delete it.")));
    }
  }

  async function cancel() {
    const job = state.job;
    if (!job) return;
    finish({ state: "cancelled" }); // also stops the upload loop
    if (job.here) {
      engine.stop();
      reportUsage({ event: "failure", failure: "cancelled" });
      return;
    }
    $("sink").src = "about:blank";
    try {
      await fetch(`api/jobs/${job.id}/cancel`, { method: "POST" });
    } catch {
      // Closing the download above stops the conversion regardless.
    }
  }

  // ---------- wiring ----------

  function wire() {
    const picker = $("picker");
    const folderPicker = $("folder-picker");
    $("choose").addEventListener("click", () => picker.click());
    $("choose-folder").addEventListener("click", () => folderPicker.click());
    picker.addEventListener("change", () => acceptFiles(picker.files));
    folderPicker.addEventListener("change", () => {
      const files = Array.from(folderPicker.files);
      const folder = files.length ? (files[0].webkitRelativePath || "").split("/")[0] || "export" : null;
      acceptFiles(files, folder);
    });
    $("try-sample").addEventListener("click", trySample);
    $("reset").addEventListener("click", reset);
    $("go").addEventListener("click", startConversion);
    $("cancel").addEventListener("click", cancel);
    $("tab-preview").addEventListener("click", () => selectTab("preview"));
    $("tab-fields").addEventListener("click", () => selectTab("fields"));
    document.querySelector(".tab-list").addEventListener("keydown", (event) => {
      if (event.key === "ArrowRight" || event.key === "ArrowLeft") {
        const next = $("tab-preview").getAttribute("aria-selected") === "true" ? "fields" : "preview";
        selectTab(next);
        $(`tab-${next}`).focus();
      }
    });
    for (const input of document.querySelectorAll("#convert input, #convert select")) {
      input.addEventListener("change", () => {
        if (input.id === "record-size" || input.id === "text-encoding") {
          if (inspectOptions() !== state.inspected) inspect();
        } else updateOutput();
      });
    }

    // Schema editor.
    $("edit-schema").addEventListener("click", () => openEditor(!(state.report && state.report.schema)));
    $("add-field").addEventListener("click", () => {
      state.rows.push({ name: "", type: "C", length: 10, decimals: 0, size: 20, manual: false });
      drawRows();
      const inputs = $("editor-rows").querySelectorAll('input[type="text"]');
      inputs[inputs.length - 1].focus();
    });
    $("paste-schema").addEventListener("click", () => {
      $("paste-box").hidden = !$("paste-box").hidden;
      if (!$("paste-box").hidden) $("paste-text").focus();
    });
    $("paste-apply").addEventListener("click", () => {
      try {
        state.rows = parseSchemaText($("paste-text").value);
        $("editor-error").hidden = true;
        $("paste-box").hidden = true;
        drawRows();
      } catch (error) {
        $("editor-error").hidden = false;
        $("editor-error").textContent = error.message;
      }
    });
    $("load-schema").addEventListener("click", () => $("schema-picker").click());
    $("schema-picker").addEventListener("change", () => {
      const file = $("schema-picker").files[0];
      if (!file) return;
      state.schema = file;
      state.schemaEdited = false;
      closeEditor();
      inspect();
    });
    $("download-schema").addEventListener("click", downloadSchema);
    $("discard-schema").addEventListener("click", closeEditor);
    $("apply-schema").addEventListener("click", applySchema);

    // Drag and drop, anywhere on the page.
    const drop = $("drop");
    let depth = 0;
    document.addEventListener("dragenter", (event) => {
      if (!event.dataTransfer || !Array.from(event.dataTransfer.types).includes("Files")) return;
      depth += 1;
      drop.classList.add("over");
    });
    document.addEventListener("dragleave", () => {
      depth = Math.max(0, depth - 1);
      if (!depth) drop.classList.remove("over");
    });
    document.addEventListener("dragover", (event) => event.preventDefault());
    document.addEventListener("drop", (event) => {
      event.preventDefault();
      depth = 0;
      drop.classList.remove("over");
      if (state.job || !event.dataTransfer) return;
      acceptDrop(event.dataTransfer);
    });

    document.addEventListener("keydown", (event) => {
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "o") {
        event.preventDefault();
        picker.click();
      }
    });
    window.addEventListener("beforeunload", (event) => {
      if (state.job) {
        event.preventDefault();
        event.returnValue = "";
      }
    });
  }

  wire();
  loadConfig();
})();
