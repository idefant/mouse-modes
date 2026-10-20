import process from 'node:process';
import { URL } from 'node:url';
import fs from 'node:fs';
import { parseVersion, extractChangelog } from './release-utils.mjs';

const version = parseVersion(process.argv[2]);
const content = fs.readFileSync(new URL('../CHANGELOG.md', import.meta.url), 'utf8');
process.stdout.write(extractChangelog(content, version) + '\n');
