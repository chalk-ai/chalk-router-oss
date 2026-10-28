"use client";

import { useEffect, useState } from "react";
import { judgeBackendError, type JudgeBackend } from "@/lib/routeGrid";
import { validateJudgeBackend } from "@/lib/judgeValidation";

export type JudgeValidation = { checking: boolean; error: string | null; retry: () => void };

export function useJudgeValidation(backend: JudgeBackend | null | undefined): JudgeValidation {
  // Associate the result with this exact backend/target. A previous success must
  // never enable Save for a changed target, even before the effect runs.
  const key = backend && backend.type !== "heuristic" ? JSON.stringify(backend) : null;
  const issue = key ? judgeBackendError(backend) : null;
  const [revision, setRevision] = useState(0);
  const [result, setResult] = useState<{ key: string; revision: number; error: string | null } | null>(null);
  useEffect(() => {
    if (!key || issue) return;
    const controller = new AbortController();
    let current = true;
    const timeout = setTimeout(() => controller.abort(), 10000);
    // Debounce custom model-ID typing; selecting a different target cancels it.
    const delay = setTimeout(() => {
      validateJudgeBackend(JSON.parse(key), controller.signal).then(
        () => { if (current) setResult({ key, revision, error: null }); },
        error => { if (current) setResult({ key, revision, error: controller.signal.aborted ? "Judge compatibility check timed out. Try again." : error instanceof Error ? error.message : "Could not check judge compatibility. Try again." }); },
      ).finally(() => clearTimeout(timeout));
    }, 200);
    return () => { current = false; clearTimeout(delay); clearTimeout(timeout); controller.abort(); };
  }, [key, issue, revision]);
  const checking = !!key && !issue && (result?.key !== key || result?.revision !== revision);
  return { checking, error: issue ?? (key && !checking ? result?.error ?? null : null), retry: () => setRevision(value => value + 1) };
}
