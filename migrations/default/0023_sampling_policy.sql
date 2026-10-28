-- Per-environment trace sampling policy, mirroring chalk_router.judge_policy:
-- the whole admin-editable policy is a single JSONB document — the default rate
-- and content-logging setting plus the ordered match rules. A rate for one API
-- key or one usage pool is a rule inside that document, so neither
-- chalk_router.api_keys nor chalk_router.usage_pools needs a column of its own.
--
-- The '{}' default decodes to SamplingPolicy::default() (enabled = false), and
-- a disabled policy exports every LLM-frontend trace — an environment with no
-- row here, or a row nobody has edited yet, still gets 100% of its traces,
-- exactly as it did before this table existed. There is no separate
-- "tracing enabled" flag to set here or in the environment: OTEL_TRACES_SAMPLER
-- already governs every non-LLM-frontend route, and this policy only ever
-- thins the LLM-frontend traffic below the 100% it gets by default.
CREATE TABLE chalk_router.sampling_policy (
    environment_id TEXT PRIMARY KEY,
    policy         JSONB NOT NULL DEFAULT '{}'
);
