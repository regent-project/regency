# Users, API tokens and policies

## Authentication

| Way | For |
|---|---|
| local accounts (password) | people, small installations |
| API tokens | programs (CI, scripts); belong to a service account |
| OIDC (one or more providers) | people through your SSO (Keycloak, Entra ID, Google, Authentik…), programs with client credentials |
| bootstrap admin | the first login and break-glass access |

The **bootstrap admin** (`admin`) is created at first start with the
initial password you provide; it can be disabled but not deleted, and is
always authorized, so no policy can lock everyone out.

After login, backend issues a short-lived session token. Passwords and
tokens are hashed (argon2id) before they leave backend.

OIDC providers are configured in backend:

```toml
[[oidc]]
issuer = "https://sso.example.com/realms/ops"
audience = "regency"
username_claim = "preferred_username"
groups_claim = "groups"
```

## Authorization (Cedar)

Every read and every command is checked with [Cedar](https://www.cedarpolicy.com)
policies. Resources form a hierarchy — `Source` ⊃ `Inventory` ⊃ `Host` —
plus expected states, orders, settings and policies.

Built-in roles, by group membership:

| Group | May |
|---|---|
| `viewers` | read everything |
| `operators` | read, Assess, abort |
| `enforcers` | read, Assess, Reach, abort |
| `admins` | everything, including settings, configuration, users and policies |

Your own policies narrow or widen this, for example:

```cedar
permit(
  principal in Group::"web-team",
  action in [Action::"assess", Action::"reach"],
  resource in Inventory::"infra/web"
);
```

Policies are managed through the API (`UpsertPolicy`, `DeletePolicy`),
validated before they are accepted, and applied by every backend at once.
