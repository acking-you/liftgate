import * as React from "react";
import { useEffect, useState } from "react";
import { LoaderCircle, Play } from "lucide-react";
import { ProseBlock } from "@/components/CodeBlock";
import { Modal } from "@/components/ui/dialog";
import { Button, Field, Segmented } from "@/components/ui";
import { useGateway } from "@/hooks/gateway";
import { errorMessage } from "@/lib/api";
import { useI18n } from "@/lib/i18n";
import type { TaskProtocol } from "@/types/telemetry";

export function ConnectionModal({
  open,
  onOpenChange,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}): React.JSX.Element {
  const { baseUrl, apiKey, updateConnection } = useGateway();
  const { m } = useI18n();
  const t = m.shell.connect;
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
      title={t.title}
      description={t.description}
    >
      <form
        className="flex flex-col gap-3.5"
        onSubmit={(ev) => {
          ev.preventDefault();
          updateConnection(draftUrl, draftKey);
          onOpenChange(false);
        }}
      >
        <Field label={t.baseUrl} htmlFor="conn-base-url" hint={t.baseUrlHint}>
          <input
            id="conn-base-url"
            className="lg-input lg-num"
            placeholder="http://127.0.0.1:8787"
            value={draftUrl}
            onChange={(e) => setDraftUrl(e.target.value)}
          />
        </Field>
        <Field label="API Key" htmlFor="conn-api-key" hint={t.apiKeyHint}>
          <input
            id="conn-api-key"
            className="lg-input lg-num"
            type="password"
            autoComplete="off"
            placeholder={t.optional}
            value={draftKey}
            onChange={(e) => setDraftKey(e.target.value)}
          />
        </Field>
        <div className="flex justify-end gap-2 pt-1">
          <Button onClick={() => onOpenChange(false)}>{m.common.cancel}</Button>
          <Button type="submit" variant="primary">
            {t.saveReconnect}
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
  const { m } = useI18n();
  const t = m.shell.probeModal;
  const [protocol, setProtocol] = useState<TaskProtocol>("openai_responses");
  const [prompt, setPrompt] = useState(t.defaultPrompt);
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
      title={t.title}
      description={t.description}
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
          ariaLabel={t.protocol}
          value={protocol}
          onChange={setProtocol}
          options={[
            { value: "openai_responses", label: "OpenAI /v1/responses" },
            { value: "anthropic_messages", label: "Anthropic /v1/messages" },
          ]}
        />
        <Field label={t.model} htmlFor="probe-model">
          <input
            id="probe-model"
            className="lg-input lg-num"
            list="probe-model-list"
            value={model}
            onChange={(e) => onModelChange(e.target.value)}
          />
          <datalist id="probe-model-list">
            {models.map((entry) => (
              <option key={entry.modelId} value={entry.modelId}>
                {entry.name}
              </option>
            ))}
          </datalist>
        </Field>
        <Field label={t.prompt} htmlFor="probe-prompt">
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
            title={result.ok ? t.response : t.failed}
            tone={result.ok ? undefined : "err"}
            maxHeight={240}
          />
        )}
        <div className="flex justify-end gap-2 pt-1">
          <Button onClick={() => onOpenChange(false)}>{m.common.close}</Button>
          <Button type="submit" variant="primary" disabled={running || model.trim() === ""}>
            {running ? <LoaderCircle className="lg-spin" /> : <Play />}
            {running ? t.sending : t.send}
          </Button>
        </div>
      </form>
    </Modal>
  );
}
