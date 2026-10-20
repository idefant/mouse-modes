import console from 'node:console';
import process from 'node:process';
import { URL } from 'node:url';
import fs from 'node:fs';
import { fileURLToPath } from 'node:url';
import { parseVersion } from './release-utils.mjs';

const root = fileURLToPath(new URL('../', import.meta.url));
const version = parseVersion(process.argv[2]);
for (const file of ['package.json', 'package-lock.json', 'src-tauri/tauri.conf.json']) {
  const data = JSON.parse(fs.readFileSync(root + file, 'utf8'));
  data.version = version;
  if (file === 'package-lock.json') data.packages[''].version = version;
  fs.writeFileSync(root + file, JSON.stringify(data, null, 2) + '\n');
}
for (const [file, section] of [
  ['src-tauri/Cargo.toml', /(^\[package\][\s\S]*?^version = ")[^"]+("$)/m],
  ['src-tauri/Cargo.lock', /(^\[\[package\]\]\nname = "mouse-modes"\nversion = ")[^"]+("$)/m],
]) {
  const content = fs.readFileSync(root + file, 'utf8').replaceAll('\r\n', '\n');
  if (!section.test(content)) throw new Error(`Package version missing in ${file}`);
  fs.writeFileSync(root + file, content.replace(section, `$1${version}$2`));
}
console.log(`Set application version to ${version}`);
