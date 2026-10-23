// Environment tag rendering (R18): a colour bar on tree nodes and tabs, a labelled pill in
// the status bar and forms. Untagged is never shown — the default is Production.

import type { EnvTag as Tag } from "../lib/ipc/connections";

export const TAG_LABEL: Record<Tag, string> = {
  local: "Local",
  dev: "Dev",
  staging: "Staging",
  production: "Production",
};

export const TAGS: Tag[] = ["local", "dev", "staging", "production"];

export function TagBar({ tag }: { tag: Tag }) {
  return <span className="tag-bar" data-tag={tag} aria-hidden="true" />;
}

export function TagPill({ tag }: { tag: Tag }) {
  return (
    <span className="tag-pill" data-tag={tag}>
      {TAG_LABEL[tag]}
    </span>
  );
}
