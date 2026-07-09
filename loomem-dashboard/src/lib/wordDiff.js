// Cycle/173 S3 — word-level diff for the memory version History tab. Zero deps:
// a classic LCS over whitespace-delimited tokens, emitted as a flat list of
// { type: "eq" | "add" | "del", text } segments. `add` = present in the newer
// version only (green), `del` = present in the older version only (struck-out).
//
// Tokens keep their trailing whitespace so re-joining segments reproduces the
// original text. Complexity is O(n*m) in token counts — fine for memory chunks
// (short); callers should not feed novel-length inputs.

function tokenize(text) {
  // Split into words + their trailing whitespace so joins are lossless.
  return (text || "").match(/\S+\s*|\s+/g) || [];
}

// Longest-common-subsequence table over two token arrays.
function lcsTable(a, b) {
  const rows = a.length + 1;
  const cols = b.length + 1;
  const dp = Array.from({ length: rows }, () => new Array(cols).fill(0));
  for (let i = a.length - 1; i >= 0; i -= 1) {
    for (let j = b.length - 1; j >= 0; j -= 1) {
      dp[i][j] = a[i] === b[j] ? dp[i + 1][j + 1] + 1 : Math.max(dp[i + 1][j], dp[i][j + 1]);
    }
  }
  return dp;
}

// Diff `oldText` → `newText` at word granularity. Returns ordered segments.
export function wordDiff(oldText, newText) {
  const a = tokenize(oldText);
  const b = tokenize(newText);
  const dp = lcsTable(a, b);
  const out = [];
  let i = 0;
  let j = 0;
  const push = (type, text) => {
    const last = out[out.length - 1];
    if (last && last.type === type) last.text += text;
    else out.push({ type, text });
  };
  while (i < a.length && j < b.length) {
    if (a[i] === b[j]) {
      push("eq", a[i]);
      i += 1;
      j += 1;
    } else if (dp[i + 1][j] >= dp[i][j + 1]) {
      push("del", a[i]);
      i += 1;
    } else {
      push("add", b[j]);
      j += 1;
    }
  }
  while (i < a.length) {
    push("del", a[i]);
    i += 1;
  }
  while (j < b.length) {
    push("add", b[j]);
    j += 1;
  }
  return out;
}
