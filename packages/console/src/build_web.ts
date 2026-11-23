import * as fs from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { renderConsoleCss, renderConsoleHtml, renderConsoleJs } from "./app.js";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
export const defaultDistWebDir = path.resolve(__dirname, "../dist-web");

export function writeOfflineWebBundle(outDir = defaultDistWebDir): string[] {
  fs.mkdirSync(outDir, { recursive: true });
  const files: [string, string][] = [
    ["index.html", renderConsoleHtml()],
    ["styles.css", renderConsoleCss()],
    ["app.js", renderConsoleJs()],
  ];
  const written: string[] = [];
  for (const [name, content] of files) {
    const p = path.join(outDir, name);
    fs.writeFileSync(p, content, "utf8");
    written.push(p);
  }
  return written;
}

// Run when invoked directly via `node dist/build_web.js`
if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  writeOfflineWebBundle();
}
