#!/usr/bin/env node

const { spawnSync } = require("node:child_process");
const { existsSync } = require("node:fs");
const { join } = require("node:path");

const exe = process.platform === "win32" ? "portzilla.exe" : "portzilla";
const localBinary = join(__dirname, "vendor", exe);

if (!existsSync(localBinary)) {
  console.error("portzilla binary is missing. Reinstall the package with: npm install -g portzilla");
  process.exit(1);
}

const result = spawnSync(localBinary, process.argv.slice(2), { stdio: "inherit" });

if (result.error) {
  console.error(result.error.message);
  process.exit(1);
}

if (typeof result.status === "number") {
  process.exit(result.status);
}

process.exit(1);
