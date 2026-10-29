/* Language preference is presentation-only. Static links work without JavaScript. */
(() => {
  const key = 'loomem.locale';
  const marker = 'loomem_lang';
  const valid = value => value === 'pl' || value === 'en';
  const current = document.documentElement.lang === 'pl' ? 'pl' : 'en';
  const url = new URL(location.href);
  const englishPath = path => path === '/pl' || path === '/pl/' ? '/' : path.replace(/^\/pl(?=\/)/, '');
  const counterpart = (path, locale) => {
    const en = englishPath(path);
    if (!/^\/(?:index\.html|guide(?:\/index\.html|\/|\/(?:quick-start|installation|configuration|api-reference|mcp-tools|architecture|deployment|security|backup-and-restore)(?:\.html)?)?|(?:compare|benchmarks)(?:\.html)?)?$/.test(en)) return null;
    if (locale === 'en') return en;
    return en === '/' ? '/pl/' : '/pl' + en;
  };
  let stored = null;
  try { stored = localStorage.getItem(key); } catch (_) { /* Optional storage. */ }
  const override = url.searchParams.get(marker);
  const preferred = valid(override) ? override : valid(stored) ? stored :
    (navigator.languages && navigator.languages.length ? navigator.languages : [navigator.language])
      .map(value => typeof value === 'string' ? value.toLowerCase().split('-')[0] : '')
      .find(valid) || 'en';
  const destination = counterpart(url.pathname, preferred);
  if (destination && preferred !== current) {
    url.pathname = destination;
    location.replace(url.href);
    return;
  }
  document.addEventListener('DOMContentLoaded', () => {
    document.querySelectorAll('a[href]').forEach(link => {
      if (link.hasAttribute('data-language')) return;
      const target = new URL(link.href, location.href);
      if (target.origin !== location.origin) return;
      const path = counterpart(target.pathname, current);
      if (!path) return;
      target.pathname = path;
      if (valid(override)) target.searchParams.set(marker, current);
      link.href = target.href;
    });
    document.querySelectorAll('[data-language]').forEach(link => {
      link.addEventListener('click', event => {
        if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
        const language = link.getAttribute('data-language');
        const targetPath = counterpart(location.pathname, language);
        if (!valid(language) || !targetPath) return;
        event.preventDefault();
        const next = new URL(location.href);
        next.pathname = targetPath;
        next.searchParams.delete(marker);
        try { localStorage.setItem(key, language); }
        catch (_) { next.searchParams.set(marker, language); }
        location.assign(next.href);
      });
    });
  });
})();
