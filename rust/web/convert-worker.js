// sap-bin in the browser: the same engine as the server and the app,
// compiled to WebAssembly, converting in this worker so the page stays
// responsive.
//
// The files are read here, a few megabytes at a time. The output is written
// to the browser's private file system for this site (OPFS) as it comes, so
// memory stays flat whatever the size. When the conversion ends, the page
// hands that file to the browser's downloads. Nothing is sent anywhere.
//
// Messages in: { id, type: "inspect" | "sample" | "convert", ... }.
// Messages out: { id, progress } while converting, then { id, result } or
// { id, error, hint }, with `unavailable` if the engine could not load.
import init, { convert, inspect, sample } from "./wasm/sap_bin_wasm.js";

// Writing the output needs synchronous access to the private file system,
// which browsers offer in workers only, so it is checked here.
const ready = (async () => {
  if (!(self.FileSystemFileHandle && "createSyncAccessHandle" in FileSystemFileHandle.prototype)) {
    throw new Error("This browser cannot write files from a worker.");
  }
  await init();
})();
const OUTPUT = "sap-bin-output-";
// A finished output stays until the download has surely copied it.
const KEEP_MS = 10 * 60 * 1000;

const reply = (id, message) => self.postMessage({ id, ...message });

/** The engine reports failures as JSON { error, hint }. */
function problem(thrown) {
  if (typeof thrown === "string") {
    try {
      const parsed = JSON.parse(thrown);
      if (parsed && parsed.error) return parsed;
    } catch {
      // Not the engine's JSON: an error of its own.
    }
    return { error: thrown };
  }
  if (thrown && thrown.name === "QuotaExceededError") {
    return {
      error: "The browser ran out of storage space for the output.",
      hint: "Choose Parquet, which is much smaller, or convert with the app for your computer.",
    };
  }
  return { error: (thrown && thrown.message) || String(thrown) };
}

const bytesOf = async (blob) => (blob ? new Uint8Array(await blob.arrayBuffer()) : undefined);

/** Remove outputs of earlier conversions that have had time to download. */
async function sweep(root) {
  for await (const [name, handle] of root.entries()) {
    if (!name.startsWith(OUTPUT)) continue;
    try {
      const file = await handle.getFile();
      if (Date.now() - file.lastModified > KEEP_MS) await root.removeEntry(name);
    } catch {
      // Still open elsewhere, or gone already.
    }
  }
}

async function runConversion(id, { files, schema, params }) {
  const root = await navigator.storage.getDirectory();
  await sweep(root);
  const name = `${OUTPUT}${id}`;
  const handle = await root.getFileHandle(name, { create: true });
  const access = await handle.createSyncAccessHandle();
  let at = 0;
  let reported = 0;
  try {
    const stats = JSON.parse(
      convert(
        files,
        await bytesOf(schema),
        JSON.stringify(params),
        (chunk) => {
          at += access.write(chunk, { at });
        },
        (records, bytes) => {
          const now = performance.now();
          if (now - reported > 150) {
            reported = now;
            reply(id, { progress: { records, bytes } });
          }
        },
      ),
    );
    access.flush();
    access.close();
    return { stats: { ...stats, bytes_out: at }, file: await handle.getFile() };
  } catch (error) {
    access.close();
    await root.removeEntry(name).catch(() => {});
    throw error;
  }
}

self.onmessage = async ({ data }) => {
  const { id, type } = data;
  try {
    await ready;
  } catch (thrown) {
    // No engine here: the page falls back to converting on the server.
    reply(id, { ...problem(thrown), unavailable: true });
    return;
  }
  try {
    if (type === "inspect") {
      const report = inspect(
        await bytesOf(data.head),
        await bytesOf(data.tail),
        data.size,
        data.name,
        await bytesOf(data.schema),
        data.recordSize || undefined,
        data.textEncoding,
      );
      reply(id, { result: JSON.parse(report) });
    } else if (type === "sample") {
      reply(id, { result: sample(data.records, data.shards) });
    } else if (type === "convert") {
      reply(id, { result: await runConversion(id, data) });
    } else {
      reply(id, { error: `unknown request ${type}` });
    }
  } catch (thrown) {
    reply(id, problem(thrown));
  }
};
