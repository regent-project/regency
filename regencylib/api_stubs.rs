// Included by build.rs (feature `api`): backend service stubs.
pub fn stubs() {
    use tonic_build::manual::{Builder, Method, Service};

    let method = |name: &str, route: &str, input: &str, output: &str, streaming: bool| {
        let builder = Method::builder()
            .name(name)
            .route_name(route)
            .input_type(format!("crate::proto::{input}"))
            .output_type(format!("crate::proto::{output}"))
            .codec_path("regent_sdk::proto::grpc::ProtobufCodec");
        if streaming { builder.server_streaming().build() } else { builder.build() }
    };
    let service = Service::builder()
        .name("Regency")
        .package("regency.v1")
        .comment("regency backend API (D17), see proto/regency/v1/api.proto.")
        .method(method("login", "Login", "LoginRequest", "LoginResponse", false))
        .method(method("who_am_i", "WhoAmI", "WhoAmIRequest", "WhoAmIResponse", false))
        .method(method("watch_hosts", "WatchHosts", "WatchHostsRequest", "HostEvent", true))
        .method(method("watch_jobs", "WatchJobs", "WatchJobsRequest", "JobEvent", true))
        .method(method("list_outcomes", "ListOutcomes", "ListOutcomesRequest", "ListOutcomesResponse", false))
        .method(method("get_outcome", "GetOutcome", "GetOutcomeRequest", "GetOutcomeResponse", false))
        .method(method("list_orders", "ListOrders", "ListOrdersRequest", "ListOrdersResponse", false))
        .method(method("list_catalog", "ListCatalog", "ListCatalogRequest", "ListCatalogResponse", false))
        .method(method("get_effective_state", "GetEffectiveState", "GetEffectiveStateRequest", "GetEffectiveStateResponse", false))
        .method(method("get_attribute_schema", "GetAttributeSchema", "GetAttributeSchemaRequest", "GetAttributeSchemaResponse", false))
        .method(method("convert_attributes", "ConvertAttributes", "ConvertAttributesRequest", "ConvertAttributesResponse", false))
        .method(method("get_settings", "GetSettings", "GetSettingsRequest", "GetSettingsResponse", false))
        .method(method("list_policies", "ListPolicies", "ListPoliciesRequest", "ListPoliciesResponse", false))
        .method(method("list_users", "ListUsers", "ListUsersRequest", "ListUsersResponse", false))
        .method(method("run_now", "RunNow", "RunNowRequest", "Accepted", false))
        .method(method("abort_job", "AbortJob", "AbortJobRequest", "Accepted", false))
        .method(method("upsert_order", "UpsertOrder", "UpsertOrderRequest", "Accepted", false))
        .method(method("delete_order", "DeleteOrder", "DeleteOrderRequest", "Accepted", false))
        .method(method("set_settings", "SetSettings", "SetSettingsRequest", "Accepted", false))
        .method(method("delete_host", "DeleteHost", "DeleteHostRequest", "Accepted", false))
        .method(method("approve_certificate", "ApproveCertificate", "ApproveCertificateRequest", "Accepted", false))
        .method(method("upsert_catalog_item", "UpsertCatalogItem", "UpsertCatalogItemRequest", "Accepted", false))
        .method(method("delete_catalog_item", "DeleteCatalogItem", "DeleteCatalogItemRequest", "Accepted", false))
        .method(method("refresh_source", "RefreshSource", "RefreshSourceRequest", "Accepted", false))
        .method(method("upsert_user", "UpsertUser", "UpsertUserRequest", "Accepted", false))
        .method(method("delete_user", "DeleteUser", "DeleteUserRequest", "Accepted", false))
        .method(method("create_api_token", "CreateApiToken", "CreateApiTokenRequest", "CreateApiTokenResponse", false))
        .method(method("upsert_policy", "UpsertPolicy", "UpsertPolicyRequest", "Accepted", false))
        .method(method("delete_policy", "DeletePolicy", "DeletePolicyRequest", "Accepted", false))
        .method(method("issue_enrollment_token", "IssueEnrollmentToken", "IssueEnrollmentTokenRequest", "IssueEnrollmentTokenResponse", false))
        .method(method("list_instances", "ListInstances", "ListInstancesRequest", "ListInstancesResponse", false))
        .method(method("get_service_config", "GetServiceConfig", "GetServiceConfigRequest", "GetServiceConfigResponse", false))
        .method(method("set_service_config", "SetServiceConfig", "SetServiceConfigRequest", "Accepted", false))
        .build();
    Builder::new().compile(&[service]);
}
