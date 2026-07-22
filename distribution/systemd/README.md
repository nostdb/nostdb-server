# systemd candidate initialization

The unit runs as the dedicated `nostosdb` account. Initialize state as that same account so the daemon can read the generated `0600` credential files; running `nostosd init` as `root` and leaving the credentials root-owned makes `serve` fail closed.

After installing the `nostosdb` system account, binaries, and unit, initialize once:

```bash
sudo install -d -o nostosdb -g nostosdb -m 0700 /var/lib/nostosdb
sudo install -d -o nostosdb -g nostosdb -m 0700 /etc/nostosdb
sudo --user=nostosdb -- /usr/local/bin/nostosd init \
  --data-dir /var/lib/nostosdb \
  --config /etc/nostosdb/server.toml \
  --listen 127.0.0.1:7878
sudo chown root:nostosdb /etc/nostosdb/server.toml
sudo chmod 0640 /etc/nostosdb/server.toml
sudo chown root:nostosdb /etc/nostosdb
sudo chmod 0750 /etc/nostosdb
sudo systemctl enable --now nostosdb.service
```

The data tree and both credential files remain owned by `nostosdb:nostosdb`; do not recursively change them back to `root`. The configuration becomes root-owned and group-readable after initialization, while `/etc/nostosdb` remains traversable by the `nostosdb` group. The unit deliberately omits `ConfigurationDirectory=nostosdb` so systemd does not replace this root-managed configuration boundary. `nostosd init` is intentionally not an `ExecStartPre` action because it is a one-time, fail-if-existing operation.
