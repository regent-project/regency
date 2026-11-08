# Ports

| Port | Service | Who connects |
|---|---|---|
| 8080 | backend: API (gRPC, gRPC-Web) and web client | users, API clients (usually through a TLS proxy or ingress) |
| 8443 (packages: 9443) | endpoint: gRPC over mTLS | regops agents |
| 9090 (or per service) | health and metrics | probes, Prometheus — private network only |
| 5672 / 5671 | RabbitMQ AMQP / AMQPS | every service |
| 5552 / 5551 | RabbitMQ streams / with TLS | runners, backend |
| 15672 / 15671 | RabbitMQ management API | regencyctl |
| 5984 / 6984 | CouchDB HTTP / HTTPS | every service but the runner (central configuration only) |
| 22 (or other) | SSH | runners → passive hosts |
| 443/UDP (or other) | SSHOQ | runners → passive hosts |

Packaged configurations use distinct health ports per service so several
services can share a machine: dbwriter 9091, scheduler 9092, runner 9093,
backend 9094, endpoint 9095, sourcesync 9096.
