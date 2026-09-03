#!/usr/bin/env node
// Regenerates crates/majik-app/assets/icons/*.svg from HugeIcons' free set (the icon set ../flarly
// uses). The package ships icons as JS path arrays, not SVG files, so each manifest entry in
// icons.json is imported and written out as a 24×24 stroke SVG. Stroke width is raised from the
// baked-in 1.5 to 2 — flarly's house weight for chrome icons — while the few icons that use another
// width on purpose (dots) keep theirs. An entry may be `{ "icon", "fill": true }` for a filled variant or
// `{ "icon", "rotate": 90 }` to turn the drawing about its centre.
//
//   node packaging/icons.mjs                 # fetches @hugeicons/core-free-icons@VERSION with npm pack
//   HUGEICONS_DIR=path/to/package node …     # uses an installed copy instead
import { execFileSync } from "node:child_process";
import { mkdtempSync, readdirSync, readFileSync, rmSync, unlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const VERSION = "4.3.0";
const STROKE_WIDTH = "2";
const NATIVE_STROKE_WIDTH = "1.5";
// OS-chrome glyphs gpui-component's title bar draws on Windows/Linux; not part of any icon set.
const KEEP = new Set(["window-close", "window-minimize", "window-maximize", "window-restore", "resize-corner"]);

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const iconsDir = join(root, "crates/majik-app/assets/icons");
const manifest = JSON.parse(readFileSync(join(root, "packaging/icons.json"), "utf8"));
delete manifest["//"];

const packageDir = process.env.HUGEICONS_DIR ? resolve(process.env.HUGEICONS_DIR) : fetchPackage();
const installed = JSON.parse(readFileSync(join(packageDir, "package.json"), "utf8")).version;
if (installed !== VERSION) {
  throw new Error(`expected @hugeicons/core-free-icons ${VERSION}, found ${installed} at ${packageDir}`);
}

const kebab = (name) => name.replace(/[A-Z]/g, (c, i) => (i ? "-" : "") + c.toLowerCase());
const escape = (value) => String(value).replace(/&/g, "&amp;").replace(/"/g, "&quot;");

for (const [name, entry] of Object.entries(manifest)) {
  const { icon, fill = false, rotate = 0 } = typeof entry === "string" ? { icon: entry } : entry;
  const modulePath = join(packageDir, "dist/esm", `${icon}Icon.js`);
  let elements;
  try {
    ({ default: elements } = await import(pathToFileURL(modulePath).href));
  } catch (error) {
    throw new Error(`${name}: no HugeIcons export ${icon} (${modulePath})`, { cause: error });
  }
  const children = elements
    .map(([tag, attributes]) => {
      const rendered = Object.entries(attributes)
        .filter(([key]) => key !== "key")
        .map(([key, value]) => [kebab(key), key === "strokeWidth" && value === NATIVE_STROKE_WIDTH ? STROKE_WIDTH : value])
        .map(([key, value]) => ` ${key}="${escape(value)}"`)
        .join("");
      return `<${tag}${rendered}/>`;
    })
    .join("");
  const body = rotate ? `<g transform="rotate(${rotate} 12 12)">${children}</g>` : children;
  const svg =
    `<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24"` +
    ` fill="${fill ? "currentColor" : "none"}" stroke="currentColor" stroke-width="${STROKE_WIDTH}"` +
    ` stroke-linecap="round" stroke-linejoin="round">${body}</svg>\n`;
  writeFileSync(join(iconsDir, `${name}.svg`), svg);
}

let removed = 0;
for (const file of readdirSync(iconsDir)) {
  if (!file.endsWith(".svg")) continue;
  const name = file.slice(0, -4);
  if (!(name in manifest) && !KEEP.has(name)) {
    unlinkSync(join(iconsDir, file));
    removed += 1;
  }
}
console.log(`wrote ${Object.keys(manifest).length} icons from HugeIcons ${VERSION}, removed ${removed} stale files`);

function fetchPackage() {
  const dir = mkdtempSync(join(tmpdir(), "hugeicons-"));
  process.on("exit", () => rmSync(dir, { recursive: true, force: true }));
  execFileSync("npm", ["pack", `@hugeicons/core-free-icons@${VERSION}`, "--pack-destination", dir], { stdio: "ignore" });
  const tarball = readdirSync(dir).find((file) => file.endsWith(".tgz"));
  execFileSync("tar", ["-xzf", join(dir, tarball), "-C", dir]);
  return join(dir, "package");
}
