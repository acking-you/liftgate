import type * as React from "react";

const zh = {
  loading: "加载中…",
  cancel: "取消",
  close: "关闭",
  save: "保存",
  copy: "复制",
  copied: "已复制",
  none: "无",
  wrap: "自动换行",
  text: "文本",
  patch: "补丁",
  showMoreLines: (n: string) => `显示其余 ${n} 行`,
  showMoreChars: (n: string) => `显示其余 ${n} 个字符`,
  lines: (n: string) => `${n} 行`,
  pager: (page: number, pages: number, total: string) => `${page} / ${pages} · 共 ${total} 条`,
  prevPage: "上一页",
  nextPage: "下一页",
  unsupportedTitle: (feature: string) => `当前网关尚未提供${feature}`,
  unsupportedHint: (endpoint: React.ReactNode): React.ReactNode => (
    <>需要 {endpoint}，升级网关后自动点亮。</>
  ),
  patchOps: { update: "改", add: "增", delete: "删" },
  sizeUnchanged: "不变",
  harnessUnknown: "未识别",
  modeBashFirst: "Bash 聚合",
  modeDirect: "直调",
  themeToLight: "切换到浅色主题",
  themeToDark: "切换到深色主题",
  localeSwitch: "Switch to English",
  localeShort: "EN",
  segmentAria: (label: string, parts: string[]) => `${label}：${parts.join("，")}`,
  segmentPart: (label: string, value: string, share: string) => `${label} ${value}（${share}）`,
  trend: {
    empty: "完成至少一轮请求后，这里会画出逐轮缓存命中率。",
    aria: (n: number, last: string, low: number) =>
      `最近 ${n} 轮请求的缓存命中率，最新一轮 ${last}，其中 ${low} 轮低于阈值`,
    threshold: "阈值",
    cacheRead: "缓存读",
    lowTurns: (n: number) => `低于阈值的轮次（${n}）`,
  },
  skyline: {
    bytesAria: (n: number, maxOut: string, failures: number) =>
      `最近 ${n} 次工具调用的输入与输出体积，最大输出 ${maxOut}，${failures} 次失败`,
    durationAria: (n: number, maxDur: string, failures: number) =>
      `最近 ${n} 次工具调用的耗时，最长 ${maxDur}，${failures} 次失败`,
    outPeak: (v: string) => `↑ 输出 · 峰值 ${v}`,
    inPeak: (v: string) => `↓ 输入 · 峰值 ${v}`,
    durPeak: (v: string) => `↑ 耗时 · 峰值 ${v}`,
    turnStep: (turn: number, step: number) => `轮 ${turn} · 步 ${step}`,
    io: (input: string, output: string) => `入 ${input} → 出 ${output}`,
    running: "进行中",
    preflightRejected: "网关预检拒绝",
    failed: "调用失败",
    legendOrchestrator: "CodeMode 编排器",
    legendOther: "其他调用",
    legendFailed: "失败",
  },
  latency: {
    ttft: "首 Token 延迟",
    elapsed: "耗时",
    tool: "工具耗时",
    levels: { ok: "快", neutral: "正常", warn: "偏慢", err: "很慢" },
    hint: (label: string, value: string, level: string) => `${label} ${value} · ${level}`,
    scale: (bounds: [string, string, string]) =>
      `绿 < ${bounds[0]} · 常规 < ${bounds[1]} · 黄 < ${bounds[2]} · 红 ≥ ${bounds[2]}`,
  },
  cost: {
    unpriced: "未定价",
    perMillion: "/ 百万 tokens",
  },
};

export type CommonMessages = typeof zh;

const en: CommonMessages = {
  loading: "Loading…",
  cancel: "Cancel",
  close: "Close",
  save: "Save",
  copy: "Copy",
  copied: "Copied",
  none: "None",
  wrap: "Wrap lines",
  text: "Text",
  patch: "Patch",
  showMoreLines: (n) => `Show ${n} more lines`,
  showMoreChars: (n) => `Show ${n} more characters`,
  lines: (n) => `${n} lines`,
  pager: (page, pages, total) => `${page} / ${pages} · ${total} total`,
  prevPage: "Previous",
  nextPage: "Next",
  unsupportedTitle: (feature) => `This gateway does not provide ${feature} yet`,
  unsupportedHint: (endpoint) => <>Needs {endpoint}; it lights up once the gateway is upgraded.</>,
  patchOps: { update: "M", add: "A", delete: "D" },
  sizeUnchanged: "Unchanged",
  harnessUnknown: "Unknown",
  modeBashFirst: "Bash-first",
  modeDirect: "Direct",
  themeToLight: "Switch to light theme",
  themeToDark: "Switch to dark theme",
  localeSwitch: "切换到中文",
  localeShort: "中",
  segmentAria: (label, parts) => `${label}: ${parts.join(", ")}`,
  segmentPart: (label, value, share) => `${label} ${value} (${share})`,
  trend: {
    empty: "The per-turn cache hit rate appears here after the first completed request.",
    aria: (n, last, low) =>
      `Cache hit rate of the last ${n} requests; latest ${last}, ${low} below the threshold`,
    threshold: "Threshold",
    cacheRead: "Cache read",
    lowTurns: (n) => `Turns below the threshold (${n})`,
  },
  skyline: {
    bytesAria: (n, maxOut, failures) =>
      `Input and output size of the last ${n} tool calls; largest output ${maxOut}, ${failures} failed`,
    durationAria: (n, maxDur, failures) =>
      `Duration of the last ${n} tool calls; longest ${maxDur}, ${failures} failed`,
    outPeak: (v) => `↑ Output · peak ${v}`,
    inPeak: (v) => `↓ Input · peak ${v}`,
    durPeak: (v) => `↑ Duration · peak ${v}`,
    turnStep: (turn, step) => `Turn ${turn} · step ${step}`,
    io: (input, output) => `In ${input} → out ${output}`,
    running: "Running",
    preflightRejected: "Rejected by gateway preflight",
    failed: "Call failed",
    legendOrchestrator: "CodeMode orchestrator",
    legendOther: "Other calls",
    legendFailed: "Failed",
  },
  latency: {
    ttft: "Time to first token",
    elapsed: "Duration",
    tool: "Tool duration",
    levels: { ok: "fast", neutral: "normal", warn: "slow", err: "very slow" },
    hint: (label, value, level) => `${label} ${value} · ${level}`,
    scale: (bounds) =>
      `Green < ${bounds[0]} · normal < ${bounds[1]} · amber < ${bounds[2]} · red ≥ ${bounds[2]}`,
  },
  cost: {
    unpriced: "Unpriced",
    perMillion: "per 1M tokens",
  },
};

export default { zh, en };
