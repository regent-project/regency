# gRPC API

backend serves one gRPC service, `regency.v1.Regency`
(`regencylib/proto/regency/v1/api.proto`), as gRPC (HTTP/2) and gRPC-Web
(for browsers). Authenticate with `authorization: Bearer <token>` (a session
from `Login`, an API token, or an OIDC access token).

Commands take a client-chosen `command_id`: retrying with the same id never
runs the command twice. They answer "accepted"; the effect shows up in the
live views.

```text
rpc Login(LoginRequest) returns (LoginResponse);
rpc WhoAmI(WhoAmIRequest) returns (WhoAmIResponse);
rpc WatchHosts(WatchHostsRequest) returns (stream HostEvent);
rpc WatchJobs(WatchJobsRequest) returns (stream JobEvent);
rpc ListOutcomes(ListOutcomesRequest) returns (ListOutcomesResponse);
rpc GetOutcome(GetOutcomeRequest) returns (GetOutcomeResponse);
rpc ListOrders(ListOrdersRequest) returns (ListOrdersResponse);
rpc ListCatalog(ListCatalogRequest) returns (ListCatalogResponse);
rpc GetSettings(GetSettingsRequest) returns (GetSettingsResponse);
rpc ListPolicies(ListPoliciesRequest) returns (ListPoliciesResponse);
rpc ListUsers(ListUsersRequest) returns (ListUsersResponse);
rpc RunNow(RunNowRequest) returns (Accepted);
rpc AbortJob(AbortJobRequest) returns (Accepted);
rpc UpsertOrder(UpsertOrderRequest) returns (Accepted);
rpc DeleteOrder(DeleteOrderRequest) returns (Accepted);
rpc SetSettings(SetSettingsRequest) returns (Accepted);
rpc DeleteHost(DeleteHostRequest) returns (Accepted);
rpc ApproveCertificate(ApproveCertificateRequest) returns (Accepted);
rpc UpsertCatalogItem(UpsertCatalogItemRequest) returns (Accepted);
rpc DeleteCatalogItem(DeleteCatalogItemRequest) returns (Accepted);
rpc RefreshSource(RefreshSourceRequest) returns (Accepted);
rpc UpsertUser(UpsertUserRequest) returns (Accepted);
rpc DeleteUser(DeleteUserRequest) returns (Accepted);
rpc CreateApiToken(CreateApiTokenRequest) returns (CreateApiTokenResponse);
rpc UpsertPolicy(UpsertPolicyRequest) returns (Accepted);
rpc DeletePolicy(DeletePolicyRequest) returns (Accepted);
rpc ListInstances(ListInstancesRequest) returns (ListInstancesResponse);
rpc GetServiceConfig(GetServiceConfigRequest) returns (GetServiceConfigResponse);
rpc SetServiceConfig(SetServiceConfigRequest) returns (Accepted);
rpc IssueEnrollmentToken(IssueEnrollmentTokenRequest) returns (IssueEnrollmentTokenResponse);
```

Generate a client from the `.proto` files with your gRPC tooling (tonic,
grpc-go, grpcio…); the web client itself is such a client (Rust, compiled to
WebAssembly).
