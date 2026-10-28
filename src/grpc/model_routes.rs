#![allow(clippy::result_large_err)] // tonic::Status is the gRPC wire error

use fieldmask::{Maskable, SelfMaskable};
use tonic::{Request, Response, Status};

use crate::model_routes::{ModelRoute, RouteInput, RouteListFilter, RouteUpdate};
use crate::service::model_routes::{RouteDiagnostic, RouteWithDiagnostic};
use crate::AppState;

use super::conv;
use super::field_mask;
use super::proto;
use super::proto::model_route_service_server::ModelRouteService;

pub struct ModelRouteServer {
    pub state: AppState,
}

#[derive(Debug, Default, Maskable, SelfMaskable)]
struct RoutePatch {
    connection_id: String,
    upstream_model_id: String,
    published: bool,
    // Placeholder so `update_mask: ["routing"]` passes mask-path validation.
    // Its value is unused; routing is taken from the operation directly.
    routing: bool,
}

#[tonic::async_trait]
impl ModelRouteService for ModelRouteServer {
    async fn validate_route_judge_backend(
        &self,
        req: Request<proto::ValidateRouteJudgeBackendRequest>,
    ) -> Result<Response<proto::ValidateRouteJudgeBackendResponse>, Status> {
        let state = self.state.for_request(&req);
        let backend = judge_backend_from_proto(req.into_inner().backend)?;
        state
            .model_route_service()
            .validate_judge_backend(&backend)
            .await?;
        Ok(Response::new(proto::ValidateRouteJudgeBackendResponse {}))
    }

    async fn get_route_judge_defaults(
        &self,
        _req: Request<proto::GetRouteJudgeDefaultsRequest>,
    ) -> Result<Response<proto::GetRouteJudgeDefaultsResponse>, Status> {
        let defaults = |classification: crate::route_grid::Classification| {
            classification
                .default_outcomes()
                .iter()
                .map(|(label, description)| proto::JudgeOutcomeDefault {
                    label: (*label).into(),
                    description: (*description).into(),
                })
                .collect()
        };
        Ok(Response::new(proto::GetRouteJudgeDefaultsResponse {
            difficulty: defaults(crate::route_grid::Classification::Difficulty),
            task: defaults(crate::route_grid::Classification::Task),
        }))
    }

    async fn list_model_routes(
        &self,
        req: Request<proto::ListModelRoutesRequest>,
    ) -> Result<Response<proto::ListModelRoutesResponse>, Status> {
        let state = self.state.for_request(&req);
        let body = req.into_inner();
        let filter = body.filters.unwrap_or_default();
        let (routes, next_cursor) = state
            .model_route_service()
            .list_page(
                body.limit,
                body.cursor,
                RouteListFilter {
                    connection_ids: filter.connection_ids,
                    published: filter.published,
                },
            )
            .await?;
        Ok(Response::new(proto::ListModelRoutesResponse {
            routes: routes.iter().map(route_with_diag_to_proto).collect(),
            next_cursor,
        }))
    }

    async fn create_model_route(
        &self,
        req: Request<proto::CreateModelRouteRequest>,
    ) -> Result<Response<proto::CreateModelRouteResponse>, Status> {
        let state = self.state.for_request(&req);
        let body = req.into_inner();
        let input = RouteInput {
            routing: body.routing.map(routing_from_proto).transpose()?,
            public_model_id: body.public_model_id,
            connection_id: body.connection_id,
            upstream_model_id: body.upstream_model_id,
            published: body.published,
        };
        let route = state.model_route_service().create(input).await?;
        state.backend.invalidate_routing_cache().await;
        Ok(Response::new(proto::CreateModelRouteResponse {
            route: Some(route_with_diag_to_proto(&route)),
        }))
    }

    async fn update_model_route(
        &self,
        req: Request<proto::UpdateModelRouteRequest>,
    ) -> Result<Response<proto::UpdateModelRouteResponse>, Status> {
        let state = self.state.for_request(&req);
        let body = req.into_inner();
        let op = body.update.unwrap_or_default();
        let mut populated = Vec::new();
        if op.connection_id.is_some() {
            populated.push("connection_id");
        }
        if op.upstream_model_id.is_some() {
            populated.push("upstream_model_id");
        }
        if op.published.is_some() {
            populated.push("published");
        }
        if op.routing.is_some() {
            populated.push("routing");
        }
        let resolved = field_mask::resolve(&body.update_mask, &populated)?;
        let had_routing = op.routing.is_some();

        let upd = if resolved.is_noop() {
            RouteUpdate::default()
        } else {
            let mut target = RoutePatch::default();
            let source = RoutePatch {
                connection_id: field_mask::trim_nonempty(op.connection_id).unwrap_or_default(),
                upstream_model_id: field_mask::trim_nonempty(op.upstream_model_id)
                    .unwrap_or_default(),
                published: op.published.unwrap_or_default(),
                routing: false,
            };
            field_mask::apply(&resolved, &mut target, source)?;

            field_mask::reject_if_blank(&resolved, "connection_id", &target.connection_id)?;
            field_mask::reject_if_blank(&resolved, "upstream_model_id", &target.upstream_model_id)?;

            let routing = if resolved.contains("routing") {
                field_mask::clearable_update(
                    &resolved,
                    "routing",
                    had_routing,
                    op.routing.map(routing_from_proto).transpose()?,
                )
            } else {
                None
            };

            RouteUpdate {
                routing,
                connection_id: resolved
                    .contains("connection_id")
                    .then_some(target.connection_id),
                upstream_model_id: resolved
                    .contains("upstream_model_id")
                    .then_some(target.upstream_model_id),
                published: resolved.contains("published").then_some(target.published),
            }
        };

        let route = state.model_route_service().update(&body.id, upd).await?;
        state.backend.invalidate_routing_cache().await;
        Ok(Response::new(proto::UpdateModelRouteResponse {
            route: Some(route_with_diag_to_proto(&route)),
        }))
    }

    async fn delete_model_route(
        &self,
        req: Request<proto::DeleteModelRouteRequest>,
    ) -> Result<Response<proto::DeleteModelRouteResponse>, Status> {
        let state = self.state.for_request(&req);
        let body = req.into_inner();

        state.model_route_service().delete(&body.id).await?;
        state.backend.invalidate_routing_cache().await;
        // Policies cannot reference aliases, so deletion cannot strand another grid.
        let warnings = Vec::new();

        Ok(Response::new(proto::DeleteModelRouteResponse {
            id: body.id,
            warnings,
        }))
    }
}

// ---------------------------------------------------------------------------
// proto <-> domain conversions (local to this file)
// ---------------------------------------------------------------------------

fn diagnostic_to_proto(d: RouteDiagnostic) -> i32 {
    let value = match d {
        RouteDiagnostic::Ready => proto::RouteDiagnostic::Ready,
        RouteDiagnostic::Unconfigured => proto::RouteDiagnostic::Unconfigured,
        RouteDiagnostic::Disabled => proto::RouteDiagnostic::Disabled,
        RouteDiagnostic::Unreadable => proto::RouteDiagnostic::Unreadable,
        RouteDiagnostic::MissingConnection => proto::RouteDiagnostic::MissingConnection,
        RouteDiagnostic::Unknown => proto::RouteDiagnostic::Unspecified,
    };
    value as i32
}

fn route_to_proto(route: &ModelRoute, diagnostic: i32) -> proto::ModelRoute {
    proto::ModelRoute {
        id: route.id.clone(),
        public_model_id: route.public_model_id.clone(),
        connection_id: route.connection_id.clone(),
        upstream_model_id: route.upstream_model_id.clone(),
        published: route.published,
        diagnostic,
        routing: route.routing.as_ref().map(routing_to_proto),
        created_at: Some(conv::ts_to_proto(route.created_at)),
        updated_at: Some(conv::ts_to_proto(route.updated_at)),
    }
}

fn route_with_diag_to_proto(rd: &RouteWithDiagnostic) -> proto::ModelRoute {
    route_to_proto(&rd.route, diagnostic_to_proto(rd.diagnostic))
}

fn judge_backend_from_proto(
    backend: Option<proto::RouteJudgeBackend>,
) -> Result<crate::route_grid::RouteJudgeBackend, Status> {
    use crate::route_grid::{
        CompletionJudgeBackend, DecisionsJudgeBackend, HeuristicJudgeBackend, RouteJudgeBackend,
    };
    Ok(
        match backend
            .and_then(|b| b.kind)
            .ok_or_else(|| Status::invalid_argument("judge backend must be configured"))?
        {
            proto::route_judge_backend::Kind::Heuristic(config) => {
                RouteJudgeBackend::Heuristic(HeuristicJudgeBackend {
                    use_default_keywords: config.use_default_keywords,
                    added_hard_keywords: config.added_hard_keywords,
                    removed_hard_keywords: config.removed_hard_keywords,
                    code_blocks_are_hard: config.code_blocks_are_hard,
                    max_easy_input_bytes: config.max_easy_input_bytes,
                })
            }
            proto::route_judge_backend::Kind::Completion(config) => {
                RouteJudgeBackend::Completion(CompletionJudgeBackend {
                    target: target_from_proto(config.target, "completion judge target")?,
                    timeout_ms: normalize_timeout(config.timeout_ms),
                })
            }
            proto::route_judge_backend::Kind::Decisions(config) => {
                RouteJudgeBackend::Decisions(DecisionsJudgeBackend {
                    target: target_from_proto(config.target, "decisions judge target")?,
                    timeout_ms: normalize_timeout(config.timeout_ms),
                })
            }
        },
    )
}

fn routing_from_proto(value: proto::RouteGrid) -> Result<crate::route_grid::RouteGrid, Status> {
    use crate::route_grid::{Classification, JudgeMode, JudgeOutcomeConfig, RouteGrid, RouteJudge};
    let default_target = target_from_proto(value.default_target, "routing default target")?;
    let judge = value
        .judge
        .map(|j| -> Result<RouteJudge, Status> {
            Ok(RouteJudge {
                mode: match proto::JudgeMode::try_from(j.mode)
                    .map_err(|_| Status::invalid_argument("unknown judge mode"))?
                {
                    proto::JudgeMode::Unspecified | proto::JudgeMode::Off => JudgeMode::Off,
                    proto::JudgeMode::Shadow => JudgeMode::Shadow,
                    proto::JudgeMode::Live => JudgeMode::Live,
                },
                classification: match proto::Classification::try_from(j.classification)
                    .map_err(|_| Status::invalid_argument("unknown classification"))?
                {
                    proto::Classification::Unspecified => {
                        return Err(Status::invalid_argument(
                            "judge classification must be configured",
                        ))
                    }
                    proto::Classification::Task => Classification::Task,
                    proto::Classification::Difficulty => Classification::Difficulty,
                },
                backend: judge_backend_from_proto(j.backend)?,
                outcomes: j
                    .outcomes
                    .into_iter()
                    .map(|(label, outcome)| {
                        (
                            label,
                            JudgeOutcomeConfig {
                                model: outcome.model,
                                description: outcome.description,
                                fallbacks: outcome.fallbacks.map(|f| f.models),
                            },
                        )
                    })
                    .collect(),
            })
        })
        .transpose()?;
    let grid = RouteGrid {
        default_target,
        judge,
        fallbacks: value
            .fallbacks
            .into_iter()
            .map(|(model, f)| (model, f.models))
            .collect(),
    };
    grid.validate().map_err(Status::invalid_argument)?;
    Ok(grid)
}

fn routing_to_proto(grid: &crate::route_grid::RouteGrid) -> proto::RouteGrid {
    use crate::route_grid::{Classification, JudgeMode, RouteJudgeBackend};
    proto::RouteGrid {
        default_target: Some(target_to_proto(&grid.default_target)),
        fallbacks: grid
            .fallbacks
            .iter()
            .map(|(model, alternatives)| {
                (
                    model.clone(),
                    proto::RouteFallbacks {
                        models: alternatives.clone(),
                    },
                )
            })
            .collect(),
        judge: grid.judge.as_ref().map(|j| proto::RouteJudge {
            mode: match j.mode {
                JudgeMode::Off => proto::JudgeMode::Off,
                JudgeMode::Shadow => proto::JudgeMode::Shadow,
                JudgeMode::Live => proto::JudgeMode::Live,
            } as i32,
            classification: match j.classification {
                Classification::Task => proto::Classification::Task,
                Classification::Difficulty => proto::Classification::Difficulty,
            } as i32,
            backend: Some(proto::RouteJudgeBackend {
                kind: Some(match &j.backend {
                    RouteJudgeBackend::Heuristic(config) => {
                        proto::route_judge_backend::Kind::Heuristic(proto::HeuristicJudgeBackend {
                            use_default_keywords: config.use_default_keywords,
                            added_hard_keywords: config.added_hard_keywords.clone(),
                            removed_hard_keywords: config.removed_hard_keywords.clone(),
                            code_blocks_are_hard: config.code_blocks_are_hard,
                            max_easy_input_bytes: config.max_easy_input_bytes,
                        })
                    }
                    RouteJudgeBackend::Completion(config) => {
                        proto::route_judge_backend::Kind::Completion(
                            proto::CompletionJudgeBackend {
                                target: Some(target_to_proto(&config.target)),
                                timeout_ms: config.timeout_ms as u32,
                            },
                        )
                    }
                    RouteJudgeBackend::Decisions(config) => {
                        proto::route_judge_backend::Kind::Decisions(proto::DecisionsJudgeBackend {
                            target: Some(target_to_proto(&config.target)),
                            timeout_ms: config.timeout_ms as u32,
                        })
                    }
                }),
            }),
            outcomes: j
                .outcomes
                .iter()
                .map(|(label, outcome)| {
                    (
                        label.clone(),
                        proto::JudgeOutcomeConfig {
                            model: outcome.model.clone(),
                            description: outcome.description.clone(),
                            fallbacks: outcome.fallbacks.as_ref().map(|models| {
                                proto::RouteFallbacks {
                                    models: models.clone(),
                                }
                            }),
                        },
                    )
                })
                .collect(),
        }),
    }
}

fn normalize_timeout(timeout_ms: u32) -> u64 {
    if timeout_ms == 0 {
        crate::route_grid::DEFAULT_JUDGE_TIMEOUT_MS
    } else {
        timeout_ms.into()
    }
}

fn target_from_proto(
    value: Option<proto::RouteTarget>,
    label: &str,
) -> Result<crate::route_grid::RouteTarget, Status> {
    let value =
        value.ok_or_else(|| Status::invalid_argument(format!("{label} must be configured")))?;
    let target = crate::route_grid::RouteTarget {
        connection_id: value.connection_id,
        upstream_model_id: value.upstream_model_id,
    };
    target
        .validate()
        .map_err(|error| Status::invalid_argument(format!("invalid {label}: {error}")))?;
    Ok(target)
}

fn target_to_proto(value: &crate::route_grid::RouteTarget) -> proto::RouteTarget {
    proto::RouteTarget {
        connection_id: value.connection_id.clone(),
        upstream_model_id: value.upstream_model_id.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::route_grid::{
        Classification, CompletionJudgeBackend, DecisionsJudgeBackend, HeuristicJudgeBackend,
        JudgeOutcomeConfig, RouteGrid, RouteJudge, RouteJudgeBackend, RouteTarget,
    };

    fn target(connection: &str, model: &str) -> RouteTarget {
        RouteTarget {
            connection_id: connection.into(),
            upstream_model_id: model.into(),
        }
    }

    fn grid(backend: RouteJudgeBackend) -> RouteGrid {
        RouteGrid {
            default_target: target("serving", "astra"),
            judge: Some(RouteJudge {
                backend,
                classification: Classification::Difficulty,
                outcomes: ["0", "1"]
                    .into_iter()
                    .map(|label| {
                        (
                            label.into(),
                            JudgeOutcomeConfig {
                                model: "serving/astra".into(),
                                description: String::new(),
                                fallbacks: None,
                            },
                        )
                    })
                    .collect(),
                ..Default::default()
            }),
            fallbacks: Default::default(),
        }
    }

    #[test]
    fn every_backend_roundtrips_with_its_target_and_configuration() {
        for backend in [
            RouteJudgeBackend::Heuristic(HeuristicJudgeBackend {
                use_default_keywords: Some(false),
                added_hard_keywords: vec!["custom".into()],
                removed_hard_keywords: vec!["algorithm".into()],
                code_blocks_are_hard: Some(false),
                max_easy_input_bytes: Some(0),
            }),
            RouteJudgeBackend::Completion(CompletionJudgeBackend {
                target: target("classifier", "completion"),
                timeout_ms: 1234,
            }),
            RouteJudgeBackend::Decisions(DecisionsJudgeBackend {
                target: target("classifier", "decisions"),
                timeout_ms: 2345,
            }),
        ] {
            let mut grid = grid(backend);
            grid.judge
                .as_mut()
                .unwrap()
                .outcomes
                .get_mut("1")
                .unwrap()
                .fallbacks = Some(vec![]);
            assert_eq!(routing_from_proto(routing_to_proto(&grid)).unwrap(), grid);
            assert_eq!(
                grid.references_connection("classifier"),
                grid.judge.as_ref().unwrap().backend.target().is_some()
            );
            assert!(grid.references_connection("serving"));
            assert!(!grid.references_connection("class"));
        }
    }

    #[test]
    fn backend_configuration_and_concrete_remote_target_are_required() {
        for backend in [None, Some(proto::RouteJudgeBackend { kind: None })] {
            let mut wire = routing_to_proto(&grid(RouteJudgeBackend::default()));
            wire.judge.as_mut().unwrap().backend = backend;
            assert_eq!(
                routing_from_proto(wire).unwrap_err().code(),
                tonic::Code::InvalidArgument
            );
        }
        for target in [
            None,
            Some(proto::RouteTarget::default()),
            Some(proto::RouteTarget {
                connection_id: "classifier".into(),
                upstream_model_id: String::new(),
            }),
            Some(proto::RouteTarget {
                connection_id: String::new(),
                upstream_model_id: "judge".into(),
            }),
            Some(proto::RouteTarget {
                connection_id: "chalk".into(),
                upstream_model_id: "alias".into(),
            }),
        ] {
            for kind in [
                proto::route_judge_backend::Kind::Completion(proto::CompletionJudgeBackend {
                    target: target.clone(),
                    timeout_ms: 0,
                }),
                proto::route_judge_backend::Kind::Decisions(proto::DecisionsJudgeBackend {
                    target: target.clone(),
                    timeout_ms: 0,
                }),
            ] {
                let mut wire = routing_to_proto(&grid(RouteJudgeBackend::default()));
                wire.judge.as_mut().unwrap().backend =
                    Some(proto::RouteJudgeBackend { kind: Some(kind) });
                assert_eq!(
                    routing_from_proto(wire).unwrap_err().code(),
                    tonic::Code::InvalidArgument
                );
            }
        }
    }

    #[test]
    fn explicit_custom_routing_requires_its_own_default_target() {
        assert_eq!(
            routing_from_proto(proto::RouteGrid::default())
                .unwrap_err()
                .code(),
            tonic::Code::InvalidArgument
        );
        for target in [
            None,
            Some(proto::RouteTarget::default()),
            Some(proto::RouteTarget {
                connection_id: "serving".into(),
                upstream_model_id: String::new(),
            }),
            Some(proto::RouteTarget {
                connection_id: String::new(),
                upstream_model_id: "astra".into(),
            }),
        ] {
            let mut wire = routing_to_proto(&grid(RouteJudgeBackend::default()));
            wire.default_target = target;
            assert_eq!(
                routing_from_proto(wire).unwrap_err().code(),
                tonic::Code::InvalidArgument
            );
        }
    }

    #[test]
    fn unspecified_mode_defaults_off_but_classification_must_be_explicit() {
        let mut wire = routing_to_proto(&grid(RouteJudgeBackend::default()));
        wire.judge.as_mut().unwrap().mode = proto::JudgeMode::Unspecified as i32;
        assert_eq!(
            routing_from_proto(wire.clone())
                .unwrap()
                .judge
                .unwrap()
                .mode,
            crate::route_grid::JudgeMode::Off
        );

        // Even a disabled judge must carry a valid, explicitly chosen classification.
        for mode in [
            proto::JudgeMode::Unspecified,
            proto::JudgeMode::Off,
            proto::JudgeMode::Shadow,
            proto::JudgeMode::Live,
        ] {
            wire.judge.as_mut().unwrap().mode = mode as i32;
            wire.judge.as_mut().unwrap().classification = proto::Classification::Unspecified as i32;
            let error = routing_from_proto(wire.clone()).unwrap_err();
            assert_eq!(error.code(), tonic::Code::InvalidArgument);
            assert_eq!(error.message(), "judge classification must be configured");
        }
    }

    #[test]
    fn unknown_judge_enum_values_are_rejected() {
        for unknown in [-1, 999] {
            let mut wire = routing_to_proto(&grid(RouteJudgeBackend::default()));
            wire.judge.as_mut().unwrap().mode = unknown;
            let error = routing_from_proto(wire).unwrap_err();
            assert_eq!(error.code(), tonic::Code::InvalidArgument);
            assert_eq!(error.message(), "unknown judge mode");

            let mut wire = routing_to_proto(&grid(RouteJudgeBackend::default()));
            wire.judge.as_mut().unwrap().classification = unknown;
            let error = routing_from_proto(wire).unwrap_err();
            assert_eq!(error.code(), tonic::Code::InvalidArgument);
            assert_eq!(error.message(), "unknown classification");
        }
    }

    #[test]
    fn default_target_without_judge_is_valid_custom_routing() {
        let wire = proto::RouteGrid {
            default_target: Some(proto::RouteTarget {
                connection_id: "serving".into(),
                upstream_model_id: "astra".into(),
            }),
            ..Default::default()
        };
        let parsed = routing_from_proto(wire.clone()).unwrap();
        assert!(parsed.judge.is_none());
        assert_eq!(routing_to_proto(&parsed), wire);
    }

    #[test]
    fn heuristic_option_presence_survives_roundtrip() {
        for config in [
            HeuristicJudgeBackend::default(),
            HeuristicJudgeBackend {
                use_default_keywords: Some(false),
                code_blocks_are_hard: Some(false),
                max_easy_input_bytes: Some(0),
                ..Default::default()
            },
            HeuristicJudgeBackend {
                use_default_keywords: Some(true),
                code_blocks_are_hard: Some(true),
                max_easy_input_bytes: Some(6000),
                ..Default::default()
            },
        ] {
            let original = grid(RouteJudgeBackend::Heuristic(config));
            assert_eq!(
                routing_from_proto(routing_to_proto(&original)).unwrap(),
                original
            );
        }
    }

    #[test]
    fn zero_remote_timeout_uses_default_for_both_backends() {
        for backend in [
            RouteJudgeBackend::Completion(CompletionJudgeBackend {
                target: target("classifier", "completion"),
                timeout_ms: 0,
            }),
            RouteJudgeBackend::Decisions(DecisionsJudgeBackend {
                target: target("classifier", "decisions"),
                timeout_ms: 0,
            }),
        ] {
            let original = grid(backend);
            let parsed = routing_from_proto(routing_to_proto(&original)).unwrap();
            match parsed.judge.unwrap().backend {
                RouteJudgeBackend::Completion(config) => assert_eq!(config.timeout_ms, 1000),
                RouteJudgeBackend::Decisions(config) => assert_eq!(config.timeout_ms, 1000),
                RouteJudgeBackend::Heuristic(_) => panic!("remote backend changed transport"),
            }
        }
    }
}
