# Deploy on Termux (Android edge node)

Plan 2.12 (#24). Short path to running an agentgrid node-daemon on your
phone. Works on any aarch64 Android via the `musl-aarch64` release
tarball; armv7 works on the gnu armv7 target too.

## What this gives you

- A low-power node that registers itself as capacity — useful for test
  / dry-run workloads that don't need a build-server.
- Hard RSS defaults (`max_rss_mib = 256`, `max_parallel_attempts = 1`)
  because Android aggressively kills serial daemons. Override in
  `$PREFIX/var/lib/agentgrid/config.toml` once you know the device.
- No systemd, no root. Everything runs as the termux user inside
  `$PREFIX/var/lib/agentgrid/`.

## 1. Prereqs (Termux)

```sh
pkg update && pkg install -y bash curl tar git sqlite
# Optional (imports the config into the shared hospital):
# termux-setup-storage   # if you plan to git-clone large repos
```

## 2. Download release binaries

Termux expects `aarch64-unknown-linux-musl`. Grab the tarball from
GitHub Releases for this repo (name pattern
`agentgrid-aarch64.tar.gz`) and drop it next to this script's staging
dir (`./release-aarch64`).

```sh
mkdir -p release-aarch64
tar -xz -C release-aarch64 -f agentgrid-aarch64.tar.gz
```

## 3. Install the node

```sh
chmod +x deploy/install-node-termux.sh
./deploy/install-node-termux.sh \
  --server https://your-cp.example.com \
  --token <enroll-token> \
  --adapters fake-acp \
  --binaries ./release-aarch64
```

Defaults the script assumes (override with flags):
- `max_rss_mib = 256` — Android OOM-kills above ~200 MiB RSS
  for serial daemons on 3 GiB devices. Set on the node via
  `AGENTGRID_MAX_RSS_MIB=256` (the heartbeat writes it to `nodes.max_rss_mib`;
  before this knob existed the gate stayed pinned to the schema default
  1024 MiB and real OOM pressure slipped through).
- `max_parallel_attempts = 1` — a second attempt competes for the
  radio + battery.
- `workspace_dir = $PREFIX/var/lib/agentgrid/workspace` — inside
  Termux's private storage so other apps can't read it.

## 4. Start the daemon

```sh
nohup agentgrid-agent --config "$PREFIX/var/lib/agentgrid/config.toml" \
  > "$PREFIX/var/lib/agentgrid/noded.log" 2>&1 &
```

Optional (better supervision on auto-restart after crashes):

```sh
pkg install termux-services
mkdir -p $PREFIX/var/service/agentgrid-node
cat > $PREFIX/var/service/agentgrid-node/run <<'SH'
#!/data/data/com.termux/files/usr/bin/sh
exec agentgrid-agent --config "$PREFIX/var/lib/agentgrid/config.toml" 2>&1
SH
chmod +x $PREFIX/var/service/agentgrid-node/run
termux-notification --content "agentgrid node active" --id 4200
```

## 5. Verify (from ANY `ag` client)

```sh
ag nodes list
# finds the node (name = hostname or --name) with Online status.
ag doctor --server https://your-cp.example.com
# passes tasks_get, nodes check; simulates one attempt + waits for green.
```

## 6. Battery + thermal reality-check

- Termux holds a wake lock only while the foreground (or a
  `termux-wake-lock` script) is running. Battery-saver kills
  background CPUs aggressively.
- Long-running builds will thermal-throttle the device. Prefer
  `AGENTGRID_MAX_RSS_MIB=256` / `max_parallel_attempts = 1` — that keeps the
  file-lock in RAM.
- For CI coverage on cheap ARM devices, Termux-on-Android fits
  `ag autopilot` iteration loops nicely because one attempt ≈ one
  LSP parse + one git-commit; nothing else.

## 7. Uninstall

```sh
rm -rf "$PREFIX/var/lib/agentgrid"
rm "$PREFIX/bin/agentgrid-agent" "$PREFIX/bin/adapter-fake-acp"
```
