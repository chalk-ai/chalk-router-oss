import { withSession } from "@/lib/auth";
import { isOutcomeScalar, samplingRuleValidationError } from "./validation";
import { NextRequest, NextResponse } from "next/server";
import { samplingPolicyClient } from "@/lib/grpc";
import {
  outcomeComparatorFromJson,
  samplingPolicyToJson,
  type OutcomeRuleJson,
  type SamplingPolicyJson,
  type SamplingRuleJson,
} from "@/lib/serialize";
import { connectErrorResponse } from "@/lib/connectError";
import { parseJsonObjectBody } from "@/lib/bff";
import { fromJson } from "@bufbuild/protobuf";
import { ValueSchema } from "@bufbuild/protobuf/wkt";

const EMPTY: SamplingPolicyJson = {
  enabled: false,
  default_rate: 1,
  default_log_content: true,
  min_conversation_rate: 0,
  rules: [],
  outcome_rules: [],
};

async function handleGET() {
  try {
    const res = await samplingPolicyClient().getSamplingPolicy({});
    return NextResponse.json(res.policy ? samplingPolicyToJson(res.policy) : EMPTY);
  } catch (e) {
    return connectErrorResponse(e);
  }
}

async function handlePUT(req: NextRequest) {
  try {
    const parsed = await parseJsonObjectBody(req);
    if ("error" in parsed) return parsed.error;
    const { body } = parsed;
    if (body.rules !== undefined && !Array.isArray(body.rules)) {
      return NextResponse.json(
        { error: { message: "rules must be an array" } },
        { status: 400 },
      );
    }
    const rules = (body.rules as SamplingRuleJson[] | undefined) ?? [];
    const outcomeRules = (body.outcome_rules as OutcomeRuleJson[] | undefined) ?? [];
    for (const [index, rule] of rules.entries()) {
      const message = samplingRuleValidationError(rule, index);
      if (message) {
        return NextResponse.json({ error: { message } }, { status: 400 });
      }
    }
    for (const [index, rule] of outcomeRules.entries()) {
      if (!isOutcomeScalar(rule.value)) {
        return NextResponse.json(
          {
            error: {
              message: `outcome_rules[${index}].value must be null, a string, a finite number, or a boolean`,
            },
          },
          { status: 400 },
        );
      }
    }
    const res = await samplingPolicyClient().updateSamplingPolicy({
      update: {
        enabled: !!body.enabled,
        defaultRate: Number(body.default_rate ?? 0),
        defaultLogContent: !!body.default_log_content,
        minConversationRate: Number(body.min_conversation_rate ?? 0),
        // Whole-list replacement, same as `rules`: order is precedence, so a
        // partial merge could not express a reorder or removal unambiguously.
        rules: rules.map((rule) => {
          const value = rule.value.trim();
          const appliesTo =
            rule.applies_to === "request_tag"
              ? { case: "requestTag" as const, value }
              : rule.applies_to === "key_id"
                ? { case: "keyId" as const, value }
                : { case: "usagePoolId" as const, value };
          return {
            appliesTo,
            rate: Number(rule.rate ?? 0),
            logContent: !!rule.log_content,
          };
        }),
        outcomeRules: outcomeRules.map((rule) => ({
          path: (rule.path ?? "").trim(),
          op: outcomeComparatorFromJson(rule.op),
          // Validated above: `google.protobuf.Value` receives an explicit,
          // lossless scalar rather than a structured value that would later
          // be misrepresented by the editor.
          value: fromJson(
            ValueSchema,
            rule.value as string | number | boolean | null,
          ),
          rate: Number(rule.rate ?? 0),
        })),
      },
      updateMask: {
        paths: [
          "enabled",
          "default_rate",
          "default_log_content",
          "min_conversation_rate",
          "rules",
          "outcome_rules",
        ],
      },
    });
    return NextResponse.json(res.policy ? samplingPolicyToJson(res.policy) : EMPTY);
  } catch (e) {
    return connectErrorResponse(e);
  }
}

export const GET = withSession(handleGET);
export const PUT = withSession(handlePUT);
