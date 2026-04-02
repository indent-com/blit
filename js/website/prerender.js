import { readFileSync, writeFileSync } from "node:fs";
import { resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = dirname(fileURLToPath(import.meta.url));

const { render } = await import(resolve(__dirname, "dist-ssr/entry-server.js"));
const html = render();

const indexPath = resolve(__dirname, "dist/index.html");
const template = readFileSync(indexPath, "utf-8");
const output = template.replace(
  '<div id="root"></div>',
  `<div id="root">${html}</div>`,
);
writeFileSync(indexPath, output);

console.log("Pre-rendered Landing into dist/index.html");
