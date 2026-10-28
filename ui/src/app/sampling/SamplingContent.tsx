import { samplingPolicyClient } from "@/lib/grpc";
import { samplingPolicyToJson, type SamplingPolicyJson } from "@/lib/serialize";
import { SamplingClient } from "./SamplingClient";


const EMPTY: SamplingPolicyJson = {
  enabled: false,
  default_rate: 1,
  default_log_content: false,
  min_conversation_rate: 0,
  rules: [],
  outcome_rules: [],
};

export async function SamplingContent({ embedded = false }: { embedded?: boolean } = {}) {
  let policy: SamplingPolicyJson = EMPTY;
  let error: string | null = null;

  try {
    const res = await samplingPolicyClient().getSamplingPolicy({});
    if (res.policy) policy = samplingPolicyToJson(res.policy);
  } catch (e) {
    error = e instanceof Error ? e.message : String(e);
  }

  return (
    <div className="mx-auto max-w-[1080px]">
      {error && (
        <div
          className="mb-6 px-4 py-3 rounded-lg text-sm"
          style={{ background: "#1c0a0a", border: "1px solid #7f1d1d", color: "#fca5a5" }}
        >
          <span className="font-medium">Error:</span> {error}
        </div>
      )}

      <SamplingClient initialPolicy={policy} />
    </div>
  );
}


