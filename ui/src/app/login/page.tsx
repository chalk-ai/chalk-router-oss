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
  return <section className="max-w-lg space-y-5">
    <h1 className="text-2xl font-semibold">Sign in to Chalk Router</h1>
    <p>Use your Chalk JWT with router administration permissions. Sessions last up to 15 minutes.</p>
    <form onSubmit={signIn} className="space-y-4">
      <label className="block">Chalk JWT<input name="token" type="password" required autoComplete="off" className="mt-1 block w-full rounded border border-gray-600 p-2" /></label>
      <label className="block">Environment ID<input name="environment" autoComplete="off" className="mt-1 block w-full rounded border border-gray-600 p-2" /><span className="text-sm text-gray-400">Required for cluster tokens that do not name an environment.</span></label>
      <label className="block">Playground API key (optional)<input name="inferenceKey" type="password" autoComplete="off" className="mt-1 block w-full rounded border border-gray-600 p-2" /><span className="text-sm text-gray-400">Use a restricted router key with only the models and budgets you need.</span></label>
      {error && <p role="alert" className="text-red-400">{error}</p>}
      <button disabled={busy} className="rounded bg-white px-4 py-2 text-gray-950 disabled:opacity-50">{busy ? "Signing in…" : "Sign in"}</button>
    </form>
  </section>;
}
