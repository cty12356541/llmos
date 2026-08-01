import { readdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

function generatedFiles(directory) {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const path = join(directory, entry.name);
    return entry.isDirectory() ? generatedFiles(path) : [path];
  });
}

for (const path of generatedFiles("gen")) {
  if (!path.endsWith(".py") && !path.endsWith(".ts")) {
    continue;
  }
  const source = readFileSync(path, "utf8");
  const normalized = `${source.replace(/[ \t]+$/gmu, "").trimEnd()}\n`;
  if (source !== normalized) {
    writeFileSync(path, normalized);
  }
}
