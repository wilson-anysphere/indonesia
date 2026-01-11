import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(scriptDir, "..", "..", "..");
const cargoTomlPath = path.join(repoRoot, "Cargo.toml");
const packageJsonPath = path.join(repoRoot, "editors", "vscode", "package.json");
const packageLockPath = path.join(repoRoot, "editors", "vscode", "package-lock.json");

const cargoToml = fs.readFileSync(cargoTomlPath, "utf8");

// Extremely small TOML parse: we only need the workspace.package version.
const match = cargoToml.match(/^\s*version\s*=\s*\"([^\"]+)\"\s*$/m);
if (!match) {
  console.error(`Could not find [workspace.package].version in ${cargoTomlPath}`);
  process.exit(1);
}
const version = match[1];

function updateJson(filePath, mutate) {
  if (!fs.existsSync(filePath)) {
    return;
  }
  const json = JSON.parse(fs.readFileSync(filePath, "utf8"));
  const updated = mutate(json);
  if (!updated) {
    return;
  }
  fs.writeFileSync(filePath, JSON.stringify(json, null, 2) + "\n");
}

updateJson(packageJsonPath, (pkg) => {
  let changed = false;
  if (pkg.version !== version) {
    pkg.version = version;
    changed = true;
  }

  // Keep the default download tag aligned with the extension version so users
  // get the matching nova-lsp/nova-dap binaries without extra configuration.
  const releaseTagPath =
    pkg?.contributes?.configuration?.properties?.["nova.download.releaseTag"];
  if (releaseTagPath && releaseTagPath.default !== `v${version}`) {
    releaseTagPath.default = `v${version}`;
    changed = true;
  }

  return changed;
});

updateJson(packageLockPath, (lock) => {
  let changed = false;
  if (lock.version !== version) {
    lock.version = version;
    changed = true;
  }
  // npm v9+ stores the root package version under packages[""].
  if (lock.packages && lock.packages[""] && lock.packages[""].version !== version) {
    lock.packages[""].version = version;
    changed = true;
  }
  return changed;
});

console.log(`Synced VS Code extension version to ${version}`);
