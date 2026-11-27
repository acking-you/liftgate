import cache from "./cache";
import catalog from "./catalog";
import common from "./common";
import context from "./context";
import infra from "./infra";
import overview from "./overview";
import shell from "./shell";
import tools from "./tools";
import usage from "./usage";

/** Every UI string, grouped by area. Chinese defines the shape every locale must fill. */
const zh = {
  common: common.zh,
  shell: shell.zh,
  overview: overview.zh,
  usage: usage.zh,
  tools: tools.zh,
  catalog: catalog.zh,
  context: context.zh,
  cache: cache.zh,
  infra: infra.zh,
};

export type Messages = typeof zh;

const en: Messages = {
  common: common.en,
  shell: shell.en,
  overview: overview.en,
  usage: usage.en,
  tools: tools.en,
  catalog: catalog.en,
  context: context.en,
  cache: cache.en,
  infra: infra.en,
};

export const catalogs: Record<"zh" | "en", Messages> = { zh, en };
