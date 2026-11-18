import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { ICONS, iconPath } from "../../assets/herdr-app/mods/tchelvis/main.js";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(__dirname, "../..");
const mainJsPath = path.join(repoRoot, "assets/herdr-app/mods/tchelvis/main.js");

test("main.js has no emoji left", () => {
  const src = fs.readFileSync(mainJsPath, "utf8");
  // The single alert-detection literal `line.includes("⚠")` stays per spec;
  // typographic symbols `▶` and `↗` stay as text per spec even though Unicode
  // marks them Extended_Pictographic.
  const withoutAllowed = src
    .replace('line.includes("⚠")', 'line.includes("")')
    .replace(/[▶↗]/g, "");
  const matches = withoutAllowed.match(/\p{Extended_Pictographic}/gu) ?? [];
  assert.deepEqual(matches, []);
});

test("every icon in the map has a file", () => {
  const keys = Object.keys(ICONS);
  assert.equal(keys.length, 16);
  for (const key of keys) {
    const rel = iconPath(key);
    assert.ok(rel, `iconPath(${key}) should return a path`);
    const fullPath = path.join(repoRoot, "assets/herdr-app", rel);
    assert.ok(fs.existsSync(fullPath), `missing icon file: ${fullPath}`);
  }
});

test("iconPath returns null for unknown names", () => {
  assert.equal(iconPath("nonexistent"), null);
  assert.equal(iconPath(undefined), null);
  assert.equal(iconPath(""), null);
});
