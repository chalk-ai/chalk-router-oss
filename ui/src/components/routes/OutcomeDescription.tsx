"use client";

import { useId } from "react";
import { inputStyle } from "@/components/router/RouterPrimitives";

export function OutcomeDescription({ value, disabled, onChange }: {
  value: string; disabled: boolean; onChange: (value: string) => void;
}) {
  const id = useId();
  return <div className="min-w-0" title={value}>
    <label htmlFor={id} className="mb-1.5 block text-xs text-cds-ink-secondary">Description for the judge</label>
    <textarea id={id} rows={3} disabled={disabled} value={value}
      aria-describedby={disabled ? `${id}-help` : undefined}
      className="w-full resize-y rounded-md px-3 py-2 text-sm disabled:cursor-not-allowed disabled:opacity-50"
      style={inputStyle} onChange={event => onChange(event.target.value)} />
    {disabled && <p id={`${id}-help`} className="mt-1 text-xs text-cds-ink-subtle">Not used by the local heuristic.</p>}
  </div>;
}
