# Docker Compose

`deploy/compose/quickstart.yaml` runs the all-in-one image with single-node
RabbitMQ and CouchDB: see [Quick start](../getting-started/quick-start.md).

For a compose deployment closer to production, run one container per
service from the per-service images (`ghcr.io/regent-project/regency-<service>`),
configured with environment variables:

```yaml
services:
  backend:
    image: ghcr.io/regent-project/regency-backend:1.2.0
    environment:
      REGENCY_INSTANCE: backend-1
      REGENCY_SECURITY_INTERNAL: "1"
      REGENCY_SECURITY_EXTERNAL: "2"
      REGENCY_RABBITMQ_URL: amqp://rabbitmq:5672
      REGENCY_RABBITMQ_STREAM_HOST: rabbitmq
      REGENCY_RABBITMQ_USERNAME: regency-backend
      REGENCY_COUCHDB_URL: http://couchdb:5984
      REGENCY_COUCHDB_USERNAME: regency-backend
      REGENCY_BACKEND_TLS_OFFLOADED: "true"     # behind your TLS proxy
    volumes:
      - ./config/backend.toml:/etc/regency/backend.toml:ro   # secret references, TLS files
      - ./secrets:/etc/regency/secrets:ro
    restart: always
```

- Every image reads `/etc/regency/<service>.toml` when present; environment
  variables override any key (`REGENCY_<SECTION>_<KEY>`).
- `restart: always` is required: services exit to apply a new central
  configuration.
- Run `regencyctl` once with the `regency-regencyctl` image (its default
  command is `init`).
