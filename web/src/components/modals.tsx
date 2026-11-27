import * as React from "react";
import { useEffect, useState } from "react";
import { LoaderCircle, Play } from "lucide-react";
import { ProseBlock } from "@/components/CodeBlock";
import { Modal } from "@/components/ui/dialog";
import { Button, Field, Segmented } from "@/components/ui";
import { useGateway } from "@/hooks/gateway";
import { errorMessage } from "@/lib/api";
import type { TaskProtocol } from "@/types/telemetry";

export function ConnectionModal({
  open,
  onOpenChange,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}): React.JSX.Element {
  const { baseUrl, apiKey, updateConnection } = useGateway();
  const [draftUrl, setDraftUrl] = useState(baseUrl);
  const [draftKey, setDraftKey] = useState(apiKey);

  useEffect(() => {
    if (open) {
      setDraftUrl(baseUrl);
      setDraftKey(apiKey);
    }
  }, [open, baseUrl, apiKey]);

  return (
    <Modal
      open={open}
      onOpenChange={onOpenChange}
      title="连接网关"
      description="默认连接提供这个页面的同源网关。网关在别的地址或开启了密钥时，在这里指定。"
    >
      <form
        className="flex flex-col gap-3.5"
        onSubmit={(ev) => {
          ev.preventDefault();
          updateConnection(draftUrl, draftKey);
          onOpenChange(false);
        }}
      >
        <Field label="网关地址" htmlFor="conn-base-url" hint="留空表示同源。">
          <input
            id="conn-base-url"
            className="lg-input lg-num"
            placeholder="http://127.0.0.1:8787"
            value={draftUrl}
            onChange={(e) => setDraftUrl(e.target.value)}
          />
        </Field>
        <Field label="API Key" htmlFor="conn-api-key" hint="网关用 --api-key 启动时需要。只保存在这个浏览器里。">
          <input
            id="conn-api-key"
            className="lg-input lg-num"
            type="password"
            autoComplete="off"
            placeholder="可选"
            value={draftKey}
            onChange={(e) => setDraftKey(e.target.value)}
          />
        </Field>
        <div className="flex justify-end gap-2 pt-1">
          <Button onClick={() => onOpenChange(false)}>取消</Button>
          <Button type="submit" variant="primary">
            保存并重连
          </Button>
        </div>
      </form>
    </Modal>
  );
}

export function ProbeModal({
  open,
  onOpenChange,
  model,
  onModelChange,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  model: string;
  onModelChange: (model: string) => void;
}): React.JSX.Element {
  const { client, snapshot } = useGateway();
  const [protocol, setProtocol] = useState<TaskProtocol>("openai_responses");
  const [prompt, setPrompt] = useState("用一句话介绍 Rust 2024 Edition 的优势。");
  const [running, setRunning] = useState(false);
  const [result, setResult] = useState<{ ok: boolean; text: string } | null>(null);
  const models = snapshot?.available_models ?? [];

  const run = async (): Promise<void> => {
    setRunning(true);
    setResult(null);
    try {
      setResult({ ok: true, text: await client.probe(protocol, model.trim(), prompt) });
    } catch (err) {
      setResult({ ok: false, text: errorMessage(err) });
    } finally {
      setRunning(false);
    }
  };

  return (
    <Modal
      open={open}
      onOpenChange={onOpenChange}
      title="拨测"
      description="向网关发一条真实的非流式请求，验证协议转换、会话与缓存统计是否正常。会消耗少量额度。"
      className="max-w-[560px]"
    >
      <form
        className="flex flex-col gap-3.5"
        onSubmit={(ev) => {
          ev.preventDefault();
          void run();
        }}
      >
        <Segmented<TaskProtocol>
          ariaLabel="协议"
          value={protocol}
          onChange={setProtocol}
          options={[
            { value: "openai_responses", label: "OpenAI /v1/responses" },
            { value: "anthropic_messages", label: "Anthropic /v1/messages" },
          ]}
        />
        <Field label="模型" htmlFor="probe-model">
          <input
            id="probe-model"
            className="lg-input lg-num"
            list="probe-model-list"
            value={model}
            onChange={(e) => onModelChange(e.target.value)}
          />
          <datalist id="probe-model-list">
            {models.map((m) => (
              <option key={m.modelId} value={m.modelId}>
                {m.name}
              </option>
            ))}
          </datalist>
        </Field>
        <Field label="提示词" htmlFor="probe-prompt">
          <textarea
            id="probe-prompt"
            className="lg-input h-auto py-2"
            rows={3}
            value={prompt}
            onChange={(e) => setPrompt(e.target.value)}
          />
        </Field>
        {result && (
          <ProseBlock
            text={result.text}
            title={result.ok ? "响应" : "请求失败"}
            tone={result.ok ? undefined : "err"}
            maxHeight={240}
          />
        )}
        <div className="flex justify-end gap-2 pt-1">
          <Button onClick={() => onOpenChange(false)}>关闭</Button>
          <Button type="submit" variant="primary" disabled={running || model.trim() === ""}>
            {running ? <LoaderCircle className="lg-spin" /> : <Play />}
            {running ? "请求中…" : "发送"}
          </Button>
        </div>
      </form>
    </Modal>
  );
}
