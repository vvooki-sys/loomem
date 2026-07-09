// Cycle/171 — the loomem.ai hero mark, corrected to match the LIVE landing
// (https://loomem.ai, source: loomem repo docs/index.html @ origin/main).
// The /170 version ported an older hero (static ring + drifting context
// graph) from a stale local checkout; the deployed hero is the ring mark
// exploded into three ramp-C gradient rings spinning in 3D on different
// axes: outer r=77 tilts around (1,.35,0) in 24s, middle r=56 counter-spins
// around (.3,1,.1) in 18s, inner r=36 rotates flat in 12s. Pure SVG + CSS
// (keyframes m3a/b/c in index.css) — no JS animation loop at all.
// prefers-reduced-motion: rings freeze at the landing's static composition
// (-18° / 150° / 40°, also in index.css).

const RAMP = [
  ["0", "#EE9913"],
  ["0.28", "#F7756F"],
  ["0.52", "#B86ED2"],
  ["0.74", "#667CE6"],
  ["1", "#1684DC"],
];

function RampDefs({ id }) {
  return (
    <defs>
      <linearGradient id={id} x1="0.05" y1="0" x2="0.95" y2="1">
        {RAMP.map(([o, c]) => (
          <stop key={o} offset={o} stopColor={c} />
        ))}
      </linearGradient>
    </defs>
  );
}

export default function LoginHeroRing({ size = 180 }) {
  return (
    <div
      className="mark3d"
      style={{ width: size, height: size }}
      aria-hidden="true"
      data-testid="login-hero-ring"
    >
      <svg className="m3 m3-r1" viewBox="0 0 200 200" fill="none">
        <RampDefs id="loginHero3d1" />
        <circle cx="100" cy="100" r="77" stroke="url(#loginHero3d1)" strokeWidth="13" strokeDasharray="404 80" strokeLinecap="round" />
      </svg>
      <svg className="m3 m3-r2" viewBox="0 0 200 200" fill="none">
        <RampDefs id="loginHero3d2" />
        <circle cx="100" cy="100" r="56" stroke="url(#loginHero3d2)" strokeWidth="13" strokeDasharray="294 62" strokeLinecap="round" />
      </svg>
      <svg className="m3 m3-r3" viewBox="0 0 200 200" fill="none">
        <RampDefs id="loginHero3d3" />
        <circle cx="100" cy="100" r="36" stroke="url(#loginHero3d3)" strokeWidth="12" strokeDasharray="170 56" strokeLinecap="round" />
      </svg>
    </div>
  );
}
