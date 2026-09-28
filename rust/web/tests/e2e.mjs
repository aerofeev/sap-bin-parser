// End-to-end test of the web page in a real browser.
//
//   node rust/web/tests/e2e.mjs [path/to/sap-bin] [output-dir]
//   BASE_PATH=/sap-bin-parser node rust/web/tests/e2e.mjs ...   (under a path prefix)
//   CONVERT=server node rust/web/tests/e2e.mjs ...   (the server path, not WebAssembly)
//
// Starts the server, drives the page with Playwright (Chromium), downloads
// the converted files into output-dir, and fails on any console error,
// including Content-Security-Policy violations.
import { spawn } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { chromium } from "playwright";

const binary = resolve(process.argv[2] ?? "target/release/sap-bin");
const outDir = resolve(process.argv[3] ?? "target/e2e");
mkdirSync(outDir, { recursive: true });
const port = 18000 + Math.floor(Math.random() * 2000);
const basePath = (process.env.BASE_PATH || "").replace(/\/+$/, "");
// CONVERT=server tests the server path, which browsers without WebAssembly
// or a private file system use.
const onServer = process.env.CONVERT === "server";
const pageUrl = `${onServer ? "?convert=server" : ""}`;
const base = `http://127.0.0.1:${port}${basePath}`;

const statsToken = "e2e-token-0123456789abcdefgh";
const serverArgs = ["serve", "--port", String(port), "--stats-token", statsToken];
if (basePath) serverArgs.push("--base-path", basePath);
const server = spawn(binary, serverArgs, { stdio: ["ignore", "ignore", "pipe"] });
let serverLog = "";
server.stderr.on("data", (d) => { serverLog += d; });

function check(condition, message) {
  if (!condition) throw new Error(`FAILED: ${message}`);
  console.log(`ok - ${message}`);
}

async function waitForServer() {
  for (let i = 0; i < 50; i++) {
    try {
      if ((await fetch(`${base}/healthz`)).ok) return;
    } catch { /* not up yet */ }
    await new Promise((r) => setTimeout(r, 100));
  }
  throw new Error(`server did not start:\n${serverLog}`);
}

const browser = await chromium.launch({ executablePath: process.env.CHROMIUM_PATH || undefined });
try {
  await waitForServer();
  // A real locale: headless Chromium may report the system's (en-US@posix),
  // which Intl rejects.
  const context = await browser.newContext({ acceptDownloads: true, viewport: { width: 1200, height: 900 }, locale: "en-US" });
  const page = await context.newPage();
  const problems = [];
  // Requests that would carry an export to the server.
  const uploads = [];
  page.on("console", (m) => { if (m.type() === "error") problems.push(m.text()); });
  page.on("pageerror", (e) => problems.push(String(e)));
  page.on("request", (r) => { if (/\/api\/(inspect|jobs|convert)/.test(r.url())) uploads.push(new URL(r.url()).pathname); });

  await page.goto(`${base}/${pageUrl}`);
  check((await page.title()).startsWith("sap-bin"), "page loads");
  await page.waitForSelector("#mode-badge:not([hidden])");
  const badge = await page.textContent("#mode-badge");
  check(badge === (onServer ? "Stores nothing" : "Converts in your browser"), `hosted mode badge (${badge})`);

  // 1. The sample export: inspect, preview, fields.
  await page.click("#try-sample");
  await page.waitForSelector("#summary:not([hidden])");
  const summary = await page.textContent("#summary");
  check(summary.includes("BSIS") && summary.includes("126 bytes") && summary.includes("125 of fields + 1 pad byte"),
    "summary shows table and record geometry");
  check(summary.includes("Shards3"), "summary shows the shard count");
  check((await page.$$("#panel-preview tbody tr")).length === 20, "preview shows 20 rows");
  await page.click("#tab-fields");
  check((await page.$$("#panel-fields tbody tr")).length === 9, "fields tab lists 9 fields");
  await page.screenshot({ path: join(outDir, "inspect.png"), fullPage: false });

  // 2. Convert it to Parquet through the browser's download manager.
  await page.check('input[name="format"][value="parquet"]', { force: true });
  check((await page.textContent("#go")) === "Convert to Parquet", "button follows the format");
  check((await page.textContent("#out-name")).includes("BSIS.parquet"), "output name shown");
  const [parquet] = await Promise.all([page.waitForEvent("download"), page.click("#go")]);
  check(parquet.suggestedFilename() === "BSIS.parquet", "download is named after the table");
  await parquet.saveAs(join(outDir, "sample.parquet"));
  await page.waitForSelector("#result .alert.ok");
  check((await page.textContent("#result")).includes("Converted in your browser") !== onServer,
    onServer ? "converted on the server" : "converted in the browser, without an upload");
  await page.waitForSelector("#result .alert.ok");
  check((await page.textContent("#result")).includes("Converted 60,000 records from 3 shards"), "completion reported");
  await page.screenshot({ path: join(outDir, "done.png") });

  // 3. A loose .BIN with its sidecar, dropped together, to CSV.
  const archive = Buffer.from(await (await fetch(`${base}/api/sample?records=500&shards=1`)).arrayBuffer());
  writeFileSync(join(outDir, "one.zip"), archive);
  writeFileSync(join(outDir, "three.zip"), Buffer.from(await (await fetch(`${base}/api/sample?records=400&shards=3`)).arrayBuffer()));
  const { execFileSync } = await import("node:child_process");
  execFileSync("python3", ["-c", `
import zipfile, io, sys
z = zipfile.ZipFile(sys.argv[1])
for member, inner in (("BSIS.QUERY/DATA.1.zip", "DATA.1.BIN"), ("BSIS.QUERY/DATA.0.zip", "DATA.0.TXT")):
    data = zipfile.ZipFile(io.BytesIO(z.read(member))).read(inner)
    open(sys.argv[2] + "/" + inner, "wb").write(data)
`, join(outDir, "one.zip"), outDir]);
  await page.click("#reset");
  await page.setInputFiles("#picker", [join(outDir, "DATA.1.BIN"), join(outDir, "DATA.0.TXT")]);
  await page.waitForSelector("#summary:not([hidden])");
  check((await page.textContent("#file-name")).includes("DATA.1.BIN + DATA.0.TXT"), "sidecar paired with the data file");
  check((await page.textContent("#summary")).includes("500"), "exact record count for a loose file");
  await page.check('input[name="format"][value="csv"]', { force: true });
  const [csv] = await Promise.all([page.waitForEvent("download"), page.click("#go")]);
  await csv.saveAs(join(outDir, "loose.csv"));
  await page.waitForSelector("#result .alert.ok");
  check((await page.textContent("#result")).includes("500 records"), "loose conversion reported");

  // 3b. Several shard files plus their sidecar, chosen together.
  execFileSync("python3", ["-c", `
import zipfile, io, sys, os
z = zipfile.ZipFile(sys.argv[1]); out = sys.argv[2]
os.makedirs(out + "/multi", exist_ok=True); os.makedirs(out + "/folder", exist_ok=True)
z.extractall(out + "/folder")
for n in (1, 2, 3):
    data = zipfile.ZipFile(io.BytesIO(z.read(f"BSIS.QUERY/DATA.{n}.zip"))).read(f"DATA.{n}.BIN")
    open(f"{out}/multi/DATA.{n}.BIN", "wb").write(data)
open(out + "/multi/DATA.0.TXT", "wb").write(zipfile.ZipFile(io.BytesIO(z.read("BSIS.QUERY/DATA.0.zip"))).read("DATA.0.TXT"))
`, join(outDir, "three.zip"), outDir]);
  await page.click("#reset");
  await page.setInputFiles("#picker", ["DATA.3.BIN", "DATA.1.BIN", "DATA.0.TXT", "DATA.2.BIN"].map((n) => join(outDir, "multi", n)));
  await page.waitForSelector("#summary:not([hidden])");
  const multiSummary = await page.textContent("#summary");
  check(multiSummary.includes("Records1,200") && multiSummary.includes("Shards3"), "separate shard files counted exactly");
  const [multiCsv] = await Promise.all([page.waitForEvent("download"), page.click("#go")]);
  await multiCsv.saveAs(join(outDir, "multi.csv"));
  await page.waitForSelector("#result .alert.ok");
  check((await page.textContent("#result")).includes("1,200 records from 3 shards"), "separate files convert as one export");

  // 3c. The unzipped export folder, nested per-shard zips and all.
  await page.click("#reset");
  await page.setInputFiles("#folder-picker", join(outDir, "folder", "BSIS.QUERY"));
  await page.waitForSelector("#summary:not([hidden])");
  check((await page.textContent("#file-name")).includes("BSIS.QUERY (folder, 3 files)"), "folder recognised");
  const [folderCsv] = await Promise.all([page.waitForEvent("download"), page.click("#go")]);
  check(folderCsv.suggestedFilename() === "BSIS.csv", "folder download named after the table");
  await folderCsv.saveAs(join(outDir, "folder.csv"));
  await page.waitForSelector("#result .alert.ok");
  check((await page.textContent("#result")).includes("1,200 records from 3 shards"), "folder converts as one export");

  // 3d. A data file with no schema: define it by pasting, then edit it.
  await page.click("#reset");
  await page.setInputFiles("#picker", [join(outDir, "multi", "DATA.1.BIN")]);
  await page.waitForSelector("#editor:not([hidden])");
  check(!(await page.isHidden("#paste-box")), "no schema: the editor opens ready for pasting");
  const sidecar = (await import("node:fs")).readFileSync(join(outDir, "multi", "DATA.0.TXT"), "utf8");
  await page.fill("#paste-text", sidecar);
  await page.click("#paste-apply");
  check((await page.textContent("#editor-sum")).includes("9 fields · 125 bytes of fields · 126-byte records"), "pasted sidecar parsed");
  await page.fill('[data-cell="1-name"]', "ACCOUNT");
  await page.dispatchEvent('[data-cell="1-name"]', "change");
  await page.click("#apply-schema");
  await page.waitForSelector("#summary:not([hidden])");
  check((await page.textContent("#panel-preview thead")).includes("ACCOUNT"), "edited field name used");
  check((await page.textContent("#file-name")).includes("edited schema"), "edited schema flagged");
  const [edited] = await Promise.all([page.waitForEvent("download"), page.click("#go")]);
  await edited.saveAs(join(outDir, "edited.csv"));
  await page.waitForSelector("#result .alert.ok");
  const editedHead = (await import("node:fs")).readFileSync(join(outDir, "edited.csv"), "utf8").split("\r\n")[0];
  check(editedHead === "BUKRS,ACCOUNT,ZUONR,GJAHR,BELNR,BUZEI,BUDAT,BLART,DMBTR", "edited schema drives the output");

  // 3e. Typing a schema from scratch: short lines, wrong width caught by the probe.
  await page.click("#edit-schema");
  await page.click("#paste-schema");
  await page.fill("#paste-text", "BUKRS C 4\nREST C 55\nDMBTR P 7 2");
  await page.click("#paste-apply");
  check((await page.textContent("#editor-sum")).includes("3 fields · 125 bytes"), "typed schema computes sizes");
  await page.click("#apply-schema");
  await page.waitForSelector("#summary:not([hidden])");
  check((await page.$$("#panel-preview tbody tr")).length === 20 && (await page.isHidden("#decode-error")), "hand-typed schema decodes");
  await page.click("#reset");
  await page.setInputFiles("#picker", [join(outDir, "multi", "DATA.1.BIN"), join(outDir, "multi", "DATA.0.TXT")]);
  await page.waitForSelector("#summary:not([hidden])");

  // 3f. A large export: far more output than network buffers hold, so this
  // stalls unless upload and download travel on separate requests.
  writeFileSync(join(outDir, "big.zip"), Buffer.from(await (await fetch(`${base}/api/sample?records=200000&shards=1`)).arrayBuffer()));
  execFileSync("python3", ["-c", `
import zipfile, io, sys
z = zipfile.ZipFile(sys.argv[1] + "/big.zip")
data = zipfile.ZipFile(io.BytesIO(z.read("BSIS.QUERY/DATA.1.zip"))).read("DATA.1.BIN")
open(sys.argv[1] + "/big.BIN", "wb").write(data * 5)
`, outDir]);
  await page.click("#reset");
  await page.setInputFiles("#picker", [join(outDir, "big.BIN"), join(outDir, "multi", "DATA.0.TXT")]);
  await page.waitForSelector("#summary:not([hidden])");
  await page.check('input[name="format"][value="csv"]', { force: true });
  const bigStarted = Date.now();
  const [big] = await Promise.all([page.waitForEvent("download"), page.click("#go")]);
  await big.saveAs(join(outDir, "big.csv"));
  await page.waitForSelector("#result .alert.ok", { timeout: 120000 });
  const bigLines = (await import("node:fs")).readFileSync(join(outDir, "big.csv"), "utf8").split("\r\n").length - 2;
  check(bigLines === 1000000, `a 126 MB export converts in full through the page (${bigLines} records, ${((Date.now() - bigStarted) / 1000).toFixed(1)} s)`);
  await page.click("#reset");
  await page.setInputFiles("#picker", [join(outDir, "multi", "DATA.1.BIN"), join(outDir, "multi", "DATA.0.TXT")]);
  await page.waitForSelector("#summary:not([hidden])");
  (await import("node:fs")).rmSync(join(outDir, "big.BIN"));
  (await import("node:fs")).rmSync(join(outDir, "big.csv"));

  // 4. A wrong record size: clear error, probe, one-click fix.
  await page.click("summary:has-text('More options')");
  await page.fill("#record-size", "127");
  await page.dispatchEvent("#record-size", "change");
  await page.waitForSelector("#decode-error:not([hidden])");
  check((await page.textContent("#decode-error")).includes("usually means the record size is wrong"), "misalignment explained");
  await page.click("#probe button:has-text('Use 126 bytes')");
  await page.waitForSelector("#decode-error", { state: "hidden" });
  const applied = await page.inputValue("#record-size");
  check(applied === "126", `probe suggestion applied (${JSON.stringify(applied)})`);

  // 5. Command-line equivalents follow the options.
  await page.click("summary:has-text('Do the same')");
  const snippets = await page.textContent("#snippets");
  check(snippets.includes("sap-bin convert DATA.1.BIN") && snippets.includes("--schema DATA.0.TXT") && snippets.includes("--record-size 126"),
    "CLI snippet mirrors the options");
  check(snippets.includes(`${base}/api/convert?`), "curl snippet points at this server, prefix included");

  // 6. Phone width: nothing scrolls sideways.
  await page.setViewportSize({ width: 375, height: 800 });
  await page.goto(`${base}/${pageUrl}`);
  const overflow = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
  check(overflow <= 0, "no horizontal scroll at 375px");
  await page.screenshot({ path: join(outDir, "phone.png"), fullPage: true });

  // 7. Dark mode renders.
  const dark = await browser.newContext({ colorScheme: "dark", viewport: { width: 1200, height: 900 }, locale: "en-US" });
  const darkPage = await dark.newPage();
  await darkPage.goto(`${base}/${pageUrl}`);
  await darkPage.click("#try-sample");
  await darkPage.waitForSelector("#summary:not([hidden])");
  await darkPage.screenshot({ path: join(outDir, "dark.png") });

  if (!onServer) {
    check(uploads.length === 0, `in the browser, nothing was sent to the server${uploads.length ? `: ${uploads.join(" ")}` : ""}`);
  }

  // 8. Downloads are compressed on the wire, and saved decompressed.
  const encoding = await page.evaluate(async () => {
    const sample = await (await fetch("api/sample?records=2000&shards=1")).blob();
    const response = await fetch("api/convert?format=csv", { method: "POST", body: sample });
    const text = await response.text();
    return { coding: response.headers.get("content-encoding"), lines: text.split("\r\n").length - 2 };
  });
  check(encoding.coding === "zstd" && encoding.lines === 2000, `CSV downloads are zstd-compressed (${JSON.stringify(encoding)})`);

  // 9. The operator's usage dashboard.
  await page.setViewportSize({ width: 1200, height: 900 });
  await page.goto(`${base}/stats`);
  await page.fill("#token", "not-the-token-at-all-000000");
  await page.click("#sign-in-form button[type=submit]");
  await page.waitForSelector("#sign-in-error:not([hidden])");
  check((await page.textContent("#sign-in-error")).includes("not right"), "stats: a wrong token is refused");
  // The browser logs that refusal (a 401) as a failed load; it is expected.
  const refusal = problems.findIndex((p) => p.includes("401"));
  if (refusal >= 0) problems.splice(refusal, 1);
  await page.fill("#token", statsToken);
  await page.click("#sign-in-form button[type=submit]");
  await page.waitForSelector("#dashboard:not([hidden])");
  const records = Number((await page.textContent("#kpi-records")).replace(/\D/g, ""));
  check(records > 1000000, `stats: the dashboard counts the records converted (${records})`);
  check((await page.textContent("#tables")).includes("BSIS"), "stats: SAP tables are listed");
  await page.screenshot({ path: join(outDir, "stats.png"), fullPage: true });
  await page.reload();
  await page.waitForSelector("#dashboard:not([hidden])");
  check(true, "stats: the token lasts for the tab");

  check(problems.length === 0, `no console errors${problems.length ? `: ${problems.join(" | ")}` : ""}`);
  console.log("all browser checks passed");
} finally {
  await browser.close();
  server.kill();
}
