// Connect — the onboarding surface of a hosted single-user instance: "how do
// I point my AI tools at this Loomem". Everything on this screen is real: the
// endpoint is computed from the page origin, the token is the one that just
// unlocked the dashboard, the recipes are parameterized with both, and the
// status light is a live probe against the engine — never a mock.

import { useCallback, useEffect, useState } from "react";
import { getMemoryStatus } from "../lib/api";

const CLIENTS = ["Claude", "Claude Code", "ChatGPT", "Cursor"];

function maskToken(token) {
  if (!token) return null;
  if (token.length <= 8) return "••••••••";
  return `${token.slice(0, 3)}${"•".repeat(20)}${token.slice(-4)}`;
}

// Build the per-client recipe text from the REAL endpoint + token. The copy
// button copies exactly this — pasted into a client, it must just work.
function buildRecipe(client, endpoint, token) {
  const auth = token ? `Authorization: Bearer ${token}` : null;
  switch (client) {
    case "Claude":
      return [
        "// claude_desktop_config.json",
        JSON.stringify(
          {
            mcpServers: {
              loomem: {
                command: "npx",
                args: [
                  "-y",
                  "mcp-remote",
                  endpoint,
                  ...(auth ? ["--header", auth] : []),
                ],
              },
            },
          },
          null,
          2,
        ),
      ].join("\n");
    case "Claude Code":
      return [
        "# one line — Claude Code speaks HTTP natively",
        `claude mcp add --transport http loomem ${endpoint}${auth ? ` \\\n  --header "${auth}"` : ""}`,
      ].join("\n");
    case "ChatGPT":
      return [
        "// ChatGPT → Settings → Connectors → Add custom",
        `URL   ${endpoint}`,
        "Auth  OAuth (developer mode on)",
      ].join("\n");
    case "Cursor":
      return [
        "// ~/.cursor/mcp.json",
        JSON.stringify(
          {
            mcpServers: {
              loomem: {
                url: endpoint,
                ...(auth ? { headers: { Authorization: `Bearer ${token}` } } : {}),
              },
            },
          },
          null,
          2,
        ),
      ].join("\n");
    default:
      return "";
  }
}

function CopyButton({ text, label = "Copy" }) {
  const [copied, setCopied] = useState(false);
  const copy = () => {
    navigator.clipboard?.writeText(text).then(
      () => {
        setCopied(true);
        window.setTimeout(() => setCopied(false), 1200);
      },
      () => {},
    );
  };
  return (
    <button
      type="button"
      onClick={copy}
      className="font-mono text-[12px] font-semibold text-[#0F5491] bg-[var(--surface-cool)] border border-[#9CD0F7] rounded-lg px-2.5 py-1.5 shrink-0 hover:-translate-y-[1px] transition-transform"
    >
      {copied ? "Copied ✓" : label}
    </button>
  );
}

function Field({ label, value, masked, children }) {
  return (
    <div>
      <div className="font-mono text-[11px] font-semibold uppercase tracking-[0.06em] text-[var(--text-subtle)]">{label}</div>
      <div className="flex items-center justify-between gap-3 bg-[var(--row-hover)] border border-[var(--border)] rounded-xl px-3.5 py-3 mt-2">
        <code className="font-mono text-[13px] text-[var(--ink-800,#2E2820)] overflow-hidden text-ellipsis whitespace-nowrap">
          {masked ?? value}
        </code>
        <span className="flex items-center gap-1.5">{children}</span>
      </div>
    </div>
  );
}

// Live status card — a REAL authorized probe against /v1/status: green
// "connected" when the engine answers, red "unreachable" when it doesn't.
function LiveStatus() {
  const [state, setState] = useState("checking"); // checking | up | down
  const [detail, setDetail] = useState("");
  const [checking, setChecking] = useState(false);

  const probe = useCallback(async () => {
    setChecking(true);
    try {
      const status = await getMemoryStatus();
      setState("up");
      const chunks = status?.total_chunks ?? status?.chunks ?? null;
      setDetail(
        chunks != null
          ? `The engine answered with ${Number(chunks).toLocaleString()} memories on board.`
          : "The engine answered. Your clients can connect with this token.",
      );
    } catch (err) {
      setState("down");
      setDetail(err?.message ? `Probe failed: ${err.message}` : "Probe failed.");
    } finally {
      setChecking(false);
    }
  }, []);

  useEffect(() => {
    probe();
    const t = window.setInterval(probe, 20000);
    return () => window.clearInterval(t);
  }, [probe]);

  const up = state === "up";
  const down = state === "down";
  return (
    <div
      className={`flex items-center gap-3 mt-6 border rounded-2xl px-4 py-3.5 ${
        up
          ? "bg-[var(--success-bg)] border-[#BBE3CD]"
          : down
            ? "bg-[var(--danger-bg)] border-[var(--danger)]/40"
            : "bg-[var(--surface-warm)] border-[#FBD68A]"
      }`}
      role="status"
    >
      <span
        className={`w-[11px] h-[11px] rounded-full shrink-0 ${
          up ? "bg-[var(--success)]" : down ? "bg-[var(--danger)]" : "bg-[#EE9913] animate-pulse-slow"
        }`}
      />
      <div className="min-w-0">
        <div className="font-semibold text-[14px]">
          {up ? "Connected — this instance is live" : down ? "Unreachable" : "Checking the engine…"}
        </div>
        <div className="text-[13px] text-[var(--text-muted)] truncate">{detail}</div>
      </div>
      <button
        type="button"
        onClick={probe}
        disabled={checking}
        className="ml-auto shrink-0 rounded-full text-[13px] font-semibold text-white px-4 py-2 hover:-translate-y-[1px] transition-transform disabled:opacity-60"
        style={{ backgroundImage: "var(--weave-vivid)" }}
      >
        {checking ? "Checking…" : "Check now"}
      </button>
    </div>
  );
}

const STEPS = [
  ["Paste the config", "Drop the endpoint + token into your MCP client's config."],
  ["Restart the client", "Claude, ChatGPT, Cursor — whatever you use. It picks up Loomem on boot."],
  ["Say “remember…”", "Teach it one thing. Open a different tool — it's already there."],
];

export default function ConnectPage() {
  const [client, setClient] = useState("Claude");
  const [revealed, setRevealed] = useState(false);

  const endpoint = `${window.location.origin}/mcp`;
  const token = localStorage.getItem("loomem_api_key");
  const recipe = buildRecipe(client, endpoint, token);
  // The rendered recipe masks the token; the copied recipe carries the real
  // one (a masked config would not work when pasted).
  const recipeShown = token && !revealed ? recipe.replaceAll(token, maskToken(token)) : recipe;

  return (
    <div className="h-full w-full overflow-y-auto bg-[var(--bg)]">
      <div className="max-w-[1080px] mx-auto px-8 py-9 pb-16">
        <div className="text-[12px] font-bold uppercase tracking-[0.1em] text-weave inline-block mb-2.5">
          Get connected
        </div>
        <h1 className="font-display text-[38px] font-medium leading-[1.08] tracking-[-0.02em] mb-2">
          Connect your{" "}
          <em className="italic text-weave" style={{ backgroundImage: "var(--weave-text)" }}>
            first tool
          </em>
        </h1>
        <p className="text-[16px] text-[var(--text-muted)] max-w-[64ch] leading-[1.55]">
          Point any MCP client at your Loomem. One endpoint, one token — and your memory is shared
          by every tool you connect. Swap the model, switch the tool; your context follows.
        </p>

        <div className="grid gap-5 mt-6" style={{ gridTemplateColumns: "1.15fr 0.85fr" }}>
          <div className="bg-[var(--panel)] border border-[var(--border)] rounded-2xl p-6 shadow-[var(--shadow-sm)] space-y-5">
            <Field label="Your MCP endpoint" value={endpoint}>
              <CopyButton text={endpoint} />
            </Field>
            {token ? (
              <Field label="Access token" value={token} masked={revealed ? token : maskToken(token)}>
                <button
                  type="button"
                  onClick={() => setRevealed((r) => !r)}
                  className="font-mono text-[12px] font-semibold text-[#0F5491] bg-[var(--surface-cool)] border border-[#9CD0F7] rounded-lg px-2.5 py-1.5 shrink-0 hover:-translate-y-[1px] transition-transform"
                >
                  {revealed ? "Hide" : "Reveal"}
                </button>
                <CopyButton text={token} />
              </Field>
            ) : (
              <div className="text-[13px] text-[var(--text-muted)] bg-[var(--surface-warm)] border border-[#FBD68A] rounded-xl px-3.5 py-3">
                This instance runs without a token (local mode) — clients connect with no
                Authorization header.
              </div>
            )}
            <div className="text-[12.5px] text-[var(--text-subtle)] leading-[1.5]">
              Keep the token secret — it is the one key to this instance. Rotating it logs every
              client out until they re-auth.
            </div>
          </div>

          <div className="bg-[var(--panel)] border border-[var(--border)] rounded-2xl p-6 shadow-[var(--shadow-sm)]">
            <div className="font-mono text-[11px] font-semibold uppercase tracking-[0.06em] text-[var(--text-subtle)] mb-2.5">
              Add to your client
            </div>
            <div className="flex gap-1.5 flex-wrap mb-3" role="tablist" aria-label="MCP client">
              {CLIENTS.map((c) => (
                <button
                  key={c}
                  type="button"
                  role="tab"
                  aria-selected={client === c}
                  onClick={() => setClient(c)}
                  className={`text-[13px] font-semibold rounded-full px-3.5 py-1.5 border transition-colors ${
                    client === c
                      ? "bg-[var(--ink-800,#2E2820)] text-[#F7F2E9] border-[var(--ink-800,#2E2820)]"
                      : "bg-[var(--panel)] text-[var(--text-muted)] border-[var(--border)] hover:text-[var(--text)]"
                  }`}
                >
                  {c}
                </button>
              ))}
            </div>
            <div className="codebox">
              <pre className="font-mono text-[12.5px] leading-[1.65] p-5 overflow-x-auto whitespace-pre">
                {recipeShown}
              </pre>
            </div>
            <div className="mt-3 flex justify-end">
              <CopyButton text={recipe} label="Copy config" />
            </div>
          </div>
        </div>

        <LiveStatus />

        <div className="grid grid-cols-3 gap-3.5 mt-6">
          {STEPS.map(([title, desc], i) => (
            <div
              key={title}
              className="bg-[var(--panel)] border border-[var(--border)] rounded-2xl p-4 shadow-[var(--shadow-sm)]"
            >
              <div
                className="w-[30px] h-[30px] rounded-full text-white font-display font-semibold text-[15px] flex items-center justify-center mb-2.5"
                style={{ backgroundImage: "var(--weave-vivid)" }}
              >
                {i + 1}
              </div>
              <div className="font-semibold text-[14.5px] mb-0.5">{title}</div>
              <div className="text-[13px] text-[var(--text-subtle)] leading-[1.45]">{desc}</div>
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}
