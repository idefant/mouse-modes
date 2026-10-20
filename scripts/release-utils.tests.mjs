import assert from 'node:assert/strict';
import { URL } from 'node:url';
import { test } from 'node:test';
import { parseVersion, extractChangelog } from './release-utils.mjs';

test('release versions accept stable and alpha tags and reject malformed input', () => {
  assert.equal(parseVersion('v0.1.0-alpha-1'), '0.1.0-alpha-1');
  assert.equal(parseVersion('v0.1.0'), '0.1.0');
  for (const input of ['', 'main', 'v01.1.0', 'v0.1.0-01', '0.1.0\nrun', '1.2.3.4']) {
    assert.throws(() => parseVersion(input));
  }
});

test('notes contain only the exact version and require a nonempty unique section', () => {
  const changelog = '# Changelog\n\n## [Unreleased]\n\n## [0.1.0] - 2026-10-20\n\n### Added\n\n- Stable.\n\n## [0.1.0-alpha-1] - 2026-10-20\n\n- Alpha.\n';
  assert.equal(extractChangelog(changelog, '0.1.0'), '### Added\n\n- Stable.');
  assert.equal(extractChangelog(changelog, '0.1.0-alpha-1'), '- Alpha.');
  assert.throws(() => extractChangelog(changelog, '0.2.0'));
  assert.throws(() => extractChangelog('## [0.1.0] - 2026-10-20\n\n### Added\n', '0.1.0'));
  assert.throws(() => extractChangelog(changelog + '\n## [0.1.0] - 2026-10-21\n- Duplicate.', '0.1.0'));
});

test('version synchronization supports Windows CRLF and updates all lockfiles', async () => {
  const fs = await import('node:fs');
  const os = await import('node:os');
  const path = await import('node:path');
  const { fileURLToPath } = await import('node:url');
  const { execFileSync } = await import('node:child_process');
  const root = fileURLToPath(new URL('../', import.meta.url));
  const fixture = fs.mkdtempSync(path.join(os.tmpdir(), 'mouse-modes-version-'));
  try {
    fs.mkdirSync(path.join(fixture, 'scripts'));
    fs.mkdirSync(path.join(fixture, 'src-tauri'));
    const files = ['package.json', 'package-lock.json', 'src-tauri/Cargo.toml', 'src-tauri/Cargo.lock', 'src-tauri/tauri.conf.json', 'scripts/set-version.mjs', 'scripts/release-utils.mjs'];
    for (const file of files) {
      fs.writeFileSync(path.join(fixture, file), fs.readFileSync(path.join(root, file), 'utf8').replace(/\r?\n/g, '\r\n'));
    }
    for (const version of ['0.1.0-alpha-1', '0.1.0']) {
      execFileSync('node', [path.join(fixture, 'scripts/set-version.mjs'), version]);
      for (const file of files.slice(0, 5)) {
        assert.ok(fs.readFileSync(path.join(fixture, file), 'utf8').includes(version), file);
      }
      const lock = JSON.parse(fs.readFileSync(path.join(fixture, 'package-lock.json'), 'utf8'));
      assert.equal(lock.packages[''].version, version);
    }
  } finally {
    fs.rmSync(fixture, { recursive: true, force: true });
  }
});
