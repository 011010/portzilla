#!/usr/bin/env node

const { copyFileSync, createWriteStream, existsSync, mkdirSync, chmodSync, rmSync, readFileSync } = require("node:fs");
const { tmpdir } = require("node:os");
const { join } = require("node:path");
const { spawnSync } = require("node:child_process");
const https = require("node:https");

const pkg = require("../package.json");

const repo = "011010/portzilla";
const version = process.env.PORTZILLA_VERSION || `v${pkg.version}`;
const binName = process.platform === "win32" ? "portzilla.exe" : "portzilla";
const vendorDir = join(__dirname, "vendor");
const vendorBinary = join(vendorDir, binName);

function targetTriple() {
  const arch = process.arch === "x64" ? "x86_64" : process.arch === "arm64" ? "aarch64" : null;
  const os =
    process.platform === "linux"
      ? "unknown-linux-gnu"
      : process.platform === "darwin"
        ? "apple-darwin"
        : process.platform === "win32"
          ? "pc-windows-msvc"
          : null;

  if (!arch || !os) return null;
  return `${arch}-${os}`;
}

function download(url, destination) {
  return new Promise((resolve, reject) => {
    const request = https.get(url, (response) => {
      if ([301, 302, 303, 307, 308].includes(response.statusCode)) {
        response.resume();
        download(response.headers.location, destination).then(resolve, reject);
        return;
      }

      if (response.statusCode !== 200) {
        response.resume();
        reject(new Error(`HTTP ${response.statusCode}`));
        return;
      }

      const file = createWriteStream(destination);
      response.pipe(file);
      file.on("finish", () => file.close(resolve));
      file.on("error", reject);
    });

    request.on("error", reject);
  });
}

function run(command, args, options = {}) {
  return spawnSync(command, args, { stdio: "inherit", ...options });
}

function verifyChecksum(checksumPath, archivePath) {
  const expected = readFileSync(checksumPath, "utf8").trim().split(/\s+/)[0].toLowerCase();
  const actual = require("node:crypto")
    .createHash("sha256")
    .update(readFileSync(archivePath))
    .digest("hex");
  if (!/^[a-f0-9]{64}$/.test(expected) || expected !== actual) {
    throw new Error("release checksum mismatch");
  }
}

function installWithCargo() {
  const cargo = run("cargo", ["--version"], { stdio: "ignore" });
  if (cargo.status !== 0) {
    console.warn("portzilla npm install: no release asset found and cargo is not installed.");
    console.warn("Install Rust from https://rustup.rs, or install via: cargo install portzilla");
    process.exit(1);
  }

  const cargoRoot = join(__dirname, "cargo-root");
  rmSync(cargoRoot, { recursive: true, force: true });

  const cargoArgs = ["install", "portzilla", "--root", cargoRoot];
  if (version !== "latest") cargoArgs.push("--version", version.replace(/^v/, ""));

  const result = run("cargo", cargoArgs);
  if (result.status !== 0) process.exit(result.status ?? 1);

  const cargoBinary = join(cargoRoot, "bin", binName);
  if (!existsSync(cargoBinary)) {
    console.error(`portzilla npm install: cargo succeeded but did not create ${cargoBinary}`);
    process.exit(1);
  }

  mkdirSync(vendorDir, { recursive: true });
  copyFileSync(cargoBinary, vendorBinary);
  if (process.platform !== "win32") chmodSync(vendorBinary, 0o755);
  rmSync(cargoRoot, { recursive: true, force: true });
}

async function main() {
  const triple = targetTriple();
  if (!triple) {
    console.warn(`portzilla npm install: unsupported platform ${process.platform}/${process.arch}; falling back to cargo.`);
    installWithCargo();
    return;
  }

  const ext = process.platform === "win32" ? "zip" : "tar.gz";
  const archive = join(tmpdir(), `portzilla-${process.pid}.${ext}`);
  const checksum = `${archive}.sha256`;
  const tag = version === "latest" ? "latest" : version.startsWith("v") ? version : `v${version}`;
  const url =
    tag === "latest"
      ? `https://github.com/${repo}/releases/latest/download/portzilla-${triple}.${ext}`
      : `https://github.com/${repo}/releases/download/${tag}/portzilla-${triple}.${ext}`;

  try {
    await download(url, archive);
    await download(`${url}.sha256`, checksum);
    verifyChecksum(checksum, archive);
    mkdirSync(vendorDir, { recursive: true });

    if (process.platform === "win32") {
      const result = run("powershell", [
        "-NoProfile",
        "-Command",
        `Expand-Archive -LiteralPath '${archive.replace(/'/g, "''")}' -DestinationPath '${vendorDir.replace(/'/g, "''")}' -Force`,
      ]);
      if (result.status !== 0) throw new Error("failed to extract zip archive");
    } else {
      const result = run("tar", ["-xzf", archive, "-C", vendorDir]);
      if (result.status !== 0) throw new Error("failed to extract tar archive");
      chmodSync(vendorBinary, 0o755);
    }

    rmSync(archive, { force: true });
    rmSync(checksum, { force: true });
    if (!existsSync(vendorBinary)) throw new Error(`missing extracted binary: ${vendorBinary}`);
  } catch (error) {
    rmSync(archive, { force: true });
    rmSync(checksum, { force: true });
    console.warn(`portzilla npm install: ${error.message}; falling back to cargo.`);
    installWithCargo();
  }
}

main();
