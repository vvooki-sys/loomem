# loomem-dashboard

The hosted dashboard for a Loomem instance — a React SPA (Vite + Tailwind)
embedded into the `loomem-server` binary at compile time via `rust-embed`.
Every instance ships it the same way: `https://<instance>/` serves the
dashboard, `/mcp` and `/v1/*` stay the engine's API surface.

Screens (v1): **Connect** (MCP endpoint + token + per-client recipes + live
status), **Memory** (browse / search / edit / delete / version history /
entities), **Settings**.

## Development

```bash
npm install
npm run dev        # http://localhost:3031, proxies API calls to :3030
npm test           # vitest
npm run lint       # eslint
```

Run `loomem-server` locally on port 3030 for live data.

## Production build

```bash
npm run build      # writes dist/
cargo build -p loomem-server   # embeds dist/ into the binary
```

Build order matters: the front end first, then cargo. CI and `release.yml`
do this automatically. With an empty `dist/` (fresh clone) the server still
compiles; `/` then answers an honest 404 until the SPA is built.

## Conventions

- Fonts are self-hosted (`@fontsource-variable`) — no Google Fonts request,
  deterministic rendering offline and in headless environments.
- Design tokens in `src/index.css` follow Loomem Design System v2 (ramp-C
  gradients only, two-tone wordmark, no black buttons, warm ink surfaces).
- No mock data in the production path: a failed endpoint renders an error
  state, never fabricated zeros.
