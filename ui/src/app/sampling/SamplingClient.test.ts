import { describe, expect, it } from "vitest";
import {
  outcomeEditToValue,
  outcomeRuleValid,
  outcomeRulesFromPolicy,
  outcomeValueToEdit,
  matchRuleValid,
  matchRulesFromPolicy,
  rateValid,
} from "./SamplingClient";

describe("sampling outcome editor", () => {
  it("round-trips an explicit null rather than converting it into empty text", () => {
    const edit = outcomeValueToEdit(null);
    expect(edit).toMatchObject({ valueType: "null", valueText: "" });
    expect(outcomeEditToValue("null", edit.valueText)).toBeNull();

    const [rule] = outcomeRulesFromPolicy([
      { path: "stop_reason", op: "eq", value: null, rate: 1 },
    ]);
    expect(rule.valueType).toBe("null");
    expect(outcomeRuleValid(rule)).toBe(true);
  });

  it("prevents blank and non-finite values from becoming numbers", () => {
    for (const valueText of ["", " ", "NaN", "Infinity", "-Infinity"]) {
      expect(outcomeRuleValid({ path: "usage.output_tokens", op: "gt", valueType: "number", valueText, rateText: "1" })).toBe(false);
    }
    expect(outcomeRuleValid({ path: "usage.output_tokens", op: "gt", valueType: "number", valueText: "4.5", rateText: "1" })).toBe(true);
    expect(rateValid("")).toBe(false);
    expect(rateValid("Infinity")).toBe(false);
  });

  it("does not allow an unsupported legacy structured value to be saved unchanged", () => {
    const [rule] = outcomeRulesFromPolicy([
      { path: "content", op: "eq", value: { unsupported: true, value: ["old"] }, rate: 1 },
    ]);
    expect(rule.valueType).toBe("unsupported");
    expect(outcomeRuleValid(rule)).toBe(false);
  });
});

describe("sampling match rule editor", () => {
  it("round-trips exactly one selected target", () => {
    const [rule] = matchRulesFromPolicy([
      {
        applies_to: "usage_pool_id",
        value: "pool-internal",
        rate: 1,
        log_content: true,
      },
    ]);
    expect(rule).toMatchObject({
      appliesTo: "usage_pool_id",
      value: "pool-internal",
    });
    expect(matchRuleValid(rule)).toBe(true);
  });

  it("does not allow a missing target value to be saved", () => {
    expect(
      matchRuleValid({ appliesTo: "request_tag", value: "", rateText: "1", logContent: true }),
    ).toBe(false);
  });
});
