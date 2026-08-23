#!/usr/bin/env bash
# One-time environment bootstrap. Runs after the repository is checked out and
# its result is captured in the environment build snapshot, so keep it
# idempotent and let it terminate.
set -euo pipefail

export DEBIAN_FRONTEND=noninteractive

# ── system packages ────────────────────────────────────────────────────────────
# just (the command runner every workflow goes through), the Docker engine and
# compose plugin (dev Postgres + Redis run in containers), the Postgres and
# Redis clients the verify harness and ad-hoc debugging use, and the fuse
# userspace-overlay stack Docker needs inside a nested container.
sudo apt-get update
sudo apt-get install -y -o Dpkg::Options::="--force-confold" \
  just \
  docker.io \
  docker-compose-v2 \
  postgresql-client \
  redis-tools \
  fuse3 \
  fuse-overlayfs \
  uidmap \
  iptables

# ── docker for a nested container ───────────────────────────────────────────────
# The agent runs inside a pod where the kernel refuses overlay mounts, so the
# default overlayfs/containerd-snapshotter storage driver cannot mount an image.
# The fuse-overlayfs graphdriver does the same job in userspace and works here.
sudo mkdir -p /etc/docker
sudo tee /etc/docker/daemon.json >/dev/null <<'JSON'
{
  "storage-driver": "fuse-overlayfs",
  "features": { "containerd-snapshotter": false }
}
JSON

# Let the agent drive docker without sudo (start.sh also chmods the socket each
# boot, since group membership needs a fresh login to take effect).
sudo usermod -aG docker "$USER" || true

# ── warm the rust build cache ───────────────────────────────────────────────────
# The store crate deliberately avoids sqlx compile-time macros, so the whole
# workspace and its tests build without a database. Building here folds the
# compiled artifacts into the snapshot so the first `just check` / `just serve`
# on a fresh agent is fast.
cargo fetch --locked
cargo build --workspace --all-targets

# ── warm the compose images ─────────────────────────────────────────────────────
# Pull Postgres and Redis now so a fresh boot does not have to fetch them. Best
# effort: a transient registry hiccup here must not fail the whole build, and
# start.sh pulls again if an image is missing.
sudo dockerd >/tmp/dockerd-install.log 2>&1 &
dockerd_pid=$!
for _ in $(seq 1 30); do [ -S /var/run/docker.sock ] && break; sleep 1; done
sudo chmod 666 /var/run/docker.sock 2>/dev/null || true
docker compose -f deploy/compose/dev.yml pull || true
sudo kill "$dockerd_pid" 2>/dev/null || true
wait "$dockerd_pid" 2>/dev/null || true

echo "install.sh: done"
