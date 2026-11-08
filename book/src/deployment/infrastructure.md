# RabbitMQ and CouchDB

## RabbitMQ

Required:

- RabbitMQ 3.13 or newer (4.x recommended), **3 nodes or more**;
- plugins `rabbitmq_management` (used by `regencyctl init`) and
  `rabbitmq_stream`;
- `cluster_partition_handling = pause_minority`;
- the stream port reachable by runners and backend, and
  `stream.advertised_host` set to a name they resolve.

`regencyctl init` creates the vhost, one user per service with least
privilege, the exchanges and queues. Re-running it is safe; it also resets
the service passwords to the configured values.

For security level 2 and above, enable TLS on AMQP (5671), streams (5551)
and the management API; at level 3, require client certificates
(`ssl_options.verify = verify_peer`, `fail_if_no_peer_cert = true`) and TLS
1.3.

## CouchDB

Required:

- CouchDB 3.x, **3 nodes or more** (`n = 3`);
- an admin account for `regencyctl init`.

`init` creates the databases, their security (readers per service) and the
validation that only dbwriter writes. dbwriter is a CouchDB **server admin**
(it creates and drops the monthly databases); every other service only
reads.

For security level 2 and above, serve HTTPS; at level 3, require client
certificates (`[ssl] verify_ssl_certificates = true`,
`fail_if_no_peer_cert = true`).
