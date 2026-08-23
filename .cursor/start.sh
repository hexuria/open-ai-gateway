#!/usr/bin/env bash
# Per-boot startup. Brings up the Docker daemon and the dev infrastructure the
# gateway needs, then returns. Must tolerate being run against a warm snapshot
# where some of this is already in place.
set -euo pipefail

cd "$(dirname "$0")/.."

# ── docker daemon ───────────────────────────────────────────────────────────────
# No init system in the pod, so start dockerd directly and only once, detached
# so it outlives this start script rather than being torn down when start
# returns.
if ! sudo docker info >/dev/null 2>&1; then
  # No live daemon, so a leftover pid file is stale and would block startup.
  pgrep -x dockerd >/dev/null || sudo rm -f /var/run/docker.pid
  # Detach fully: nohup + background inside a root shell that exits immediately,
  # so dockerd is reparented to init and outlives this start script.
  sudo sh -c 'nohup dockerd >/tmp/dockerd.log 2>&1 &'
  for _ in $(seq 1 60); do
    sudo docker info >/dev/null 2>&1 && break
    sleep 1
  done
fi
sudo docker info >/dev/null 2>&1 || { echo "start.sh: dockerd did not come up; see /tmp/dockerd.log" >&2; exit 1; }

# Let the agent (and just) talk to docker without sudo this boot.
sudo chmod 666 /var/run/docker.sock 2>/dev/null || true

# ── dev infrastructure ──────────────────────────────────────────────────────────
# Postgres + Redis on their fixed dev ports, then the schema. Both steps are
# idempotent: `dev-up` no-ops if the containers are already healthy and
# `migrate` no-ops if every migration is already applied.
just dev-up
just migrate

echo "start.sh: infrastructure up and migrated"
