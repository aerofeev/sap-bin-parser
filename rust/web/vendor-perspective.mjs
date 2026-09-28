// Copy the Perspective table viewer (https://perspective-dev.github.io) out
// of node_modules into perspective/, which the binary embeds when it exists
// (see rust/build.rs). The page may load nothing from anywhere else, so the
// viewer is served from the binary like everything else.
//
//   cd rust/web && npm ci && npm run perspective
//
// The CDN builds find their WebAssembly by relative path (the client looks
// for the server's engine in a sibling server/ folder), so the packages keep
// their layout.
import { copyFileSync, mkdirSync, rmSync } from "node:fs";
import { dirname } from "node:path";

const FILES = {
  client: ["dist/cdn/perspective.js", "LICENSE.md"],
  server: ["dist/wasm/perspective-server.wasm", "dist/wasm/perspective-server.memory64.wasm"],
  viewer: ["dist/cdn/perspective-viewer.js", "dist/wasm/perspective-viewer.wasm", "dist/css/themes.css"],
  "viewer-datagrid": ["dist/cdn/perspective-viewer-datagrid.js"],
  "viewer-charts": ["dist/cdn/perspective-viewer-charts.js"],
};

const from = new URL("node_modules/@perspective-dev/", import.meta.url);
const to = new URL("perspective/", import.meta.url);
rmSync(to, { recursive: true, force: true });
for (const [name, files] of Object.entries(FILES)) {
  for (const file of files) {
    const target = new URL(`${name}/${file}`, to);
    mkdirSync(dirname(target.pathname), { recursive: true });
    copyFileSync(new URL(`${name}/${file}`, from), target);
  }
}
console.log("Perspective copied to rust/web/perspective/");
