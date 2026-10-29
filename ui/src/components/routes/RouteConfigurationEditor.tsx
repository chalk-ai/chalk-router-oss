"use client";

import { useEffect, useId, useState, type ReactNode } from "react";
import { inputStyle } from "@/components/router/RouterPrimitives";
import { routeSelectStyle } from "./routeFormStyles";
import { UpstreamModelSelect } from "@/components/routes/UpstreamModelSelect";
import { heuristicKeywordError, decodeTarget, effectiveTarget, encodeTarget, routeGridError, type JudgeBackend, type JudgeOutcomeConfig, type RouteConfiguration, type RouteGridConfig, type RouteJudge, type RouteTarget } from "@/lib/routeGrid";
import { HeuristicKeywordEditor } from "@/components/routes/HeuristicKeywordEditor";
import { OutcomeDescription } from "./OutcomeDescription";
import { defaultJudgeOutcomes, loadRouteJudgeDefaults, type RouteJudgeDefaults } from "@/lib/routeJudgeDefaults";

import type { JudgeValidation } from "./useJudgeValidation";

export type RouteConnection = { id: string; name: string; provider_kind?: string };
const control = "w-full rounded-md px-3 py-2 text-sm";
function Field({ label, children }: { label: string; children: ReactNode }) { return <label className="block text-xs text-cds-ink-secondary"><span className="mb-1.5 block">{label}</span>{children}</label>; }
function Button({ children, onClick, disabled = false }: { children: ReactNode; onClick: () => void; disabled?: boolean }) { return <button type="button" disabled={disabled} onClick={onClick} className="rounded border border-cds-border-strong px-2.5 py-1.5 text-xs text-cds-ink-medium hover:bg-cds-hover disabled:opacity-40">{children}</button>; }

function PolicySection({ title, description, children }: { title: string; description: string; children: ReactNode }) {
  const id = useId();
  return <section aria-labelledby={id} className="space-y-4 rounded-lg border border-cds-border-strong p-4">
    <div><h3 id={id} className="text-cds-md font-medium text-cds-ink-strong">{title}</h3><p className="mt-1 text-xs leading-relaxed text-cds-ink-secondary">{description}</p></div>
    {children}
  </section>;
}

function JudgeSection({ title, children }: { title: string; children: ReactNode }) {
  const id = useId();
  return <section aria-labelledby={id} className="space-y-4 border-t border-cds-border pt-4">
    <h4 id={id} className="text-sm font-semibold text-cds-ink">{title}</h4>
    {children}
  </section>;
}

export function TargetEditor({ label, value, connections, onChange, hideLegend = false }: { label: string; value: RouteTarget; connections: RouteConnection[]; onChange: (value: RouteTarget) => void; hideLegend?: boolean }) {
  return <fieldset className="min-w-0"><legend className={hideLegend ? "sr-only" : "mb-2 text-sm font-medium text-cds-ink"}>{label}</legend><div className="grid gap-3 sm:grid-cols-2">
    <Field label={`${label} connection`}><select className={control} style={routeSelectStyle} value={value.connection_id} onChange={e => onChange({ ...value, connection_id: e.target.value })}><option value="">Select connection…</option>{value.connection_id && !connections.some(c => c.id === value.connection_id) && <option value={value.connection_id}>Unavailable connection ({value.connection_id})</option>}{connections.map(c => <option key={c.id} value={c.id}>{c.name}{c.provider_kind ? ` (${c.provider_kind})` : ""}</option>)}</select></Field>
    <UpstreamModelSelect label={`${label} upstream model`} connectionId={value.connection_id} value={value.upstream_model_id} onChange={upstream_model_id => onChange({ ...value, upstream_model_id })} />
  </div></fieldset>;
}
function ModelEditor(props: { label: string; value: string; connections: RouteConnection[]; onChange: (value: string) => void }) { return <TargetEditor {...props} value={decodeTarget(props.value)} onChange={value => props.onChange(encodeTarget(value))} />; }

function ChainEditor({ value, onChange, connections }: { value: string[]; onChange: (value: string[]) => void; connections: RouteConnection[] }) {
  function move(index: number, offset: number) { const next = [...value]; [next[index], next[index + offset]] = [next[index + offset], next[index]]; onChange(next); }
  return <div className="space-y-3">{value.length === 0 && <p className="text-xs text-cds-ink-secondary">No fallback models added. This chain stops after its starting model.</p>}{value.map((model, index) => <div key={index} className="rounded border border-cds-border p-3"><ModelEditor label={`Fallback ${index + 1}`} value={model} connections={connections} onChange={v => onChange(value.map((m, i) => i === index ? v : m))} /><div className="mt-2 flex gap-2"><Button disabled={index === 0} onClick={() => move(index, -1)}>Move up</Button><Button disabled={index === value.length - 1} onClick={() => move(index, 1)}>Move down</Button><Button onClick={() => onChange(value.filter((_, i) => i !== index))}>Remove fallback {index + 1}</Button></div></div>)}<Button disabled={value.length >= 16} onClick={() => onChange([...value, `${connections[0]?.id ?? ""}/`])}>Add fallback</Button></div>;
}
const emptyOutcome = (target: RouteTarget): JudgeOutcomeConfig => ({ model: encodeTarget(target), description: "", fallbacks: null });
function newJudge(target: RouteTarget, defaults: RouteJudgeDefaults): RouteJudge {
  return { mode: "off", classification: "difficulty", backend: { type: "heuristic", added_hard_keywords: [], removed_hard_keywords: [] }, outcomes: defaultJudgeOutcomes(defaults, "difficulty", target) };
}

export function RouteConfigurationEditor({ value, onChange, connections, judgeValidation }: { judgeValidation: JudgeValidation; value: RouteConfiguration; onChange: (value: RouteConfiguration) => void; connections: RouteConnection[] }) {
  const [editError, setEditError] = useState<string | null>(null);
  const [customDraft, setCustomDraft] = useState<RouteGridConfig | null>(null);
  const [judgeDraft, setJudgeDraft] = useState<RouteJudge | null>(null);
  const [defaults, setDefaults] = useState<RouteJudgeDefaults | null>(null);
  const [defaultsError, setDefaultsError] = useState<string | null>(null);
  const [defaultsRevision, setDefaultsRevision] = useState(0);
  const [outcomeDrafts, setOutcomeDrafts] = useState<Partial<Record<RouteJudge["classification"], RouteJudge["outcomes"]>>>({});
  useEffect(() => {
    let current = true;
    setDefaultsError(null);
    loadRouteJudgeDefaults().then(
      result => { if (current) setDefaults(result); },
      () => { if (current) setDefaultsError("Default judge categories are unavailable."); },
    );
    return () => { current = false; };
  }, [defaultsRevision]);
  const grid = value.routing;
  const judge = grid?.judge;
  const keywordError = judge?.backend.type === "heuristic" ? heuristicKeywordError(judge.backend.added_hard_keywords, judge.backend.removed_hard_keywords) : null;
  const validationError = grid ? routeGridError(grid) : null;
  const error = validationError === keywordError ? null : validationError;
  function setGrid(next: RouteGridConfig) { setEditError(null); onChange({ ...value, routing: next }); }
  function setJudge(next: RouteJudge) { if (grid) setGrid({ ...grid, judge: next }); }
  function setBackend(backend: JudgeBackend) { if (judge) setJudge({ ...judge, backend }); }
  function restoreDefaults() {
    if (!judge || !grid || !defaults) return;
    const outcomes = defaultJudgeOutcomes(defaults, judge.classification, grid.default_target);
    for (const [label, outcome] of Object.entries(outcomes)) {
      const existing = judge.outcomes[label];
      if (existing) outcomes[label] = { ...existing, description: outcome.description };
    }
    setJudge({ ...judge, outcomes });
  }
  function updateOutcome(index: number, label: string, outcome: JudgeOutcomeConfig) {
    if (!judge) return;
    const entries = Object.entries(judge.outcomes);
    if (entries.some(([key], i) => i !== index && key === label)) { setEditError("Each outcome needs a unique label."); return; }
    entries[index] = [label, outcome];
    setJudge({ ...judge, outcomes: Object.fromEntries(entries) });
  }
  return <div className="space-y-6">
    <PolicySection title="Route configuration" description="Choose a single destination or configure model selection and fallback rules for this route.">
    <Field label="Routing behavior"><select className={control} style={routeSelectStyle} value={grid ? "custom" : "simple"} onChange={e => {
      if (e.target.value === "simple") { setCustomDraft(grid ?? null); onChange({ ...value, ...effectiveTarget(value), routing: null }); }
      else setGrid(customDraft ?? { default_target: { connection_id: value.connection_id, upstream_model_id: value.upstream_model_id }, fallbacks: {}, judge: null });
    }}><option value="simple">Simple — one connection and model</option><option value="custom">Custom — judge and fallback rules</option></select></Field>
    </PolicySection>
    <PolicySection title={grid ? "Default target" : "Route target"} description={grid ? "Used when judging is off, unavailable, or cannot classify the request. Its matching route fallback chain still applies." : "Send requests to this connection and upstream model."}>
    <TargetEditor hideLegend label={grid ? "Default target" : "Target"} value={effectiveTarget(value)} connections={connections} onChange={target => grid ? setGrid({ ...grid, default_target: target }) : onChange({ ...value, ...target })} />
    </PolicySection>
    {grid && <>
      <PolicySection title="Route fallback chains" description="Choose a starting connection and model, then add alternatives in retry order. The starting model must match the selected target exactly; chains do not follow another model's fallback rules.">
        {Object.keys(grid.fallbacks).length === 0 && <p className="text-xs text-cds-ink-secondary">No route fallback chains configured.</p>}
        {Object.entries(grid.fallbacks).map(([model, chain], index) => <div key={index} className="space-y-3 rounded-lg bg-slate-950/40 p-3"><h4 className="text-sm font-semibold text-cds-ink">Chain {index + 1}</h4><ModelEditor label="When this model is selected" value={model} connections={connections} onChange={next => {
          if (next !== model && Object.hasOwn(grid.fallbacks, next)) { setEditError("This model already has a route fallback chain. Edit that chain or choose another model."); return; }
          setGrid({ ...grid, fallbacks: Object.fromEntries(Object.entries(grid.fallbacks).map(([key, val]) => key === model ? [next, val] : [key, val])) });
        }} /><ChainEditor value={chain} connections={connections} onChange={next => setGrid({ ...grid, fallbacks: { ...grid.fallbacks, [model]: next } })} /><Button onClick={() => setGrid({ ...grid, fallbacks: Object.fromEntries(Object.entries(grid.fallbacks).filter(([key]) => key !== model)) })}>Remove chain</Button></div>)}
        <Button disabled={Object.keys(grid.fallbacks).length >= 128} onClick={() => { let model = encodeTarget(grid.default_target); let i = 1; while (Object.hasOwn(grid.fallbacks, model)) model = `${grid.default_target.connection_id}/new-model-${i++}`; setGrid({ ...grid, fallbacks: { ...grid.fallbacks, [model]: [] } }); }}>Add model chain</Button>
      </PolicySection>
      <PolicySection title="Judge policy" description="Classify each request to select a model and its fallback behavior."><label className="flex items-center gap-2 text-sm text-cds-ink"><input type="checkbox" checked={!!judge} disabled={!judge && !judgeDraft && !defaults} onChange={e => {
        if (!e.target.checked) { setJudgeDraft(judge ?? null); setGrid({ ...grid, judge: null }); }
        else if (judgeDraft) setGrid({ ...grid, judge: judgeDraft });
        else if (defaults) { setOutcomeDrafts({}); setGrid({ ...grid, judge: newJudge(grid.default_target, defaults) }); }
      }} />Configure a judge for this route</label>
      {defaultsError ? <p role="alert" className="text-xs text-amber-200">{defaultsError} <Button onClick={() => setDefaultsRevision(defaultsRevision + 1)}>Retry</Button></p> : !defaults && <p role="status" className="text-xs text-cds-ink-secondary">Loading default judge categories…</p>}
      {judge && <>
        <JudgeSection title="Judge settings">
        <div className="grid gap-3 sm:grid-cols-2"><Field label="Judge mode"><select style={routeSelectStyle} className={control} value={judge.mode} onChange={e => setJudge({ ...judge, mode: e.target.value as RouteJudge["mode"] })}><option value="off">Off</option><option value="shadow">Shadow</option><option value="live">Live</option></select></Field><Field label="Judge backend"><select style={routeSelectStyle} className={control} value={judge.backend.type} onChange={e => {
          const type = e.target.value as JudgeBackend["type"];
          setBackend(type === "heuristic" ? { type, added_hard_keywords: [], removed_hard_keywords: [] } : { type, target: judge.backend.type === "heuristic" ? { ...grid.default_target } : judge.backend.target, timeout_ms: judge.backend.type === "heuristic" ? 1000 : judge.backend.timeout_ms });
        }}><option value="heuristic">Local heuristic</option><option value="completion">Completion model</option><option value="decisions">Decisions model</option></select></Field></div>
        <p className="text-xs text-cds-ink-secondary">{judge.mode === "shadow" ? "Shadow records the judge decision while serving the default target and its fallbacks." : judge.mode === "live" ? "Live selects an outcome once before serving the request." : "Off serves the default target without running the judge."} Judging applies to Chat Completions, Messages and Responses; Responses requiring external conversation context use the default target.</p>
        {judge.mode !== "live" && <p className="rounded-md border border-amber-900/60 bg-amber-950/20 p-3 text-xs text-amber-200">{judge.mode === "off" ? "Judging is off. Switch to Live to serve the models configured in the decision grid." : "Shadow mode evaluates the decision grid but serves the default target and its fallbacks."}</p>}
        </JudgeSection>
        <JudgeSection title={judge.backend.type === "heuristic" ? "Heuristic rules" : "Judge model and timeout"}>
        {judge.backend.type === "heuristic" ? <HeuristicEditor value={judge.backend} onChange={setBackend} /> : <div className="space-y-3"><TargetEditor label="Judge model" value={judge.backend.target} connections={connections} onChange={target => { if (judge.backend.type !== "heuristic") setBackend({ ...judge.backend, target }); }} />{judgeValidation.checking && <p role="status" className="text-xs text-cds-ink-secondary">Checking judge compatibility…</p>}{judgeValidation.error && <div role="alert" className="space-y-2 text-sm text-amber-200"><p>{judgeValidation.error}</p><Button onClick={judgeValidation.retry}>Check again</Button></div>}<Field label="Judge timeout (milliseconds; 0 uses 1000 ms)"><input type="number" min={0} max={30000} step={1} className={control} style={inputStyle} value={judge.backend.timeout_ms} onChange={e => { if (judge.backend.type !== "heuristic") setBackend({ ...judge.backend, timeout_ms: e.target.valueAsNumber }); }} /></Field></div>}
        </JudgeSection>
        <JudgeSection title="Decision grid">
        <p className="text-xs text-cds-ink-secondary">Define each classification outcome, the model it selects, and what happens if that model cannot serve the request.</p>
        <Field label="Classification"><select className={control} style={routeSelectStyle} value={judge.classification} disabled={!defaults} onChange={e => {
          const classification = e.target.value as RouteJudge["classification"];
          if (!defaults) return;
          setOutcomeDrafts({ ...outcomeDrafts, [judge.classification]: judge.outcomes });
          setJudge({ ...judge, classification, outcomes: outcomeDrafts[classification] ?? defaultJudgeOutcomes(defaults, classification, grid.default_target) });
        }}><option value="difficulty">Difficulty (0 = easiest)</option><option value="task" disabled={judge.backend.type === "heuristic"}>Task category</option></select></Field>
        {judge.backend.type === "heuristic" && <p className="text-xs text-cds-ink-secondary">Local heuristic requires difficulty outcomes 0 (easy) and 1 (hard). Changing backend preserves your outcomes; adjust them below if needed.</p>}
        {Object.entries(judge.outcomes).map(([label, outcome], index) => <div key={index} className="space-y-3 rounded-lg border border-cds-border-strong bg-slate-950/40 p-4">
          <h5 className="text-sm font-semibold text-cds-ink">Outcome {label || index + 1}</h5>
          <div className="grid gap-3 sm:grid-cols-2"><Field label="Outcome label"><input style={inputStyle} className={control} value={label} disabled={judge.classification === "difficulty"} onChange={e => updateOutcome(index, e.target.value, outcome)} /></Field><OutcomeDescription disabled={judge.backend.type === "heuristic"} value={outcome.description} onChange={description => updateOutcome(index, label, { ...outcome, description })} /></div>
          <ModelEditor label="Selected model" value={outcome.model} connections={connections} onChange={model => updateOutcome(index, label, { ...outcome, model })} />
          <Field label="Fallback behavior"><select style={routeSelectStyle} className={control} value={outcome.fallbacks == null ? "inherit" : outcome.fallbacks.length ? "custom" : "stop"} onChange={e => updateOutcome(index, label, { ...outcome, fallbacks: e.target.value === "inherit" ? null : e.target.value === "stop" ? [] : [`${connections[0]?.id ?? ""}/`] })}><option value="inherit">Use selected model&apos;s route chain</option><option value="stop">Stop after selected model</option><option value="custom">Use outcome-specific chain</option></select></Field>
          {outcome.fallbacks != null && outcome.fallbacks.length > 0 && <ChainEditor value={outcome.fallbacks} connections={connections} onChange={fallbacks => updateOutcome(index, label, { ...outcome, fallbacks })} />}
          <Button disabled={judge.backend.type === "heuristic" && Object.keys(judge.outcomes).length <= 2} onClick={() => { const entries = Object.entries(judge.outcomes).filter((_, i) => i !== index); setJudge({ ...judge, outcomes: Object.fromEntries(judge.classification === "difficulty" ? entries.map(([, o], i) => [String(i), o]) : entries) }); }}>Remove outcome</Button>
        </div>)}
        <Button disabled={Object.keys(judge.outcomes).length >= (judge.backend.type === "heuristic" ? 2 : 32)} onClick={() => { let i = Object.keys(judge.outcomes).length; let label = judge.classification === "difficulty" ? String(i) : `task-${i + 1}`; while (Object.hasOwn(judge.outcomes, label)) label = `task-${++i + 1}`; setJudge({ ...judge, outcomes: { ...judge.outcomes, [label]: emptyOutcome(grid.default_target) } }); }}>Add outcome</Button>
        <div className="space-y-2">
          <Button disabled={!defaults} onClick={restoreDefaults}>Restore defaults</Button>
          <p className="text-xs text-cds-ink-secondary">Restores default categories and descriptions. Models and fallbacks are preserved for matching categories.</p>
        </div>
        <p className="text-xs text-cds-ink-secondary">Fallback precedence: matching request override (Chat Completions only), then outcome-specific chain, then the selected model&apos;s route chain. An explicit empty chain stops fallback. Targets must be concrete models, not route aliases; permissions and quotas still apply to every attempt.</p>
        </JudgeSection>
      </>}
      </PolicySection>
      {(editError || error) && <p role="alert" className="rounded border border-amber-900 bg-amber-950/30 p-3 text-sm text-amber-200">{editError || error}</p>}
    </>}
  </div>;
}
function HeuristicEditor({ value, onChange }: { value: Extract<JudgeBackend, { type: "heuristic" }>; onChange: (value: JudgeBackend) => void }) {
  return <div className="space-y-4 rounded bg-slate-950/40 p-3 text-sm text-cds-ink-medium">
    <HeuristicKeywordEditor value={value} onChange={onChange} />
    <label className="flex gap-2"><input type="checkbox" checked={value.code_blocks_are_hard ?? true} onChange={e => onChange({ ...value, code_blocks_are_hard: e.target.checked })} />Treat code blocks as hard</label>
    <Field label="Maximum easy input length (UTF-8 bytes; 0 disables)"><input type="number" min={0} max={4294967295} step={1} style={inputStyle} className={control} value={value.max_easy_input_bytes ?? 6000} onChange={e => onChange({ ...value, max_easy_input_bytes: e.target.valueAsNumber })} /></Field>
    <p className="text-xs text-cds-ink-secondary">Keywords match user messages without case sensitivity. Code blocks and length consider all messages. Any hard signal selects outcome 1; otherwise outcome 0.</p>
  </div>;
}
