import { execFileSync } from "node:child_process";

const status = execFileSync(
  "git",
  ["status", "--porcelain=v1", "--", "gen"],
  { encoding: "utf8" },
);

if (status.trim() !== "") {
  console.error("generated schema files differ from the checked-in baseline:");
  console.error(status.trimEnd());
  process.exit(1);
}
