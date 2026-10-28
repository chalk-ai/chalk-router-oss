//! Apply `google.protobuf.FieldMask` updates via the [`fieldmask`] crate.
//!
//! Semantics:
//! - An omitted or empty mask implies all **populated** fields on the operation
//!   (so we never call [`Mask::update`] with an empty mask — that would mean
//!   full replace in the crate).
//! - `"*"` means full replacement (`Mask::full()`).
//! - Unknown paths are rejected when parsing into a [`Maskable`] type.
//! - Paths are matched against Rust/proto snake_case; lowerCamelCase is
//!   normalized first.
//!
//! Call sites derive [`Maskable`] + [`SelfMaskable`] on a patch struct, build a
//! `source` from the operation (unset → type default), and [`apply`] onto a
//! `target` copied from the current resource (RMW) or `Default` (partial
//! service update).

#![allow(clippy::result_large_err)] // tonic::Status is the gRPC wire error

use fieldmask::{Mask, MaskInput, Maskable, SelfMaskable, UpdateOptions};
use prost_types::FieldMask;
use tonic::Status;

/// Normalize a FieldMask path to proto/Rust snake_case.
pub fn normalize_path(path: &str) -> String {
    path.split('.')
        .map(camel_segment_to_snake)
        .collect::<Vec<_>>()
        .join(".")
}

fn camel_segment_to_snake(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len() + 4);
    for (i, c) in segment.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                out.push('_');
            }
            for lower in c.to_lowercase() {
                out.push(lower);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Effective mask after empty→populated resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedPaths {
    /// Empty mask and nothing populated — change nothing.
    Noop,
    /// Explicit `"*"`.
    Full,
    /// Concrete snake_case paths (implied populated or explicit mask).
    Paths(Vec<String>),
}

impl ResolvedPaths {
    pub fn is_noop(&self) -> bool {
        matches!(self, Self::Noop)
    }

    pub fn contains(&self, field: &str) -> bool {
        match self {
            Self::Noop => false,
            Self::Full => true,
            Self::Paths(paths) => paths.iter().any(|p| p == field),
        }
    }
}

/// Resolve the wire FieldMask + populated operation fields.
pub fn resolve(mask: &Option<FieldMask>, populated: &[&str]) -> Result<ResolvedPaths, Status> {
    let raw: Vec<String> = mask
        .as_ref()
        .map(|m| m.paths.iter().map(|p| normalize_path(p)).collect())
        .unwrap_or_default();

    if raw.is_empty() {
        return Ok(if populated.is_empty() {
            ResolvedPaths::Noop
        } else {
            ResolvedPaths::Paths(populated.iter().map(|s| (*s).to_string()).collect())
        });
    }

    if raw.iter().any(|p| p == "*") {
        if raw.len() != 1 {
            return Err(Status::invalid_argument(
                "update_mask: '*' must be the only path when used",
            ));
        }
        return Ok(ResolvedPaths::Full);
    }

    Ok(ResolvedPaths::Paths(raw))
}

/// Parse resolved paths into a typed [`Mask`], rejecting unknown fields.
pub fn parse_mask<T: Maskable>(resolved: &ResolvedPaths) -> Result<Mask<T>, Status> {
    match resolved {
        ResolvedPaths::Noop => Ok(Mask::empty()),
        ResolvedPaths::Full => Ok(Mask::full()),
        ResolvedPaths::Paths(paths) => {
            let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
            Mask::try_from(MaskInput(refs.into_iter()))
                .map_err(|e| Status::invalid_argument(format!("update_mask: {e}")))
        }
    }
}

fn update_options() -> UpdateOptions {
    UpdateOptions::builder()
        .replace_repeated(true)
        .replace_message(true)
        .build()
}

/// Apply `source` onto `target` according to `resolved`.
///
/// No-ops leave `target` unchanged. Unknown paths error via [`parse_mask`].
pub fn apply<T: Maskable + SelfMaskable>(
    resolved: &ResolvedPaths,
    target: &mut T,
    source: T,
) -> Result<(), Status> {
    if resolved.is_noop() {
        return Ok(());
    }
    let mask = parse_mask::<T>(resolved)?;
    mask.update_with_options(target, source, &update_options());
    Ok(())
}

/// Trim a string; treat blank as unset (`None`).
pub fn trim_nonempty(value: Option<String>) -> Option<String> {
    value.and_then(|v| {
        let t = v.trim();
        if t.is_empty() {
            None
        } else {
            Some(t.to_string())
        }
    })
}

/// After [`apply`], reject a `field` that was named in the mask but resolved
/// to a blank (post-trim) value. Several update RPCs model a required string
/// field that cannot be cleared via field-mask — blank there is a client
/// error, not an implicit no-op — and previously repeated this same
/// mask-and-blank check with a hand-written message at each call site.
pub fn reject_if_blank(resolved: &ResolvedPaths, field: &str, value: &str) -> Result<(), Status> {
    if resolved.contains(field) && value.trim().is_empty() {
        return Err(Status::invalid_argument(format!(
            "{field} must be non-empty when present in update_mask"
        )));
    }
    Ok(())
}

/// Build `Option<Option<T>>` for masked updates: absent preserves, selected
/// without a value clears, and selected with a value replaces.
pub fn clearable_update<T>(
    resolved: &ResolvedPaths,
    field: &str,
    operation_had_value: bool,
    applied: Option<T>,
) -> Option<Option<T>> {
    if !resolved.contains(field) {
        None
    } else if !operation_had_value {
        Some(None)
    } else {
        Some(applied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fieldmask::Maskable;
    use std::collections::HashMap;

    #[derive(Debug, Clone, Default, PartialEq, Maskable, fieldmask::SelfMaskable)]
    struct Sample {
        enabled: bool,
        name: String,
        floor: Option<String>,
        tags: HashMap<String, String>,
        connection_id: String,
    }

    #[test]
    fn normalizes_camel_case_paths() {
        assert_eq!(normalize_path("connectionId"), "connection_id");
        assert_eq!(normalize_path("shadowSampleRate"), "shadow_sample_rate");
        assert_eq!(normalize_path("connection_id"), "connection_id");
        assert_eq!(normalize_path("foo.barBaz"), "foo.bar_baz");
    }

    #[test]
    fn omitted_mask_uses_populated_only() {
        let r = resolve(&None, &["enabled", "name"]).unwrap();
        assert_eq!(
            r,
            ResolvedPaths::Paths(vec!["enabled".into(), "name".into()])
        );
    }

    #[test]
    fn empty_mask_with_no_populated_is_noop() {
        assert_eq!(
            resolve(&Some(FieldMask { paths: vec![] }), &[]).unwrap(),
            ResolvedPaths::Noop
        );
    }

    #[test]
    fn star_is_full() {
        let r = resolve(
            &Some(FieldMask {
                paths: vec!["*".into()],
            }),
            &[],
        )
        .unwrap();
        assert_eq!(r, ResolvedPaths::Full);
    }

    #[test]
    fn rejects_unknown_paths_via_maskable() {
        let r = resolve(
            &Some(FieldMask {
                paths: vec!["nope".into()],
            }),
            &[],
        )
        .unwrap();
        let err = parse_mask::<Sample>(&r).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn accepts_camel_case_against_snake_fields() {
        // "connectionId" normalizes to "connection_id", a real Sample field,
        // and the full resolve -> parse_mask -> apply pipeline succeeds and
        // actually updates the field, just like a real camelCase wire path.
        let r = resolve(
            &Some(FieldMask {
                paths: vec!["connectionId".into()],
            }),
            &[],
        )
        .unwrap();
        assert!(parse_mask::<Sample>(&r).is_ok());

        let mut target = Sample::default();
        let source = Sample {
            connection_id: "conn-123".into(),
            ..Sample::default()
        };
        apply(&r, &mut target, source).unwrap();
        assert_eq!(target.connection_id, "conn-123");
    }

    #[test]
    fn rejects_camel_case_for_truly_unknown_field() {
        // "shadowSampleRate" normalizes to "shadow_sample_rate", which has no
        // matching Sample field — camelCase normalization must not make an
        // otherwise-unknown path succeed.
        let r = resolve(
            &Some(FieldMask {
                paths: vec!["shadowSampleRate".into()],
            }),
            &[],
        )
        .unwrap();
        assert!(parse_mask::<Sample>(&r).is_err());
    }

    #[test]
    fn apply_updates_only_masked_fields() {
        let mut target = Sample {
            enabled: false,
            name: "old".into(),
            floor: Some("keep".into()),
            tags: HashMap::new(),
            connection_id: String::new(),
        };
        let source = Sample {
            enabled: true,
            name: "new".into(),
            floor: None,
            tags: HashMap::from([("a".into(), "1".into())]),
            connection_id: String::new(),
        };
        let r = ResolvedPaths::Paths(vec!["enabled".into(), "tags".into()]);
        apply(&r, &mut target, source).unwrap();
        assert!(target.enabled);
        assert_eq!(target.name, "old");
        assert_eq!(target.floor.as_deref(), Some("keep"));
        assert_eq!(target.tags.get("a").map(String::as_str), Some("1"));
    }

    #[test]
    fn star_applies_defaults_and_replaces_maps() {
        let mut target = Sample {
            enabled: true,
            name: "old".into(),
            floor: Some("x".into()),
            tags: HashMap::from([("a".into(), "1".into())]),
            connection_id: String::new(),
        };
        apply(&ResolvedPaths::Full, &mut target, Sample::default()).unwrap();
        assert!(!target.enabled);
        assert!(target.name.is_empty());
        assert!(target.tags.is_empty());
        // fieldmask Option<String>: None → Some(""); call sites post-clear.
        assert_eq!(target.floor.as_deref(), Some(""));
    }

    #[test]
    fn noop_apply_leaves_target() {
        let mut target = Sample {
            enabled: true,
            ..Sample::default()
        };
        apply(&ResolvedPaths::Noop, &mut target, Sample::default()).unwrap();
        assert!(target.enabled);
    }

    #[test]
    fn trim_nonempty_drops_blank() {
        assert_eq!(trim_nonempty(Some("  id  ".into())).as_deref(), Some("id"));
        assert_eq!(trim_nonempty(Some("   ".into())), None);
        assert_eq!(trim_nonempty(None), None);
    }
}
