import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import { GatewayProvider } from "./hooks/gateway";
import type { GatewayClient } from "./lib/api";
import "./index.css";

const rootEl = document.getElementById("root");
if (!rootEl) {
  throw new Error("Missing #root mount element");
}
const root = createRoot(rootEl);

function render(demoClient?: GatewayClient): void {
  root.render(
    <StrictMode>
      <GatewayProvider demoClient={demoClient}>
        <App />
      </GatewayProvider>
    </StrictMode>,
  );
}

// `?demo` swaps in a synthetic backend. The branch is dev-only, so the demo
// data never reaches the bundle the gateway embeds.
if (import.meta.env.DEV && new URLSearchParams(window.location.search).has("demo")) {
  void import("./dev/demo").then(({ createDemoClient }) => render(createDemoClient()));
} else {
  render();
}
