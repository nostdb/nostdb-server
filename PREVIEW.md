# Server preview status

The Server is source-available SSPL-1.0 single-node evaluation software with no supported binary or hosted service.

- API-key authentication is the initial boundary; key lifecycle and multi-tenant identity are not implemented.
- Only `/healthz` is unauthenticated. TLS is not terminated by the Server.
- Transactions are bounded and queued per the documented version 1 contract; clustering, replication, and HA are absent.
- Snapshot Format 0 compatibility is exact/experimental; logical packages are distinct.
- No production SLA, backup service, installer/container, or external contribution intake exists.

Bind to loopback, use a unique environment-provided test key, and retain independent backups.
