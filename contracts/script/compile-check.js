#!/usr/bin/env node
/**
 * Compile-check every Solidity file with solc-js.
 *
 * `forge build` is the canonical build (see CI), but this script lets the whole
 * contract suite be type-checked in environments where the Foundry binaries are
 * not available. Run with: `node contracts/script/compile-check.js`
 */
const fs = require("fs");
const path = require("path");
const solc = require("solc");

const ROOT = path.resolve(__dirname, "..");
const REMAPPINGS = [["forge-std/", "lib/forge-std/src/"]];

function walk(dir, out = []) {
  if (!fs.existsSync(dir)) return out;
  for (const e of fs.readdirSync(dir, {withFileTypes: true})) {
    const p = path.join(dir, e.name);
    if (e.isDirectory()) walk(p, out);
    else if (e.name.endsWith(".sol")) out.push(p);
  }
  return out;
}

function resolve(importPath, fromFile) {
  for (const [prefix, target] of REMAPPINGS) {
    if (importPath.startsWith(prefix)) {
      return path.join(ROOT, target + importPath.slice(prefix.length));
    }
  }
  if (importPath.startsWith(".")) return path.resolve(path.dirname(fromFile), importPath);
  return path.join(ROOT, importPath);
}

const sources = {};
const seen = new Set();
function add(file) {
  const abs = path.resolve(file);
  if (seen.has(abs)) return;
  seen.add(abs);
  const content = fs.readFileSync(abs, "utf8");
  sources[abs] = {content};
  for (const m of content.matchAll(/import\s+(?:\{[^}]*\}\s+from\s+)?["']([^"']+)["']/g)) {
    const dep = resolve(m[1], abs);
    if (fs.existsSync(dep)) add(dep);
    else throw new Error(`unresolved import ${m[1]} in ${abs}`);
  }
}

const entry = [...walk(path.join(ROOT, "src")), ...walk(path.join(ROOT, "test")), ...walk(path.join(ROOT, "script"))]
  .filter((f) => !f.includes("/lib/") && !f.endsWith("compile-check.js"));
entry.forEach(add);

const input = {
  language: "Solidity",
  sources,
  settings: {
    optimizer: {enabled: true, runs: 1000000},
    evmVersion: "cancun",
    outputSelection: {"*": {"*": ["abi", "evm.bytecode.object", "evm.deployedBytecode.object"]}},
  },
};

const findImports = (p) => {
  const candidates = [resolve(p, ROOT), path.join(ROOT, p), p];
  for (const c of candidates) {
    if (fs.existsSync(c) && fs.statSync(c).isFile()) return {contents: fs.readFileSync(c, "utf8")};
  }
  return {error: `not found: ${p}`};
};

const out = JSON.parse(solc.compile(JSON.stringify(input), {import: findImports}));
const errors = (out.errors || []).filter((e) => e.severity === "error");
const warnings = (out.errors || []).filter((e) => e.severity !== "error");

for (const w of warnings) console.warn(w.formattedMessage);
if (errors.length) {
  for (const e of errors) console.error(e.formattedMessage);
  process.exit(1);
}

// Emit ABIs consumed by the frontend so the dashboard can talk to the contracts.
const abiDir = path.join(ROOT, "abi");
fs.mkdirSync(abiDir, {recursive: true});
let count = 0;
for (const [file, contracts] of Object.entries(out.contracts || {})) {
  if (!file.startsWith(path.join(ROOT, "src") + path.sep)) continue;
  for (const [name, c] of Object.entries(contracts)) {
    fs.writeFileSync(path.join(abiDir, `${name}.json`), JSON.stringify(c.abi, null, 2) + "\n");
    const size = (c.evm.deployedBytecode.object.length / 2) | 0;
    if (name === "MevExecutor") {
      // The Rust simulator embeds this with include_str! and injects it into the
      // anvil fork via anvil_setCode, so it can simulate before any deployment.
      const artifactDir = path.resolve(ROOT, "..", "bot", "crates", "mev-bot", "artifacts");
      fs.mkdirSync(artifactDir, {recursive: true});
      fs.writeFileSync(path.join(artifactDir, "MevExecutor.runtime.hex"), "0x" + c.evm.deployedBytecode.object);
      fs.writeFileSync(path.join(artifactDir, "MevExecutor.creation.hex"), "0x" + c.evm.bytecode.object);
      fs.writeFileSync(path.join(artifactDir, "MevExecutor.abi.json"), JSON.stringify(c.abi, null, 2) + "\n");
    }
    console.log(`ok  ${name.padEnd(24)} runtime ${String(size).padStart(6)} bytes`);
    count++;
  }
}
console.log(`\ncompiled ${Object.keys(sources).length} sources, ${count} deployable contracts, ${warnings.length} warnings`);
