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
    try {
      const result = await predicate();
      if (result) return result;
    } catch (error) {
      // Chrome can replace the execution context while a navigation is in flight.
      if (!/Execution context was destroyed|Cannot find context with specified id|Cannot find default execution context/i.test(error.message)) throw error;
    }
    await sleep(100);
  }
  throw Error(`Timed out: ${typeof message === 'function' ? message() : message}`);
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
    const { resolve, reject, timer } = pending.get(message.id);
    clearTimeout(timer);
    pending.delete(message.id);
    if (message.error) reject(Error(message.error.message)); else resolve(message.result);
  };
  socket.onclose = () => {
    for (const { reject, timer } of pending.values()) {
      clearTimeout(timer);
      reject(Error('Chrome DevTools connection closed'));
    }
    pending.clear();
  };
  function send(method, params = {}) {
    return new Promise((resolve, reject) => {
      const id = ++nextId;
      const timer = setTimeout(() => {
        pending.delete(id);
        reject(Error(`Chrome DevTools command timed out: ${method}`));
      }, 10000);
      pending.set(id, { resolve, reject, timer });
      try { socket.send(JSON.stringify({ id, method, params })); }
      catch (error) { clearTimeout(timer); pending.delete(id); reject(error); }
    });
  }
  async function evaluate(expression) {
    const result = await send('Runtime.evaluate', { expression, returnByValue: true });
    if (result.exceptionDetails) {
      // `text` is just "Uncaught"; the actual exception lives in `exception.description`.
      const { text, exception } = result.exceptionDetails;
      throw Error(exception?.description || text);
    }
    return result.result.value;
  }
  async function waitPage(pathname, language) {
    let lastError;
    await until(async () => {
      try {
        const state = await evaluate('({url: location.href, lang: document.documentElement?.lang, ready: document.readyState})');
        return state.ready === 'complete' && new URL(state.url).pathname === pathname && state.lang === language;
      } catch (error) {
        // A document that is still being swapped in by a navigation can throw while
        // probed; keep polling and report the last error only if the page never settles.
        lastError = error;
        return false;
      }
    }, () => `page ${pathname} (${language})${lastError ? `; last error: ${lastError.message}` : ''}`);
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
  try {
    fs.rmSync(profile, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  } catch (error) {
    // Chrome helper processes can still be flushing the profile after the browser exits;
    // a leftover temp directory must not turn a passing run into a failure.
    console.warn(`Could not remove Chrome profile ${profile}: ${error.message}`);
  }
});
