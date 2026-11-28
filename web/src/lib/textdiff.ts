/** Line diff between what a harness sent and what the gateway forwarded. */

export type DiffOp = "ctx" | "add" | "del";

export interface DiffLine {
  op: DiffOp;
  text: string;
  /** 1-based line in the original text; absent on added lines. */
  oldNo?: number;
  /** 1-based line in the forwarded text; absent on removed lines. */
  newNo?: number;
}

export interface LineDiff {
  lines: DiffLine[];
  added: number;
  removed: number;
  /** The middle was too large to align line by line and is shown as one replacement. */
  coarse: boolean;
}

/** LCS table cells allowed before falling back to a whole-block replacement (~10 MB). */
const MAX_CELLS = 2_500_000;

function splitLines(text: string): string[] {
  if (text === "") return [];
  const lines = text.split("\n");
  if (lines.length > 1 && lines[lines.length - 1] === "") lines.pop();
  return lines;
}

/** Diffs `before` against `after` line by line (common prefix/suffix + LCS on the middle). */
export function diffLines(before: string, after: string): LineDiff {
  const a = splitLines(before);
  const b = splitLines(after);
  let head = 0;
  while (head < a.length && head < b.length && a[head] === b[head]) head += 1;
  let tail = 0;
  while (
    tail < a.length - head &&
    tail < b.length - head &&
    a[a.length - 1 - tail] === b[b.length - 1 - tail]
  ) {
    tail += 1;
  }
  const midA = a.slice(head, a.length - tail);
  const midB = b.slice(head, b.length - tail);
  const n = midA.length;
  const m = midB.length;

  const ops: DiffOp[] = [];
  let coarse = false;
  if ((n + 1) * (m + 1) > MAX_CELLS) {
    coarse = true;
    for (let i = 0; i < n; i += 1) ops.push("del");
    for (let j = 0; j < m; j += 1) ops.push("add");
  } else {
    // lcs[i][j] = LCS length of midA[i..] and midB[j..], row-major in one buffer.
    const width = m + 1;
    const lcs = new Uint32Array((n + 1) * width);
    // In-range by construction; `?? 0` only satisfies unchecked-index typing.
    const at = (k: number): number => lcs[k] ?? 0;
    for (let i = n - 1; i >= 0; i -= 1) {
      for (let j = m - 1; j >= 0; j -= 1) {
        lcs[i * width + j] =
          midA[i] === midB[j]
            ? at((i + 1) * width + j + 1) + 1
            : Math.max(at((i + 1) * width + j), at(i * width + j + 1));
      }
    }
    let i = 0;
    let j = 0;
    while (i < n || j < m) {
      if (i < n && j < m && midA[i] === midB[j]) {
        ops.push("ctx");
        i += 1;
        j += 1;
      } else if (j < m && (i === n || at(i * width + j + 1) >= at((i + 1) * width + j))) {
        ops.push("add");
        j += 1;
      } else {
        ops.push("del");
        i += 1;
      }
    }
  }

  const lines: DiffLine[] = [];
  let oldNo = 0;
  let newNo = 0;
  let added = 0;
  let removed = 0;
  const pushCtx = (text: string): void => {
    oldNo += 1;
    newNo += 1;
    lines.push({ op: "ctx", text, oldNo, newNo });
  };
  for (let k = 0; k < head; k += 1) pushCtx(a[k] ?? "");
  let ia = 0;
  let ib = 0;
  for (const op of ops) {
    if (op === "ctx") {
      pushCtx(midA[ia] ?? "");
      ia += 1;
      ib += 1;
    } else if (op === "del") {
      oldNo += 1;
      removed += 1;
      lines.push({ op, text: midA[ia] ?? "", oldNo });
      ia += 1;
    } else {
      newNo += 1;
      added += 1;
      lines.push({ op, text: midB[ib] ?? "", newNo });
      ib += 1;
    }
  }
  for (let k = a.length - tail; k < a.length; k += 1) pushCtx(a[k] ?? "");
  return { lines, added, removed, coarse };
}

export type DiffRow = { kind: "line"; line: DiffLine } | { kind: "gap"; hidden: number };

/** Keeps `context` unchanged lines around each change and folds the rest into gap rows. */
export function foldContext(lines: readonly DiffLine[], context = 3): DiffRow[] {
  const keep = new Uint8Array(lines.length);
  lines.forEach((line, idx) => {
    if (line.op === "ctx") return;
    const from = Math.max(0, idx - context);
    const to = Math.min(lines.length - 1, idx + context);
    for (let k = from; k <= to; k += 1) keep[k] = 1;
  });
  const rows: DiffRow[] = [];
  let hidden = 0;
  lines.forEach((line, idx) => {
    if (keep[idx]) {
      if (hidden > 0) rows.push({ kind: "gap", hidden });
      hidden = 0;
      rows.push({ kind: "line", line });
    } else {
      hidden += 1;
    }
  });
  if (hidden > 0) rows.push({ kind: "gap", hidden });
  return rows;
}

/** Shows invisible control characters (terminal escapes, CR) as Unicode control pictures. */
export function revealControls(text: string): string {
  // eslint-disable-next-line no-control-regex
  return text.replace(/[\u0000-\u0008\u000b-\u001f\u007f]/g, (ch) =>
    ch === "\u007f" ? "\u2421" : String.fromCharCode(0x2400 + ch.charCodeAt(0)),
  );
}
