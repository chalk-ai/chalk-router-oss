import { describe, expect, it } from "vitest";
import { isOutcomeScalar, samplingRuleValidationError } from "./validation";

describe("sampling policy BFF outcome values", () => {
  it("accepts exactly the supported scalar values", () => {
    for (const value of [null, "done", true, false, 0, -1.25]) {
      expect(isOutcomeScalar(value)).toBe(true);
    }
  });

  it("rejects structured and non-finite values before protobuf conversion", () => {
    for (const value of [[], { nested: true }, Number.NaN, Number.POSITIVE_INFINITY]) {
      expect(isOutcomeScalar(value)).toBe(false);
    }
  });
});

describe("sampling policy BFF match rules", () => {
  it("requires one recognized target and a nonblank value", () => {
    expect(
      samplingRuleValidationError(
        { applies_to: "request_tag", value: "nightly" },
        0,
      ),
    ).toBeNull();
    for (const rule of [
      {},
      { applies_to: "unsupported", value: "nightly" },
      { applies_to: "request_tag", value: "  " },
      { applies_to: "key_id", value: 42 },
      [],
    ]) {
      expect(samplingRuleValidationError(rule, 0)).not.toBeNull();
    }
  });
});
