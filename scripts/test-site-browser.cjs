const assert = require('node:assert/strict');
const { spawn } = require('node:child_process');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const profile = fs.mkdtempSync(path.join(os.tmpdir(), 'loomem-browser-'));
const browser = spawn(process.env.CHROME_BIN || 'google-chrome', [
  '--headless', '--no-sandbox', '--disable-gpu', '--disable-dev-shm-usage',
  '--remote-debugging-port=0', `--user-data-dir=${profile}`, '--window-size=1440,900', 'about:blank',
], { stdio: 'ignore' });
let socket;
let browserError;
browser.on('error', error => { browserError = error; });
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
async function until(predicate, message) {
  const deadline = Date.now() + 15000;
  while (Date.now() < deadline) {
    if (browserError) throw browserError;
    if (browser.exitCode !== null) throw Error(`Chrome exited with ${browser.exitCode}`);
    const result = await predicate();
    if (result) return result;
    await sleep(100);
  }
  throw Error(`Timed out: ${message}`);
}
async function main() {
  const activePort = path.join(profile, 'DevToolsActivePort');
  const port = await until(() => fs.existsSync(activePort) && fs.readFileSync(activePort, 'utf8').split('\n')[0], 'Chrome startup');
  const tabs = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
  socket = new WebSocket(tabs.find(tab => tab.type === 'page').webSocketDebuggerUrl);
  await new Promise((resolve, reject) => { socket.onopen = resolve; socket.onerror = reject; });
  let nextId = 0;
  const pending = new Map();
  socket.onmessage = ({ data }) => {
    const message = JSON.parse(data);
    if (!message.id || !pending.has(message.id)) return;
    const { resolve, reject } = pending.get(message.id);
    pending.delete(message.id);
    if (message.error) reject(Error(message.error.message)); else resolve(message.result);
  };
  function send(method, params = {}) {
    return new Promise((resolve, reject) => {
      const id = ++nextId;
      pending.set(id, { resolve, reject });
      socket.send(JSON.stringify({ id, method, params }));
    });
  }
  async function evaluate(expression) {
    const result = await send('Runtime.evaluate', { expression, returnByValue: true });
    if (result.exceptionDetails) throw Error(result.exceptionDetails.text);
    return result.result.value;
  }
  async function waitPage(pathname, language) {
    await until(async () => {
      const state = await evaluate('({url: location.href, lang: document.documentElement.lang, ready: document.readyState})');
      return state.ready === 'complete' && new URL(state.url).pathname === pathname && state.lang === language;
    }, `page ${pathname} (${language})`);
  }
  async function clickLanguage(language) {
    const point = await evaluate(`(() => {
      const link = document.querySelector('[data-language="${language}"]');
      if (!link) return null;
      const rect = link.getBoundingClientRect();
      return { x: rect.x + rect.width / 2, y: rect.y + rect.height / 2, width: rect.width, height: rect.height };
    })()`);
    assert.ok(point && point.width > 0 && point.height > 0, `${language} switch is visible`);
    await send('Input.dispatchMouseEvent', { type: 'mousePressed', x: point.x, y: point.y, button: 'left', clickCount: 1 });
    await send('Input.dispatchMouseEvent', { type: 'mouseReleased', x: point.x, y: point.y, button: 'left', clickCount: 1 });
  }
  await send('Page.enable');
  await send('Page.navigate', { url: `${process.env.SITE_TEST_ORIGIN}/?loomem_lang=en&source=test#section` });
  await waitPage('/', 'en');
  await clickLanguage('pl');
  await waitPage('/pl/', 'pl');
  assert.equal(await evaluate('localStorage.getItem("loomem.locale")'), 'pl');
  assert.equal(await evaluate('location.search + location.hash'), '?source=test#section');
  await send('Page.reload');
  await until(async () => await evaluate('performance.getEntriesByType("navigation")[0]?.type') === 'reload', 'Polish reload');
  await waitPage('/pl/', 'pl');
  assert.equal(await evaluate('localStorage.getItem("loomem.locale")'), 'pl');
  await clickLanguage('en');
  await waitPage('/', 'en');
  assert.equal(await evaluate('localStorage.getItem("loomem.locale")'), 'en');
  assert.equal(await evaluate('location.search + location.hash'), '?source=test#section');
  await send('Page.reload');
  await until(async () => await evaluate('performance.getEntriesByType("navigation")[0]?.type') === 'reload', 'English reload');
  await waitPage('/', 'en');
  assert.equal(await evaluate('localStorage.getItem("loomem.locale")'), 'en');
  console.log('Browser language switching and preference persistence passed');
}
main().catch(error => { console.error(error); process.exitCode = 1; }).finally(async () => {
  if (socket) socket.close();
  if (browser.pid && browser.exitCode === null) {
    const exited = new Promise(resolve => browser.once('exit', resolve));
    browser.kill();
    await exited;
  }
  fs.rmSync(profile, { recursive: true, force: true });
});
