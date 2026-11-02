const assert = require('node:assert/strict');
const { classify, changedPaths } = require('./ci-scope.cjs');
for (const [paths, expected] of [
  [['docs/index.html'], 'full'],
  [['docs/pl/guide/security.md', 'docs/assets/locale.css'], 'presentation'],
  [['loomem-dashboard/src/index.css'], 'presentation'],
  [['README.md', 'loomem-dashboard/src/index.css'], 'presentation'],
  [['docs/index.html', 'config.toml'], 'full'],
  [['docs/index.html', 'loomem-core/src/intent_log/recovery.rs'], 'full'],
  [['docs/index.html', 'loomem-server/src/auth.rs'], 'full'],
  [['loomem-dashboard/src/components/AuthGate.jsx'], 'full'],
  [['loomem-dashboard/src/lib/api.js'], 'full'],
  [['Dockerfile'], 'full'],
  [['.github/workflows/ci.yml'], 'full'],
  [['docs/assets/locale.js'], 'full'],
  [['docs/unknown.sh'], 'full'],
  [[], 'full'],
]) assert.equal(classify(paths), expected, JSON.stringify(paths));
assert.throws(() => changedPaths('0'.repeat(40), '1'.repeat(40)));
assert.throws(() => changedPaths('invalid', '1'.repeat(40)));
assert.throws(() => changedPaths('1'.repeat(40), '2'.repeat(40)));
const fs = require('node:fs'), os = require('node:os'), path = require('node:path');
const { execFileSync } = require('node:child_process');
const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ci-scope-'));
const git = (...args) => execFileSync('git', args, { cwd: dir, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }).trim();
try {
  git('init', '-q'); git('config', 'user.name', 'CI test'); git('config', 'user.email', 'ci@example.invalid');
  fs.mkdirSync(path.join(dir, 'docs'));
  fs.writeFileSync(path.join(dir, 'docs', 'a.md'), 'a');
  git('add', '.'); git('commit', '-qm', 'base');
  const base = git('rev-parse', 'HEAD');
  fs.renameSync(path.join(dir, 'docs', 'a.md'), path.join(dir, 'config.toml'));
  fs.writeFileSync(path.join(dir, 'docs', 'b.md'), 'b');
  git('add', '-A'); git('commit', '-qm', 'rename and add');
  const head = git('rev-parse', 'HEAD');
  const old = process.cwd(); process.chdir(dir);
  try {
    const paths = changedPaths(base, head);
    assert.deepEqual(paths.sort(), ['config.toml', 'docs/a.md', 'docs/b.md']);
    assert.equal(classify(paths), 'full');
  } finally { process.chdir(old); }
} finally { fs.rmSync(dir, { recursive: true, force: true }); }
console.log('CI scope tests passed');
