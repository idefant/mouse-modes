export function parseVersion(input) {
  const version = input?.replace(/^v/, '');
  const number = '(?:0|[1-9][0-9]*)';
  const identifier = '(?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)';
  if (!new RegExp(`^${number}\\.${number}\\.${number}(?:-${identifier}(?:\\.${identifier})*)?$`).test(version ?? '')) {
    throw new Error(`Invalid release version: ${input}`);
  }
  return version;
}

export function extractChangelog(content, version) {
  const sections = content.split(/^## /m).slice(1);
  const matches = sections.filter((section) => section.split('\n')[0].startsWith(`[${version}] - `));
  if (matches.length !== 1) throw new Error(`CHANGELOG must contain exactly one dated section for ${version}`);
  const notes = matches[0].slice(matches[0].indexOf('\n') + 1).trim();
  if (!/^[-*] \S/m.test(notes)) throw new Error(`CHANGELOG section for ${version} must contain release notes`);
  return notes;
}
