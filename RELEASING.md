# Releasing regency

regency has **one version** (decision D20): every service, `regencyctl`, the
web client and the all-in-one package are released together, at the version
of `[workspace.package]` in `Cargo.toml` (every crate has
`version.workspace = true`). There is one changelog, `CHANGELOG.md`.

Releases are built and published by GitHub Actions
(`.github/workflows/release.yml`) when a tag `v<version>` is pushed; nobody
builds release artifacts by hand.

## What a release publishes

The GitHub release `v<version>`, with `CHANGELOG.md`'s section as its notes
and these assets (amd64):

| Package (`.deb` and `.rpm`) | Contents |
|---|---|
| `regency-scheduler`, `regency-runner`, `regency-dbwriter`, `regency-backend`, `regency-endpoint`, `regency-sourcesync` | the service's binary, systemd unit, configuration template |
| `regencyctl` | the admin CLI and its configuration template |
| `regency-frontend` | the web client's files (`/usr/share/regency/web`) |
| `regency-all-in-one` | all of the above on one host, under one systemd unit; conflicts with the packages above |

RabbitMQ and CouchDB are never packaged: every package connects to existing
ones. OCI images and the Helm chart are not released yet.

The binaries are built on Ubuntu 26.04. They need only the glibc versions of
the functions they use (2.39 for v0.1.0, printed by the workflow): Debian 13,
Ubuntu 24.04, RHEL 10, Fedora 40 and newer. Each package declares that
requirement, so an older system refuses to install it.

## Versioning rules

Semantic versioning of the whole, from the operators' point of view:

- **patch**: fixes, no change of configuration, messages or stored documents;
- **minor**: new features, new optional configuration keys, additive message
  or document fields (D20 compatibility rules);
- **major** (minor while in 0.x): anything an operator must act on, or a
  message/document change older services cannot read.

Pre-releases (`v1.3.0-rc.1`) are published as GitHub pre-releases.

## Changelog

Every change that matters to operators or users goes into `CHANGELOG.md`,
under `# Unreleased`, in the subsection of its component:

```markdown
# Unreleased

Summary of the release (optional).

## scheduler
## runner
## dbwriter
## backend
## endpoint
## sourcesync
## regencyctl
## frontend
## all-in-one
```

A change in `regencylib` goes into the components whose behaviour it
changes. A release lists **every** component; one that did not change says
`No change.`

## Releasing

Example: v0.2.0.

1. Make sure `main` is green (CI: lint, tests, frontend, Windows, image).
2. In `CHANGELOG.md`, rename `# Unreleased` to `# Release v0.2.0` and
   complete it (what changed, what operators must do), one `##` subsection
   per component.
3. Set `version = "0.2.0"` in `[workspace.package]` of `Cargo.toml`, then
   `cargo update -w` (updates `Cargo.lock`).
4. Check: `.github/scripts/release-check.sh 0.2.0` (the workflow runs the
   same check first and stops if it fails).
5. Commit `regency 0.2.0`, tag and push:
   ```bash
   git tag v0.2.0
   git push origin main v0.2.0
   ```
6. The release workflow builds and publishes everything (about 20-30
   minutes) and creates the GitHub release `v0.2.0`.
7. Add a new `# Unreleased` section at the top of `CHANGELOG.md`.

If the workflow fails, fix the cause, delete the GitHub release if it was
created and the tag (`git push --delete origin v0.2.0`, `git tag -d
v0.2.0`), and tag again.

## Upgrades with mixed versions

Even with one version, a running fleet is upgraded service by service (a
rolling upgrade), so for a while services of two consecutive versions work
together. The D20 compatibility rules keep that safe: additive JSON message
changes within a `schema_version`, protobuf evolution rules, additive CouchDB
documents, never changing an existing queue's arguments. A runner refuses a
task built with another regent-sdk version and records why (task header
`sdk_version`).

## Building locally (without releasing)

```bash
cargo build --release
(cd frontend && trunk build --release)
cargo deb --no-build -p runner -o dist/            # one package
cargo generate-rpm -p runner -o dist/
cargo deb --no-build -p regency-aio -o dist/       # regency-all-in-one
cargo generate-rpm -p regency-aio -o dist/
```

## Requirements of the release workflow

- `protoc` 36.1 exactly (installed by `.github/actions/setup`; D6).
- Repository settings: Actions may write contents (releases); no other
  secret is needed (`GITHUB_TOKEN`).
