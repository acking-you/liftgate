/**
 * Dependency-free syntax tinting for the handful of languages CodeMode scripts
 * and tool payloads arrive in. It is a lexer, not a parser: good enough to make
 * a script readable, cheap enough to run on every inspector open.
 */
export type CodeLanguage = "js" | "py" | "sh" | "json" | "xml" | "text";

/** `c` comment · `s` string · `n` number · `k` keyword · `f` call · `p` property/tag · `` plain */
export type TokenKind = "" | "c" | "s" | "n" | "k" | "f" | "p";

export interface Token {
  kind: TokenKind;
  text: string;
}

const JS_KEYWORDS =
  "const|let|var|function|return|if|else|for|while|do|await|async|import|from|export|new|try|catch|finally|throw|class|extends|of|in|typeof|instanceof|switch|case|break|continue|default|null|undefined|true|false|this|yield|void|delete|type|interface|declare|as";
const PY_KEYWORDS =
  "def|return|if|elif|else|for|while|import|from|as|with|try|except|finally|raise|class|lambda|pass|break|continue|in|not|and|or|is|None|True|False|async|await|yield|global|nonlocal|assert|del";
const SH_KEYWORDS =
  "if|then|else|elif|fi|for|in|do|done|while|until|case|esac|function|return|exit|export|local|set|cd|echo";

const GRAMMARS: Record<Exclude<CodeLanguage, "text">, RegExp> = {
  js: new RegExp(
    [
      "(?<c>\\/\\/[^\\n]*|\\/\\*[\\s\\S]*?\\*\\/)",
      "(?<s>\"(?:\\\\.|[^\"\\\\\\n])*\"|'(?:\\\\.|[^'\\\\\\n])*'|`(?:\\\\[\\s\\S]|[^\\\\`])*`)",
      "(?<n>\\b\\d[\\d_]*(?:\\.\\d+)?(?:[eE][+-]?\\d+)?\\b)",
      `(?<k>\\b(?:${JS_KEYWORDS})\\b)`,
      "(?<f>\\b[A-Za-z_$][\\w$]*(?=\\s*\\())",
    ].join("|"),
    "g",
  ),
  py: new RegExp(
    [
      "(?<c>#[^\\n]*)",
      "(?<s>[rbfu]{0,2}\"\"\"[\\s\\S]*?\"\"\"|[rbfu]{0,2}'''[\\s\\S]*?'''|[rbfu]{0,2}\"(?:\\\\.|[^\"\\\\\\n])*\"|[rbfu]{0,2}'(?:\\\\.|[^'\\\\\\n])*')",
      "(?<n>\\b\\d[\\d_]*(?:\\.\\d+)?(?:[eE][+-]?\\d+)?\\b)",
      `(?<k>\\b(?:${PY_KEYWORDS})\\b)`,
      "(?<f>\\b[A-Za-z_][\\w]*(?=\\s*\\())",
      "(?<p>@[A-Za-z_][\\w.]*)",
    ].join("|"),
    "g",
  ),
  sh: new RegExp(
    [
      "(?<c>(?:^|(?<=\\s))#[^\\n]*)",
      "(?<s>\"(?:\\\\.|[^\"\\\\])*\"|'[^']*')",
      "(?<p>\\$\\{[^}]*\\}|\\$[A-Za-z_@#?*!][\\w]*)",
      `(?<k>\\b(?:${SH_KEYWORDS})\\b)`,
      "(?<n>(?<=\\s)--?[A-Za-z][\\w-]*)",
    ].join("|"),
    "gm",
  ),
  json: new RegExp(
    [
      "(?<p>\"(?:\\\\.|[^\"\\\\])*\"(?=\\s*:))",
      "(?<s>\"(?:\\\\.|[^\"\\\\])*\")",
      "(?<n>-?\\b\\d+(?:\\.\\d+)?(?:[eE][+-]?\\d+)?\\b)",
      "(?<k>\\b(?:true|false|null)\\b)",
    ].join("|"),
    "g",
  ),
  xml: new RegExp(
    ["(?<c><!--[\\s\\S]*?-->)", "(?<p><\\/?[A-Za-z_][\\w:.-]*|\\/?>)", "(?<s>\"[^\"]*\")"].join(
      "|",
    ),
    "g",
  ),
};

/** Payloads above this size are rendered untinted to keep the inspector snappy. */
const MAX_HIGHLIGHT_CHARS = 120_000;

export function tokenize(source: string, language: CodeLanguage): Token[] {
  if (language === "text" || source.length > MAX_HIGHLIGHT_CHARS) {
    return [{ kind: "", text: source }];
  }
  const grammar = GRAMMARS[language];
  const tokens: Token[] = [];
  let cursor = 0;
  for (const match of source.matchAll(grammar)) {
    const start = match.index;
    const text = match[0];
    if (text.length === 0) continue;
    if (start > cursor) tokens.push({ kind: "", text: source.slice(cursor, start) });
    const groups = match.groups ?? {};
    const kind: TokenKind =
      groups.c !== undefined
        ? "c"
        : groups.s !== undefined
          ? "s"
          : groups.n !== undefined
            ? "n"
            : groups.k !== undefined
              ? "k"
              : groups.f !== undefined
                ? "f"
                : groups.p !== undefined
                  ? "p"
                  : "";
    tokens.push({ kind, text });
    cursor = start + text.length;
  }
  if (cursor < source.length) tokens.push({ kind: "", text: source.slice(cursor) });
  return tokens;
}

/** Split a token stream into per-line token lists so a gutter can number them. */
export function tokenizeLines(source: string, language: CodeLanguage): Token[][] {
  const lines: Token[][] = [[]];
  for (const token of tokenize(source, language)) {
    const parts = token.text.split("\n");
    parts.forEach((part, idx) => {
      if (idx > 0) lines.push([]);
      if (part.length > 0) lines[lines.length - 1]?.push({ kind: token.kind, text: part });
    });
  }
  return lines;
}

export function normalizeLanguage(raw: string | null | undefined): CodeLanguage | null {
  switch (raw?.toLowerCase()) {
    case "js":
    case "javascript":
    case "ts":
    case "typescript":
    case "node":
      return "js";
    case "py":
    case "python":
    case "python3":
      return "py";
    case "sh":
    case "bash":
    case "shell":
    case "zsh":
      return "sh";
    case "json":
      return "json";
    case "xml":
    case "html":
      return "xml";
    default:
      return null;
  }
}

/** Guess the script language of a CodeMode body when the gateway did not label it. */
export function guessScriptLanguage(code: string): CodeLanguage {
  const head = code.slice(0, 4000);
  const pyScore =
    (head.match(/^\s*(?:def |import \w+$|from \w[\w.]* import |elif |print\()/gm)?.length ?? 0) +
    (head.match(/:\s*$/gm)?.length ?? 0);
  const jsScore =
    (head.match(/\b(?:const|let|await|=>|function)\b|;\s*$/gm)?.length ?? 0) +
    (head.match(/[{}]\s*$/gm)?.length ?? 0);
  return pyScore > jsScore ? "py" : "js";
}
