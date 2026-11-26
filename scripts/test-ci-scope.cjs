const assert = require('node:assert/strict');
const { JOBS, classify, changedPaths, gate } = require('./ci-scope.cjs');
const selected = result => JOBS.filter(job => result.jobs[job]).join('+') || 'none';
const ALL = JOBS.join('+'), RUST = 'rust+bench', DEPS = 'rust+audit+bench';
for (const [paths, expected] of [
  // presentation
  [['README.md'], 'none'],
  [['CHANGELOG.md', 'CLAUDE.md', 'AGENTS.md'], 'none'],
  [['docs/index.html'], 'website'],
  [['docs/pl/guide/security.md', 'docs/assets/locale.css', 'docs/assets/locale.js'], 'website'],
  [['docs/unknown.sh'], 'website'],
  // dashboard
  [['loomem-dashboard/src/index.css'], 'dashboard'],
  [['loomem-dashboard/src/components/AuthGate.jsx', 'loomem-dashboard/package-lock.json'], 'dashboard'],
  [['loomem-dashboard/src/lib/api.js', 'loomem-server/src/spa.rs'], 'rust+dashboard+bench'],
  [['loomem-dashboard/dist/.gitkeep'], 'rust+dashboard+bench'],
  [['loomem-dashboard/.gitignore'], 'rust+dashboard+bench'],
  // bench
  [['bench/longmemeval/paid_runner.py'], 'bench'],
  [['bench/longmemeval/README.md', 'bench/new/x.py'], 'bench'],
  // rust (bench tests read config.toml and server fixtures)
  [['loomem-core/src/intent_log/recovery.rs'], RUST],
  [['loomem-server/src/auth.rs', 'loomem-cli/src/main.rs', 'loomem-migrate/src/main.rs'], RUST],
  [['config.toml'], RUST],
  [['.cargo/config.toml'], DEPS],
  [['rust-toolchain.toml'], RUST],
  [['loomem-server/src/mcp/dispatcher/ingest/extraction_receipt/fixtures/complete.json'], RUST],
  // dependencies and advisories
  [['Cargo.lock'], DEPS],
  [['Cargo.toml'], DEPS],
  [['loomem-core/Cargo.toml'], DEPS],
  [['deny.toml'], DEPS],
  [['.cargo/audit.toml'], DEPS],
  // ci / infra
  [['.github/workflows/ci.yml'], ALL],
  [['scripts/hooks/pre-commit'], ALL],
  [['scripts/test-site-locale.cjs'], ALL],
  [['Dockerfile'], ALL],
  [['install.sh'], ALL],
  // unclassified and empty diffs fail closed
  [['tools/legacy-fixture-gen/src/main.rs'], ALL],
  [['entities.toml.example'], ALL],
  [['LICENSE'], ALL],
  [['loomem-unknown/src/lib.rs'], ALL],
  [['docs/README.md', 'loomem-engine/README.md'], ALL],
  [[], ALL],
  // mixed diffs take the union
  [['README.md', 'docs/index.html'], 'website'],
  [['docs/index.html', 'config.toml'], 'rust+website+bench'],
  [['bench/longmemeval/qa.py', 'loomem-dashboard/src/App.jsx'], 'dashboard+bench'],
  [['bench/longmemeval/qa.py', 'Cargo.lock'], DEPS],
  [['README.md', 'bench/longmemeval/qa.py', '.github/workflows/ci.yml'], ALL],
]) assert.equal(selected(classify(paths)), expected, JSON.stringify(paths));

// Labels, events.
assert.equal(classify(['README.md']).scope, 'presentation');
assert.equal(classify(['bench/x.py']).scope, 'bench');
assert.equal(classify([]).scope, 'full');
assert.equal(selected(classify(['README.md'], 'push')), 'audit');
assert.equal(selected(classify(['bench/x.py'], 'push')), 'audit+bench');
assert.equal(selected(classify([], 'schedule')), 'audit');

// Gate: selected jobs must succeed, the rest must be skipped.
const env = (sel, res, extra = {}) => ({ SCOPE: 'test', SCOPE_RESULT: 'success', ...extra,
  ...Object.fromEntries(JOBS.map(job => [`SELECT_${job.toUpperCase()}`, String(sel.includes(job))])),
  ...Object.fromEntries(JOBS.map(job => [`RESULT_${job.toUpperCase()}`, res[job] || (sel.includes(job) ? 'success' : 'skipped')])) });
assert.deepEqual(gate(env(JOBS, {})).errors, []);
assert.deepEqual(gate(env([], {})).errors, []);
assert.deepEqual(gate(env(['bench'], {})).errors, []);
assert.equal(gate(env(['bench'], { bench: 'skipped' })).errors.length, 1, 'expected job skipped');
assert.equal(gate(env(['bench'], { bench: 'failure' })).errors.length, 1, 'expected job failed');
assert.equal(gate(env(['bench'], { bench: 'cancelled' })).errors.length, 1, 'expected job cancelled');
assert.equal(gate(env(['bench'], { rust: 'failure' })).errors.length, 1, 'unexpected job failed');
assert.equal(gate(env(['bench'], { rust: 'success' })).errors.length, 1, 'unexpected job ran');
assert.equal(gate(env(JOBS, {}, { SCOPE_RESULT: 'failure' })).errors.length, 1, 'scope failed');
assert.equal(gate({ ...env(JOBS, {}), SELECT_AUDIT: '' }).errors.length, 1, 'missing selection');
assert.equal(gate({ ...env([], {}), SELECT_RUST: undefined, RESULT_RUST: 'skipped' }).errors.length, 1, 'undefined selection');

assert.throws(() => changedPaths('0'.repeat(40), '1'.repeat(40)));
assert.throws(() => changedPaths('invalid', '1'.repeat(40)));
assert.throws(() => changedPaths('1'.repeat(40), '2'.repeat(40)));
const fs = require('node:fs'), os = require('node:os'), path = require('node:path');
const { execFileSync } = require('node:child_process');
const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ci-scope-'));
const git = (...args) => execFileSync('git', args, { cwd: dir, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }).trim();
const hook = () => execFileSync('node', [path.join(__dirname, 'ci-scope.cjs'), '--hook'], { cwd: dir, encoding: 'utf8' }).trim();
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
    assert.equal(selected(classify(paths)), 'rust+website+bench');
  } finally { process.chdir(old); }
  // Hook mode reads staged paths and fails closed.
  assert.equal(hook(), 'rust', 'nothing staged');
  fs.mkdirSync(path.join(dir, 'bench'));
  fs.writeFileSync(path.join(dir, 'bench', 'x.py'), 'x');
  git('add', 'bench/x.py');
  assert.equal(hook(), 'skip', 'bench only');
  fs.writeFileSync(path.join(dir, 'config.toml'), 'changed');
  assert.equal(hook(), 'skip', 'unstaged Rust change');
  git('add', 'config.toml');
  assert.equal(hook(), 'rust', 'bench and config.toml');
  git('commit', '-qm', 'staged');
  git('mv', 'docs/b.md', 'Cargo.toml');
  assert.equal(hook(), 'rust', 'move into a Rust path');
  // Output mode: unknown events and invalid SHAs fail closed.
  const output = path.join(dir, 'out');
  const run = (CI_EVENT, CI_BASE_SHA, CI_HEAD_SHA) => {
    fs.writeFileSync(output, '');
    execFileSync('node', [path.join(__dirname, 'ci-scope.cjs')], { cwd: dir, stdio: 'ignore',
      env: { ...process.env, GITHUB_OUTPUT: output, CI_EVENT, CI_BASE_SHA, CI_HEAD_SHA } });
    return fs.readFileSync(output, 'utf8').split('\n')[0];
  };
  assert.equal(run('pull_request', base, head), 'scope=rust+website+bench');
  assert.equal(run('push', base, head), 'scope=rust+audit+website+bench');
  assert.equal(run('schedule', '', ''), 'scope=audit');
  assert.equal(run('workflow_dispatch', base, head), 'scope=full');
  assert.equal(run('pull_request', '0'.repeat(40), head), 'scope=full');
} finally { fs.rmSync(dir, { recursive: true, force: true }); }
console.log('CI scope tests passed');
