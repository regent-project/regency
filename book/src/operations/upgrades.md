# Upgrades

regency has one version: every service, `regencyctl`, the web client and
the all-in-one package are released together (GitHub release `v<version>`).

- **Helm:** `helm upgrade regency oci://ghcr.io/regent-project/charts/regency --version <x.y.z>`;
  `regencyctl init` runs again as a hook.
- **Packages:** upgrade the packages; each service restarts itself
  (systemd). Run `regencyctl init` after upgrading `regencyctl`.
- **Images:** change the tags, restart.

Rolling upgrades are supported: during an upgrade, a service works with the
previous version of the others, so services can be upgraded one at a time.

When a version moves to another regent-sdk, a runner still on the other SDK
version refuses the tasks built with the new one, and records why, until it
is upgraded too; the next firing runs normally.

Read the release notes (`CHANGELOG.md`, the GitHub release `v<version>`)
for actions required by a version.
