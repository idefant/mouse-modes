import assert from 'node:assert/strict';
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
