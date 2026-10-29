"use client";

import { useId, useState } from "react";
import { inputStyle } from "@/components/router/RouterPrimitives";
import { heuristicKeywordError, type HeuristicBackend } from "@/lib/routeGrid";
import { activeHeuristicKeywords, addHeuristicKeyword, removeHeuristicKeyword } from "@/lib/heuristicKeywords";

export function HeuristicKeywordEditor({ value, onChange }: { value: HeuristicBackend; onChange: (value: HeuristicBackend) => void }) {
  const id = useId();
  const [draft, setDraft] = useState("");
  const [draftError, setDraftError] = useState<string | null>(null);
  const keywords = activeHeuristicKeywords(value);
  const error = draftError ?? heuristicKeywordError(value.added_hard_keywords, value.removed_hard_keywords);

  function update(next: HeuristicBackend) {
    const error = heuristicKeywordError(next.added_hard_keywords, next.removed_hard_keywords);
    setDraftError(error);
    if (error) return false;
    onChange(next);
    return true;
  }

  function add() {
    const error = heuristicKeywordError([draft], []);
    if (error) { setDraftError(error); return; }
    if (keywords.some(word => word.toLowerCase() === draft.toLowerCase())) {
      setDraftError("This keyword is already in the list.");
      return;
    }
    if (update(addHeuristicKeyword(value, draft))) setDraft("");
  }

  return <div>
    <div className="mb-2 flex items-center justify-between gap-3">
      <span id={`${id}-label`} className="text-sm font-medium text-cds-ink">Hard-task keywords</span>
      <button type="button" className="text-xs text-cds-ink-secondary underline hover:text-white" onClick={() => {
        update({ ...value, use_default_keywords: true, added_hard_keywords: [], removed_hard_keywords: [] });
        setDraft("");
      }}>Reset to defaults</button>
    </div>
    <div role="group" aria-labelledby={`${id}-label`} className="rounded-md border border-cds-border-strong bg-cds-raised p-3">
      <ul className="mb-3 flex flex-wrap gap-2" aria-label="Active hard-task keywords">
        {keywords.map(word => <li key={word.toLowerCase()} className="flex items-center gap-2 rounded bg-cds-status-neutral-fill px-2 py-1 text-xs text-cds-ink">
          <span className="whitespace-pre" title={JSON.stringify(word)}>{word.replace(/^ +| +$/g, spaces => "␠".repeat(spaces.length))}</span>
          <button type="button" aria-label={`Remove keyword ${JSON.stringify(word)}`} className="text-cds-ink-secondary hover:text-white" onClick={() => update(removeHeuristicKeyword(value, word))}>×</button>
        </li>)}
      </ul>
      {!keywords.length && <p className="mb-3 text-xs text-cds-ink-secondary">No keyword triggers. Code blocks and the length cutoff still apply if enabled.</p>}
      <div className="flex gap-2">
        <input aria-label="Add hard-task keyword" aria-invalid={!!error} aria-describedby={error ? `${id}-error` : `${id}-help`}
          className="min-w-0 flex-1 rounded-md px-3 py-2 text-sm" style={inputStyle} placeholder="Add a word or phrase"
          value={draft} onChange={event => { setDraft(event.target.value); setDraftError(null); }}
          onKeyDown={event => { if (event.key === "Enter") { event.preventDefault(); add(); } }} />
        <button type="button" disabled={!draft.trim()} className="rounded border border-cds-border-strong px-3 py-2 text-xs text-cds-ink-medium hover:bg-cds-hover disabled:opacity-40" onClick={add}>Add keyword</button>
      </div>
    </div>
    {error && <p id={`${id}-error`} role="alert" className="mt-2 text-sm text-amber-200">{error}</p>}
    <p id={`${id}-help`} className="mt-2 text-xs text-cds-ink-secondary">These are the active keywords. Remove with × or add a word or phrase. Matching ignores case; ␠ marks a leading or trailing space.</p>
  </div>;
}
