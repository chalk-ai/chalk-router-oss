---
title: Models, routes, and capabilities
---

`GET /v1/models` returns a [ListResponse](/reference/http-types/#listresponse) of [Model](/reference/http-types/#model) entries. Restricted keys see only granted direct models and routes. `GET /v1/models/{model_id}` retrieves one visible model. These handlers expose no collection pagination parameters.

IDs can name a configured provider model, a concrete `connection_id/upstream_model_id`, or a `chalk/*` public model route. Connection IDs select a configured connection, not a secret or generic provider hostname. Public routes are configured with the [ModelRouteService](/reference/grpc-model-route/#modelrouteservice), and can own a default target, judging, outcomes, and fallbacks.

## Capabilities

The additive `capabilities` object describes effective protocols/operations, modalities, limits, features, and delivery modes. [Generated capability types](/reference/capability-types/) define the current vocabulary. Native/adapted/translated/unsupported/unknown states should be interpreted per operation. An unknown capability is not a universal guarantee of support. The router combines catalog information, connection declarations, and model/route facts; handlers check the features actually requested.

`GET /v1/models/projections/vscode` returns a configuration projection for accessible models. Optional query `url` stamps the public Responses ingress URL into entries; it defaults to `/v1/responses`. Returned entries use camelCase fields such as `toolCalling`. Authentication headers remain the client's responsibility. [Projection types](/reference/model-projection-types/).

## Model deletion

`DELETE /v1/models/{model_id}` returns 501 after normal inference authentication.
It never deletes aliases or provider models, including for static admin credentials
and anonymous development mode. Use `ModelRouteService/DeleteModelRoute` with
an authorized admin credential for router aliases.

Sources: [model handlers](https://github.com/chalk-ai/chalk-router/blob/main/src/handlers/models.rs), [routing types](/reference/routing-types/), [capability source](https://github.com/chalk-ai/chalk-router/blob/main/src/capabilities.rs).
