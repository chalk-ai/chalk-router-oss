/**
 * Connect / gRPC-Web client for the chalk-router admin/management API.
 *
 * Calls run in server components and route handlers using the signed-in
 * user credential and the Node transport
 * with gRPC-Web framing. The server is tonic + tonic-web on the same port
 * as the legacy HTTP routes.
 */
import { currentSession } from "./auth";
import { createClient, type Client, type Interceptor } from "@connectrpc/connect";
import { createGrpcWebTransport } from "@connectrpc/connect-node";
import { ApiKeyService } from "@/gen/chalk/router/v1/api_key_pb";
import { ModelRouteService } from "@/gen/chalk/router/v1/model_route_pb";
import { ProviderConnectionService } from "@/gen/chalk/router/v1/provider_connection_pb";
import { RateLimitService } from "@/gen/chalk/router/v1/rate_limit_pb";
import { SamplingPolicyService } from "@/gen/chalk/router/v1/sampling_policy_pb";
import { TelemetrySettingsService } from "@/gen/chalk/router/v1/telemetry_settings_pb";
import { UsageService } from "@/gen/chalk/router/v1/usage_pb";
import { UsagePoolService } from "@/gen/chalk/router/v1/usage_pool_pb";
import { UsageBudgetService } from "@/gen/chalk/router/v1/usage_budget_pb";

const CHALK_ROUTER_URL = process.env.CHALK_ROUTER_URL ?? "http://localhost:4001";

const authInterceptor: Interceptor = (next) => async (req) => {
  const session = await currentSession();
  req.header.set("authorization", `Bearer ${session.token}`);
  req.header.delete("x-chalk-env-id");
  if (session.environment) req.header.set("x-chalk-env-id", session.environment);
  return next(req);
};

// Cached transport — created on first use. We use gRPC-Web framing because
// that is what tonic_web::GrpcWebLayer accepts; it works over HTTP/1.1 so
// we do not need to coax HTTP/2 out of axum::serve.
let cachedTransport: ReturnType<typeof createGrpcWebTransport> | null = null;
function transport() {
  if (!cachedTransport) {
    cachedTransport = createGrpcWebTransport({
      baseUrl: CHALK_ROUTER_URL,
      httpVersion: "1.1",
      interceptors: [authInterceptor],
    });
  }
  return cachedTransport;
}

export function apiKeyClient(): Client<typeof ApiKeyService> {
  return createClient(ApiKeyService, transport());
}

export function usagePoolClient(): Client<typeof UsagePoolService> {
  return createClient(UsagePoolService, transport());
}

export function usageBudgetClient(): Client<typeof UsageBudgetService> {
  return createClient(UsageBudgetService, transport());
}

export function usageClient(): Client<typeof UsageService> {
  return createClient(UsageService, transport());
}

export function rateLimitClient(): Client<typeof RateLimitService> {
  return createClient(RateLimitService, transport());
}

export function samplingPolicyClient(): Client<typeof SamplingPolicyService> {
  return createClient(SamplingPolicyService, transport());
}

export function providerConnectionClient(): Client<typeof ProviderConnectionService> {
  return createClient(ProviderConnectionService, transport());
}

export function modelRouteClient(): Client<typeof ModelRouteService> {
  return createClient(ModelRouteService, transport());
}

export function telemetrySettingsClient(): Client<typeof TelemetrySettingsService> {
  return createClient(TelemetrySettingsService, transport());
}
