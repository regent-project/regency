<div align="center">
  <img src="frontend/regent-logo.png" alt="regency" width="140" />
  <h1>regency</h1>
  <p><em>Configuration management for thousands of hosts, live.</em></p>

  [![Release](https://img.shields.io/github/v/release/regent-project/regency?include_prereleases)](https://github.com/regent-project/regency/releases)
  [![CI](https://github.com/regent-project/regency/actions/workflows/ci.yml/badge.svg)](https://github.com/regent-project/regency/actions/workflows/ci.yml)
  [![Discord](https://img.shields.io/badge/Discord-Join%20our%20server-5865F2?logo=discord&logoColor=white)](https://discord.gg/2gxAW7uzsx)

  <img src="book/src/images/demo.gif" alt="regency: 3000 hosts in 12 inventories, a Reach fixes the drifting ones (red) live" width="900" />
</div>

regency is the fleet-scale control plane of
[regent-sdk](https://github.com/regent-project/regent-sdk): describe what your
hosts should look like, and regency checks them (**Assess**), fixes them
(**Reach**), records every outcome and shows the whole fleet live.

## Architecture

<p align="center"><img src="book/src/images/architecture.svg" alt="regency architecture" width="900" /></p>

Every service is stateless and runs as many replicas as needed: no single
point of failure.

## Documentation

Everything else (concepts, deployment, operations, reference) is in
**[the book](https://regent-project.github.io/regency/)**.

## Community and contributing

Questions, ideas and feedback: join us on
**[Discord](https://discord.gg/2gxAW7uzsx)**.

Contributions are welcome: open an
[issue](https://github.com/regent-project/regency/issues) to discuss a change,
then a pull request. [Development](book/src/project/development.md) explains
how to build, test and run the whole stack locally (`dev/run_locally`).
