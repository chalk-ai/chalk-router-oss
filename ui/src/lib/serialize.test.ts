import { CacheScope } from "@/gen/chalk/router/v1/api_key_pb";
import {
  OutcomeComparator,
  OutcomeRuleSchema,
  SamplingRuleSchema,
} from "@/gen/chalk/router/v1/sampling_policy_pb";
import { ExposurePolicy } from "@/gen/chalk/router/v1/provider_connection_pb";
import { UsageBudgetPeriod } from "@/gen/chalk/router/v1/usage_budget_pb";
import { create, fromJson } from "@bufbuild/protobuf";
import { ValueSchema } from "@bufbuild/protobuf/wkt";
import { describe, expect, it } from "vitest";
import {
  cacheScopeFromString,
  cacheScopeToString,
  exposureFromString,
  exposureToString,
  outcomeRuleToJson,
  samplingRuleToJson,
  usageBudgetPeriodFromJson,
  usageBudgetPeriodToJson,
} from "./serialize";

describe("outcome rule serialization", () => {
  it("keeps an explicit null as null", () => {
    const rule = create(OutcomeRuleSchema, {
      path: "stop_reason",
      op: OutcomeComparator.EQ,
      value: fromJson(ValueSchema, null),
      rate: 1,
    });
    expect(outcomeRuleToJson(rule).value).toBeNull();
  });

  it("preserves unsupported structured values instead of rewriting them as null", () => {
    const rule = create(OutcomeRuleSchema, {
      path: "content",
      op: OutcomeComparator.EQ,
      value: fromJson(ValueSchema, { nested: ["old"] }),
      rate: 1,
    });
    expect(outcomeRuleToJson(rule).value).toEqual({
      unsupported: true,
      value: { nested: ["old"] },
    });
  });
});

describe("sampling rule serialization", () => {
  it("preserves the selected oneof target and value", () => {
    const rules = [
      ["requestTag", "nightly"],
      ["keyId", "ck-123"],
      ["usagePoolId", "pool-internal"],
    ] as const;
    for (const [target, value] of rules) {
      const rule = create(SamplingRuleSchema, {
        appliesTo: { case: target, value },
        rate: 0.5,
        logContent: true,
      });
      expect(samplingRuleToJson(rule)).toMatchObject({
        applies_to:
          target === "requestTag"
            ? "request_tag"
            : target === "keyId"
              ? "key_id"
              : "usage_pool_id",
        value,
      });
    }
  });

  it("rejects a rule without a target", () => {
    expect(() => samplingRuleToJson(create(SamplingRuleSchema))).toThrow(
      "sampling rule is missing its target",
    );
  });
});

describe("usage budget period mapping", () => {
  it("round-trips every named period through the JSON string form", () => {
    for (const period of [
      UsageBudgetPeriod.DAILY,
      UsageBudgetPeriod.WEEKLY,
      UsageBudgetPeriod.MONTHLY,
    ]) {
      const json = usageBudgetPeriodToJson(period);
      expect(usageBudgetPeriodFromJson(json)).toBe(period);
    }
  });

  it("defaults an unspecified enum value to daily", () => {
    expect(usageBudgetPeriodToJson(UsageBudgetPeriod.UNSPECIFIED)).toBe("daily");
  });

  it("returns undefined for an unrecognized JSON string", () => {
    expect(usageBudgetPeriodFromJson("yearly")).toBeUndefined();
    expect(usageBudgetPeriodFromJson(undefined)).toBeUndefined();
  });
});

describe("cache scope mapping", () => {
  it("round-trips every named scope", () => {
    for (const scope of [CacheScope.TOKEN, CacheScope.MODEL, CacheScope.TEAM]) {
      const json = cacheScopeToString(scope);
      expect(cacheScopeFromString(json)).toBe(scope);
    }
  });

  it("defaults an unrecognized string to the narrowest scope (token)", () => {
    expect(cacheScopeFromString("nonsense")).toBe(CacheScope.TOKEN);
    expect(cacheScopeFromString(undefined)).toBe(CacheScope.TOKEN);
  });
});

describe("exposure policy mapping", () => {
  it("round-trips every named policy", () => {
    for (const exposure of [
      ExposurePolicy.DYNAMIC,
      ExposurePolicy.UNLISTED,
      ExposurePolicy.EXPLICIT,
    ]) {
      const json = exposureToString(exposure);
      expect(exposureFromString(json)).toBe(exposure);
    }
  });

  it("defaults an unspecified enum value to the unspecified string", () => {
    expect(exposureToString(ExposurePolicy.UNSPECIFIED)).toBe("unspecified");
    expect(exposureFromString(undefined)).toBe(ExposurePolicy.UNSPECIFIED);
  });
});
