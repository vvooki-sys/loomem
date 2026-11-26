// Select CI jobs from the changed paths. Single source for ci.yml, the CI gate
// and scripts/hooks/pre-commit. Fail closed: a path no rule claims, an empty
// diff or any classification error selects every job.
const { execFileSync } = require('node:child_process');

const JOBS = ['rust', 'audit', 'dashboard', 'website', 'bench'];
const CRATES = 'loomem-(core|server|cli|migrate)';
// The first matching rule wins, so each rule lists every job its paths can
// affect. Bench tests read config.toml and loomem-server fixtures, so Rust
// paths select the bench job too.
const RULES = [
  [/^(\.github|scripts)\/|^(Dockerfile|\.dockerignore|install\.sh)$/, JOBS],
  [new RegExp(`^(${CRATES}/)?Cargo\\.(toml|lock)$|^deny\\.toml$|^\\.cargo/audit\\.toml$`), ['rust', 'audit', 'bench']],
  [new RegExp(`^${CRATES}/|^\\.cargo/|^config\\.toml$|^(rust-toolchain|\\.?rustfmt|\\.?clippy)(\\.toml)?$`), ['rust', 'bench']],
  [/^loomem-dashboard\//, ['dashboard']],
  [/^bench\//, ['bench']],
  [/^docs\//, ['website']],
  [/^[A-Z_]+\.md$/, []],
];

function jobsFor(path) {
  const rule = RULES.find(([pattern]) => pattern.test(path));
  return rule ? rule[1] : JOBS;
}
function label(jobs) {
  if (JOBS.every(job => jobs[job])) return 'full';
  const selected = JOBS.filter(job => jobs[job]);
  return selected.length ? selected.join('+') : 'presentation';
}
// event: 'pull_request', 'push' (to main) or 'schedule'.
function classify(paths, event = 'pull_request') {
  const jobs = Object.fromEntries(JOBS.map(job => [job, false]));
  if (event === 'schedule') jobs.audit = true;
  else for (const job of paths.length ? paths.flatMap(jobsFor) : JOBS) jobs[job] = true;
  // New advisories appear without any change here, so main is always audited.
  if (event === 'push') jobs.audit = true;
  return { scope: label(jobs), jobs };
}
function gitPaths(args) {
  // --no-renames records both sides, including moves across a boundary.
  const output = execFileSync('git', ['diff', '--no-renames', '--name-only', '-z', ...args, '--'], { encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 });
  return output ? output.slice(0, -1).split('\0') : [];
}
function changedPaths(base, head) {
  if (!/^[0-9a-f]{40}$/.test(base || '') || /^0+$/.test(base)) throw Error('Invalid base SHA');
  if (!/^[0-9a-f]{40}$/.test(head || '')) throw Error('Invalid head SHA');
  return gitPaths([base, head]);
}
// A selected job must succeed; every other job must be skipped.
function gateErrors(selected, results) {
  return JOBS.flatMap(job => {
    if (!['true', 'false'].includes(selected[job])) return [`${job}: invalid selection '${selected[job]}'`];
    const want = selected[job] === 'true' ? 'success' : 'skipped';
    return results[job] === want ? [] : [`${job}: expected ${want}, got '${results[job]}'`];
  });
}
function gate(env) {
  const selected = Object.fromEntries(JOBS.map(job => [job, env[`SELECT_${job.toUpperCase()}`]]));
  const results = Object.fromEntries(JOBS.map(job => [job, env[`RESULT_${job.toUpperCase()}`]]));
  const errors = env.SCOPE_RESULT === 'success' ? gateErrors(selected, results) : [`scope: job result '${env.SCOPE_RESULT}'`];
  const summary = [`CI scope: ${env.SCOPE}`, '', '| Job | Selected | Result |', '| --- | --- | --- |',
    ...JOBS.map(job => `| ${job} | ${selected[job]} | ${results[job]} |`)].join('\n');
  return { errors, summary };
}

function main(argv, env) {
  const fs = require('node:fs');
  if (argv[0] === '--hook') {
    // Prints "skip" only when no staged path can affect fmt or clippy.
    let rust = true;
    try { rust = classify(gitPaths(['--cached'])).jobs.rust; } catch { rust = true; }
    console.log(rust ? 'rust' : 'skip');
    return 0;
  }
  if (argv[0] === '--gate') {
    const { errors, summary } = gate(env);
    if (env.GITHUB_STEP_SUMMARY) fs.appendFileSync(env.GITHUB_STEP_SUMMARY, summary + '\n');
    console.log(summary);
    for (const error of errors) console.log(`::error::${error}`);
    return errors.length ? 1 : 0;
  }
  let result = classify([]);
  try {
    const event = env.CI_EVENT;
    if (!['pull_request', 'push', 'schedule'].includes(event)) throw Error(`Unknown event '${event}'`);
    result = classify(event === 'schedule' ? [] : changedPaths(env.CI_BASE_SHA, env.CI_HEAD_SHA), event);
  } catch (error) { console.error(`Unable to classify changes; running full CI: ${error.message}`); }
  fs.appendFileSync(env.GITHUB_OUTPUT, [`scope=${result.scope}`, ...JOBS.map(job => `${job}=${result.jobs[job]}`)].join('\n') + '\n');
  console.log(`CI scope: ${result.scope}`);
  return 0;
}
if (require.main === module) process.exitCode = main(process.argv.slice(2), process.env);
module.exports = { JOBS, classify, changedPaths, gate };
