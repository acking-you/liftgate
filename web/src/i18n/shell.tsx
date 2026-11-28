import type * as React from "react";

const zh = {
  title: "Liftgate 控制台",
  brandSub: "ACP 网关",
  navAria: "主导航",
  groups: { observe: "观测", infra: "基础设施" },
  views: {
    overview: "总览",
    usage: "用量与费用",
    tools: "工具调用",
    catalog: "工具目录",
    context: "完整上下文",
    cache: "缓存",
    sessions: "会话池",
    models: "模型",
    diagnostics: "诊断与接入",
  },
  capturing: "捕获中",
  toolFailuresBadge: (n: number) => `${n} 次工具调用失败或被拦截`,
  status: {
    demo: "演示数据",
    demoTitle: "本地合成的演示数据",
    connecting: "连接中",
    reconnecting: "重连中",
    connectingTitle: "正在连接遥测事件流",
    acpDown: "ACP 断开",
    acpDownTitle: "agy acp 主进程已断开",
    live: "实时",
    liveTitle: "遥测事件流已连接",
  },
  openNav: "打开导航",
  expandSidebar: "展开侧栏",
  collapseSidebar: "收起侧栏",
  probe: "拨测",
  connection: "连接",
  connectionSettings: "连接设置",
  providerSelectAria: "切换全局默认上游 Provider",
  optMode: {
    optimized: "智能优化",
    passthrough: "纯透传观测",
    custom: "自定义优化",
    toggleHintOptimized:
      "当前启用上下文蒸馏与工具虚拟化。点击切换为「纯透传观测 (Passthrough)」以无损采集 Harness 原始行为。",
    toggleHintPassthrough:
      "当前为「纯透传观测 (Passthrough)」：保留原始 System Prompt 与全量工具 Schema。点击恢复「智能优化」。",
  },
  demoNote: (code: React.ReactNode): React.ReactNode => (
    <>演示数据，未连接网关。去掉地址里的 {code} 即连接真实网关。</>
  ),
  unreachable: (error: React.ReactNode, start: React.ReactNode): React.ReactNode => (
    <>
      连接不到网关（{error}）。确认 {start} 已在运行，或在右上角「连接」里检查地址与密钥。
    </>
  ),
  connect: {
    title: "连接网关",
    description: "默认连接提供这个页面的同源网关。网关在别的地址或开启了密钥时，在这里指定。",
    baseUrl: "网关地址",
    baseUrlHint: "留空表示同源。",
    apiKeyHint: "网关用 --api-key 启动时需要。只保存在这个浏览器里。",
    optional: "可选",
    saveReconnect: "保存并重连",
  },
  probeModal: {
    title: "拨测",
    description: "向网关发一条真实的非流式请求，验证协议转换、会话与缓存统计是否正常。会消耗少量额度。",
    protocol: "协议",
    model: "模型",
    prompt: "提示词",
    defaultPrompt: "用一句话介绍 Rust 2024 Edition 的优势。",
    response: "响应",
    failed: "请求失败",
    sending: "请求中…",
    send: "发送",
  },
};

export type ShellMessages = typeof zh;

const en: ShellMessages = {
  title: "Liftgate Console",
  brandSub: "ACP gateway",
  navAria: "Main navigation",
  groups: { observe: "Observe", infra: "Infrastructure" },
  views: {
    overview: "Overview",
    usage: "Usage & cost",
    tools: "Tool calls",
    catalog: "Tool catalog",
    context: "Full context",
    cache: "Cache",
    sessions: "Session pool",
    models: "Models",
    diagnostics: "Diagnostics",
  },
  capturing: "On",
  toolFailuresBadge: (n) => `${n} tool ${n === 1 ? "call" : "calls"} failed or rejected`,
  status: {
    demo: "Demo data",
    demoTitle: "Synthetic data generated locally",
    connecting: "Connecting",
    reconnecting: "Reconnecting",
    connectingTitle: "Connecting to the telemetry event stream",
    acpDown: "ACP down",
    acpDownTitle: "The agy acp process has disconnected",
    live: "Live",
    liveTitle: "Telemetry event stream connected",
  },
  openNav: "Open navigation",
  expandSidebar: "Expand sidebar",
  collapseSidebar: "Collapse sidebar",
  probe: "Probe",
  connection: "Connection",
  connectionSettings: "Connection settings",
  providerSelectAria: "Switch active default upstream provider",
  optMode: {
    optimized: "Optimized",
    passthrough: "Passthrough",
    custom: "Custom Opt",
    toggleHintOptimized:
      "Prompt distillation & 0-RTT tool virtualization active. Click to switch to Passthrough (Observe-Only) to capture raw harness prompts & schemas verbatim.",
    toggleHintPassthrough:
      "Passthrough (Observe-Only) active: raw harness prompts & tool schemas forwarded verbatim. Click to re-enable Gateway Optimization.",
  },
  demoNote: (code) => <>Demo data; no gateway connected. Remove {code} from the address to connect to a real gateway.</>,
  unreachable: (error, start) => (
    <>
      Cannot reach the gateway ({error}). Make sure {start} is running, or check the address and key
      under Connection at the top right.
    </>
  ),
  connect: {
    title: "Connect to gateway",
    description:
      "By default the dashboard talks to the gateway that serves this page. Set an address or key here when the gateway lives elsewhere or requires one.",
    baseUrl: "Gateway address",
    baseUrlHint: "Leave empty for the same origin.",
    apiKeyHint: "Needed when the gateway runs with --api-key. Stored only in this browser.",
    optional: "Optional",
    saveReconnect: "Save and reconnect",
  },
  probeModal: {
    title: "Probe",
    description:
      "Sends one real non-streaming request through the gateway to check protocol translation, sessions, and cache accounting. Uses a little quota.",
    protocol: "Protocol",
    model: "Model",
    prompt: "Prompt",
    defaultPrompt: "In one sentence, what does the Rust 2024 Edition improve?",
    response: "Response",
    failed: "Request failed",
    sending: "Sending…",
    send: "Send",
  },
};

export default { zh, en };
