fn main() -> Result<(), Box<dyn std::error::Error>> {
    // tonic-build wraps prost-build, which shells out to `protoc`. CI
    // images do not always have it installed, so point at the vendored
    // binary unless the operator overrode PROTOC explicitly.
    if std::env::var_os("PROTOC").is_none() {
        let protoc = protoc_bin_vendored::protoc_bin_path()
            .expect("protoc-bin-vendored did not ship a protoc for this platform");
        std::env::set_var("PROTOC", protoc);
    }

    let router_proto_files = &[
        "proto/chalk/router/v1/api_key.proto",
        "proto/chalk/router/v1/background_response.proto",
        "proto/chalk/router/v1/debug.proto",
        "proto/chalk/router/v1/fallback_policy.proto",
        "proto/chalk/router/v1/judge_policy.proto",
        "proto/chalk/router/v1/model_route.proto",
        "proto/chalk/router/v1/provider_connection.proto",
        "proto/chalk/router/v1/rate_limit.proto",
        "proto/chalk/router/v1/sampling_policy.proto",
        "proto/chalk/router/v1/telemetry_settings.proto",
        "proto/chalk/router/v1/usage.proto",
        "proto/chalk/router/v1/usage_budget.proto",
        "proto/chalk/router/v1/usage_pool.proto",
    ];
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR")?);
    let router_descriptor = out_dir.join("router_descriptor.bin");
    tonic_build::configure()
        .build_server(true)
        .build_client(false)
        .file_descriptor_set_path(&router_descriptor)
        .compile_protos(router_proto_files, &["proto"])?;
    write_operation_permissions(
        &router_descriptor,
        &out_dir.join("operation_permissions.rs"),
    )?;

    // Cluster routers call the metadata API; the server half backs its tests.
    let metadata_proto_files = &["proto/chalk/server/v1/router_metadata_client.proto"];
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(metadata_proto_files, &["proto"])?;

    let health_proto_files = &["proto/grpc/health/v1/health.proto"];
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(health_proto_files, &["proto"])?;

    let copied_chalk_protos = &[
        "proto/chalk/auth/v1/permissions.proto",
        "proto/chalk/utils/v1/encoding.proto",
    ];
    for f in router_proto_files
        .iter()
        .chain(metadata_proto_files)
        .chain(health_proto_files)
        .chain(copied_chalk_protos)
    {
        println!("cargo:rerun-if-changed={f}");
    }
    Ok(())
}

/// Generate the admin RPC permission table from the `(chalk.auth.v1.permission)`
/// and `additional_permissions_list` method options. prost drops custom options,
/// so they are read back from the descriptor set with prost-reflect. Every
/// router RPC must declare a concrete permission; the build fails otherwise.
fn write_operation_permissions(
    descriptor: &std::path::Path,
    output: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    use prost_reflect::DescriptorPool;

    let pool = DescriptorPool::decode(std::fs::read(descriptor)?.as_slice())?;
    let extension = |name: &str| {
        pool.get_extension_by_name(name)
            .ok_or_else(|| format!("{name} is missing from proto/chalk/auth/v1/permissions.proto"))
    };
    let permission = extension("chalk.auth.v1.permission")?;
    let additional = extension("chalk.auth.v1.additional_permissions_list")?;
    let slug = extension("chalk.auth.v1.slug")?;
    let permissions = pool
        .get_enum_by_name("chalk.auth.v1.Permission")
        .ok_or("chalk.auth.v1.Permission is missing")?;
    let slug_for = |number: i32, path: &str| -> Result<String, String> {
        let value = permissions
            .get_value(number)
            .ok_or_else(|| format!("{path}: unknown permission {number}"))?;
        let options = value.options();
        let slug = options
            .get_extension(&slug)
            .as_str()
            .map(str::to_owned)
            .filter(|slug| !slug.is_empty())
            .ok_or_else(|| format!("{path}: {} has no slug", value.name()))?;
        // Login alone is not an admin grant. Prevent new RPCs from silently
        // reintroducing a control-plane authorization bypass.
        if matches!(slug.as_str(), "unspecified" | "insecure" | "authenticated") {
            return Err(format!(
                "{path}: {} is not a router permission",
                value.name()
            ));
        }
        Ok(slug)
    };

    let mut rows = Vec::new();
    for service in pool
        .services()
        .filter(|s| s.package_name() == "chalk.router.v1")
    {
        for method in service.methods() {
            let path = format!("/{}/{}", service.full_name(), method.name());
            let options = method.options();
            if !options.has_extension(&permission) {
                return Err(format!("{path} declares no (chalk.auth.v1.permission) option").into());
            }
            let primary = options
                .get_extension(&permission)
                .as_enum_number()
                .ok_or_else(|| format!("{path}: permission is not an enum value"))?;
            let mut slugs = vec![slug_for(primary, &path)?];
            if let Some(extra) = options.get_extension(&additional).as_list() {
                for value in extra {
                    let number = value.as_enum_number().ok_or_else(|| {
                        format!("{path}: additional permission is not an enum value")
                    })?;
                    let slug = slug_for(number, &path)?;
                    if !slugs.contains(&slug) {
                        slugs.push(slug);
                    }
                }
            }
            rows.push((path, slugs));
        }
    }
    rows.sort();

    let mut code = String::from(
        "// @generated by build.rs from the (chalk.auth.v1.permission) options in proto/chalk/router/v1.\n\
         /// Admin RPC path and the permission slugs that each accept (any one suffices).\n\
         pub const ADMIN_OPERATION_PERMISSIONS: &[(&str, &[&str])] = &[\n",
    );
    for (path, slugs) in rows {
        let slugs = slugs
            .iter()
            .map(|slug| format!("{slug:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        code.push_str(&format!("    ({path:?}, &[{slugs}]),\n"));
    }
    code.push_str("];\n");
    std::fs::write(output, code)?;
    Ok(())
}
