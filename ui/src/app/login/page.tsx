"use client";
import { useState, type FormEvent } from "react";

export default function Login() {
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  async function signIn(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const form = event.currentTarget;
    const data = new FormData(form);
    setBusy(true); setError("");
    try {
      const response = await fetch("/api/session", {
        method: "POST", headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ token: data.get("token"), environment: data.get("environment"), inferenceKey: data.get("inferenceKey") || undefined }),
      });
      if (!response.ok) throw new Error((await response.json()).error?.message ?? "Sign in failed");
      form.reset(); window.location.assign("/routing");
    } catch (error) { setError(error instanceof Error ? error.message : "Sign in failed"); }
    finally { setBusy(false); }
  }
  return <section className="card w-full max-w-md p-6">
    <h1 className="page-title">Sign in to Chalk Router</h1>
    <p className="mt-1 text-cds-md text-cds-ink-secondary">Use your Chalk JWT with router administration permissions. Sessions last up to 15 minutes.</p>
    <form onSubmit={signIn} className="mt-5 space-y-4">
      <label className="block"><span className="field-label">Chalk JWT</span><input name="token" type="password" required autoComplete="off" className="field" /></label>
      <label className="block"><span className="field-label">Environment ID</span><input name="environment" autoComplete="off" className="field" /><span className="mt-1 block text-cds-sm text-cds-ink-subtle">Required for cluster tokens that do not name an environment.</span></label>
      <label className="block"><span className="field-label">Playground API key (optional)</span><input name="inferenceKey" type="password" autoComplete="off" className="field" /><span className="mt-1 block text-cds-sm text-cds-ink-subtle">Use a restricted router key with only the models and budgets you need.</span></label>
      {error && <p role="alert" className="text-cds-md text-cds-error">{error}</p>}
      <button disabled={busy} className="btn btn-primary btn-md w-full">{busy ? "Signing in…" : "Sign in"}</button>
    </form>
  </section>;
}
