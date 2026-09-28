// The table viewer: Perspective (https://perspective-dev.github.io), in a
// frame of the page's own, because Perspective loads parts of itself from
// blob: URLs, which the page's strict policy forbids (see VIEWER_CSP in
// server.rs). The page sends the records here as Arrow; this shows them and
// says how many there are. Nothing is fetched from anywhere else.
const send = (message) => parent.postMessage(message, location.origin);
const viewer = document.querySelector("perspective-viewer");
let table = null;
let loads = 0;

const ready = (async () => {
  const client = await import("./perspective/client/dist/cdn/perspective.js");
  await import("./perspective/viewer/dist/cdn/perspective-viewer.js");
  // The plugins register with the viewer, so they come after it.
  await Promise.all([
    import("./perspective/viewer-datagrid/dist/cdn/perspective-viewer-datagrid.js"),
    import("./perspective/viewer-charts/dist/cdn/perspective-viewer-charts.js"),
  ]);
  return client.default.worker();
})();

ready.then(
  () => send({ type: "ready" }),
  (error) => send({ type: "error", error: error.message || String(error) }),
);

addEventListener("message", async ({ data, origin, source }) => {
  if (origin !== location.origin || source !== parent || data.type !== "load") return;
  try {
    const worker = await ready;
    const name = `records-${++loads}`;
    const next = await worker.table(data.arrow, { name });
    await viewer.load(worker);
    await viewer.restore({ table: name, theme: data.theme, title: data.title });
    if (table) await table.delete();
    table = next;
    send({ type: "loaded", rows: await table.size() });
  } catch (error) {
    send({ type: "error", error: error.message || String(error) });
  }
});
