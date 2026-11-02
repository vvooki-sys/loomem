// Fail closed: only known presentation paths avoid the Rust jobs.
const { execFileSync } = require('node:child_process');

function isPresentation(path) {
  return /^(README|CHANGELOG)\.md$/.test(path) ||
    /^docs\/.*\.(md|css|png|txt)$/.test(path) ||
    path === 'loomem-dashboard/src/index.css';
}
function classify(paths) {
  return paths.length > 0 && paths.every(isPresentation) ? 'presentation' : 'full';
}
function changedPaths(base, head) {
  if (!/^[0-9a-f]{40}$/.test(base || '') || /^0+$/.test(base)) throw Error('Invalid base SHA');
  if (!/^[0-9a-f]{40}$/.test(head || '')) throw Error('Invalid head SHA');
  // --no-renames records both sides, including moves across the boundary.
  const output = execFileSync('git', ['diff', '--no-renames', '--name-only', '-z', base, head, '--'], { encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 });
  return output ? output.slice(0, -1).split('\0') : [];
}
if (require.main === module) {
  let scope = 'full';
  try {
    const base = process.env.CI_BASE_SHA, head = process.env.CI_HEAD_SHA;
    const paths = changedPaths(base, head);
    scope = classify(paths);
  } catch (error) { scope = 'full'; console.error(`Unable to classify changes; running full CI: ${error.message}`); }
  require('node:fs').appendFileSync(process.env.GITHUB_OUTPUT, `scope=${scope}\n`);
  console.log(`CI scope: ${scope}`);
}
module.exports = { classify, changedPaths };
