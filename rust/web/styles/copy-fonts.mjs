// Copy the Inter subsets the page uses out of node_modules, so they are
// served from the binary like every other asset. Run after upgrading
// @fontsource-variable/inter: npm run fonts
import { copyFileSync } from "node:fs";

const from = new URL("../node_modules/@fontsource-variable/inter/", import.meta.url);
const to = new URL("../fonts/", import.meta.url);
for (const subset of ["latin", "cyrillic"]) {
  const file = `inter-${subset}-wght-normal.woff2`;
  copyFileSync(new URL(`files/${file}`, from), new URL(file, to));
}
copyFileSync(new URL("LICENSE", from), new URL("LICENSE-Inter.txt", to));
