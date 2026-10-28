"use client";

import { useEffect, useId, useLayoutEffect, useRef, useState, type CSSProperties } from "react";
import { inputStyle } from "@/components/router/RouterPrimitives";
import { loadConnectionModels } from "@/lib/connectionModels";
import { routeSelectStyle } from "./routeFormStyles";

type Props = {
  label: string;
  connectionId: string;
  value: string;
  onChange: (model: string) => void;
};

export function UpstreamModelSelect(props: Props) {
  // A connection change must never show results from the previous connection.
  return <ConnectionModelSelect key={props.connectionId} {...props} />;
}

function ConnectionModelSelect({ label, connectionId, value, onChange }: Props) {
  const id = useId();
  const trigger = useRef<HTMLButtonElement>(null);
  const picker = useRef<HTMLDivElement>(null);
  const activeOption = useRef<HTMLButtonElement>(null);
  const [models, setModels] = useState<string[]>([]);
  const [loading, setLoading] = useState(!!connectionId);
  const [error, setError] = useState<string | null>(null);
  const [revision, setRevision] = useState(0);
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [active, setActive] = useState(0);
  const [placement, setPlacement] = useState<CSSProperties>({});

  useLayoutEffect(() => {
    if (!open) return;
    function position() {
      const rect = trigger.current?.getBoundingClientRect();
      if (!rect) return;
      const below = window.innerHeight - rect.bottom - 12;
      const above = rect.top - 12;
      const upwards = below < 300 && above > below;
      setPlacement({
        left: Math.max(8, Math.min(rect.left, window.innerWidth - rect.width - 8)),
        width: Math.min(rect.width, window.innerWidth - 16),
        ...(upwards ? { bottom: window.innerHeight - rect.top + 4 } : { top: rect.bottom + 4 }),
        maxHeight: Math.max(80, upwards ? above : below),
      });
    }
    function onScroll(event: Event) {
      if (!(event.target instanceof Node) || !picker.current?.contains(event.target)) position();
    }
    position();
    window.addEventListener("resize", position);
    window.addEventListener("scroll", onScroll, true);
    return () => {
      window.removeEventListener("resize", position);
      window.removeEventListener("scroll", onScroll, true);
    };
  }, [open]);

  useEffect(() => {
    if (!connectionId) return;
    let current = true;
    setLoading(true);
    setError(null);
    loadConnectionModels(connectionId).then(
      result => { if (current) setModels(result); },
      () => { if (current) setError("Could not load models for this connection."); },
    ).finally(() => { if (current) setLoading(false); });
    return () => { current = false; };
  }, [connectionId, revision]);

  const search = query.trim();
  const filtered = models.filter(model => model.toLowerCase().includes(search.toLowerCase()));
  const custom = search && !models.includes(search) ? search : null;
  const choices = custom ? [...filtered, custom] : filtered;
  const activeIndex = Math.min(active, choices.length - 1);
  const unlisted = value && !loading && !error && !models.includes(value);

  useEffect(() => { activeOption.current?.scrollIntoView({ block: "nearest" }); }, [activeIndex, query]);

  function choose(model: string) {
    onChange(model);
    setOpen(false);
    trigger.current?.focus();
  }

  return <div className="relative min-w-0" onBlur={event => {
    if (!event.currentTarget.contains(event.relatedTarget)) setOpen(false);
  }}>
    <span id={`${id}-label`} className="mb-1.5 block text-xs text-slate-400">{label}</span>
    <button ref={trigger} type="button" disabled={!connectionId} aria-labelledby={`${id}-label ${id}-value`}
      aria-haspopup="dialog" aria-expanded={open} aria-controls={open ? `${id}-picker` : undefined}
      className="flex w-full items-center justify-between gap-2 rounded-md px-3 py-2 text-left text-sm disabled:opacity-50"
      style={routeSelectStyle} onClick={() => { setOpen(!open); setQuery(""); setActive(0); }}>
      <span id={`${id}-value`} className="truncate">{value || (connectionId ? "Select a model…" : "Select a connection first")}</span>
    </button>
    {open && <div ref={picker} id={`${id}-picker`} role="dialog" aria-label={`${label} picker`} style={placement}
      className="fixed z-[60] overflow-y-auto rounded-md border border-slate-600 bg-slate-900 p-2 shadow-xl"
      onKeyDown={event => { if (event.key === "Escape") { event.preventDefault(); event.stopPropagation(); setOpen(false); trigger.current?.focus(); } }}>
      <input autoFocus role="combobox" aria-label={`Search ${label.toLowerCase()}`} aria-expanded="true"
        aria-controls={`${id}-options`} aria-autocomplete="list"
        aria-activedescendant={activeIndex >= 0 ? `${id}-option-${activeIndex}` : undefined}
        value={query} placeholder="Search models or enter a custom ID"
        className="w-full rounded px-2 py-2 text-sm" style={inputStyle}
        onChange={event => { setQuery(event.target.value); setActive(0); }}
        onKeyDown={event => {
          if (event.key === "ArrowDown" || event.key === "ArrowUp") {
            event.preventDefault();
            setActive(choices.length ? (Math.max(0, activeIndex) + (event.key === "ArrowDown" ? 1 : -1) + choices.length) % choices.length : 0);
          } else if (event.key === "Enter") {
            event.preventDefault();
            if (activeIndex >= 0) choose(choices[activeIndex]);
          }
        }} />
      {loading && <p role="status" className="px-2 py-2 text-xs text-slate-400">Loading models…</p>}
      {error && <div role="status" className="px-2 py-2 text-xs text-amber-200">{error} <button type="button" className="underline" onClick={() => setRevision(revision + 1)}>Retry</button></div>}
      {!loading && !error && !filtered.length && <p className="px-2 py-2 text-xs text-slate-400">{models.length ? "No matching models." : "No models were discovered. You can enter a custom ID."}</p>}
      <div id={`${id}-options`} role="listbox" aria-label={`${label} models`} className="max-h-48 overflow-y-auto">
        {choices.map((model, index) => <button key={model} id={`${id}-option-${index}`} ref={index === activeIndex ? activeOption : undefined}
          type="button" role="option" tabIndex={-1} aria-selected={model === value}
          className={`block w-full break-all rounded px-2 py-2 text-left text-sm ${index === activeIndex ? "bg-slate-700 text-white" : "text-slate-200 hover:bg-slate-800"}`}
          onMouseDown={event => event.preventDefault()} onClick={() => choose(model)}>
          {custom === model ? <>Use custom model: <span className="font-mono">{model}</span></> : model}
        </button>)}
      </div>
      {custom && <p className="px-2 pt-2 text-xs text-amber-200">Custom IDs are not verified. Check the spelling and this connection&apos;s access.</p>}
    </div>}
    {unlisted && <p className="mt-1.5 text-xs text-amber-200">This ID is not in the connection&apos;s discovered models. Check its spelling and access, or select a listed model.</p>}
    {error && !open && <p className="mt-1.5 text-xs text-amber-200">Model list unavailable; open the dropdown to retry or enter a custom ID.</p>}
  </div>;
}
