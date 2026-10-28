import { UsageBudgetPeriod } from "@/gen/chalk/router/v1/usage_budget_pb";
import { usageBudgetPeriodFromJson } from "@/lib/serialize";
import { parsePositiveBigInt, type BffResult } from "@/lib/bff";
import { NextResponse } from "next/server";

export { rateLimitsFromJson } from "@/lib/bff";

interface UsageBudgetJson {
  token_limit?: number | string;
  period?: string;
  enabled?: boolean;
}

export interface UsageBudgetProtoInput {
  tokenLimit: bigint;
  period: UsageBudgetPeriod;
  enabled?: boolean;
}

export function usageBudgetsFromJson(value: unknown): BffResult<UsageBudgetProtoInput[]> {
  if (value === undefined) return { value: [] };
  if (!Array.isArray(value)) {
    return {
      error: NextResponse.json({ error: { message: "usage_budgets must be an array" } }, { status: 400 }),
    };
  }
  const result: UsageBudgetProtoInput[] = [];
  for (let index = 0; index < value.length; index++) {
    const entry = value[index];
    if (typeof entry !== "object" || entry === null || Array.isArray(entry)) {
      return {
        error: NextResponse.json({ error: { message: `usage_budgets[${index}] must be an object` } }, { status: 400 }),
      };
    }
    const budget = entry as UsageBudgetJson;
    if (budget.enabled != null && typeof budget.enabled !== "boolean") {
      return {
        error: NextResponse.json({ error: { message: `usage_budgets[${index}].enabled must be a boolean` } }, { status: 400 }),
      };
    }
    const tokenLimit = parsePositiveBigInt(budget.token_limit, `usage_budgets[${index}].token_limit`);
    if ("error" in tokenLimit) return tokenLimit;
    const period = usageBudgetPeriodFromJson(budget.period);
    if (period === undefined) {
      return {
        error: NextResponse.json(
          { error: { message: `usage_budgets[${index}].period must be daily, weekly, or monthly` } },
          { status: 400 },
        ),
      };
    }
    result.push({
      tokenLimit: tokenLimit.value,
      period,
      enabled: budget.enabled,
    });
  }
  return { value: result };
}
