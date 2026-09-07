#!/usr/bin/env bash
# Configuration that cannot take effect.
#
# `terraform validate` is happy with a resource that never renders: a `count`
# gated on a variable whose default is falsy, which no stack passes, is valid
# HCL for something that can never exist. That is how the Cloudflare rate limit
# sat in the tree unreachable — `rate_limit_requests_per_minute` defaulted to 0,
# `count` required it to be non-zero, and no caller set it. An apply succeeds, a
# reader sees rate limiting in the module, and there is none.
#
# The rule is narrow on purpose. Sizing knobs like `cpu` and `memory` are also
# unpassed and that is fine — a default is the answer for them. This flags only
# the case where the default decides a resource does not exist and no caller can
# say otherwise.
set -euo pipefail

cd "$(dirname "$0")/../.."

python3 - <<'PY'
import collections
import glob
import os
import re
import sys

defaults = {}   # (module, variable) -> default literal, or None
declared = collections.defaultdict(set)
for vf in glob.glob("deploy/tofu/modules/*/variables.tf"):
    module = os.path.basename(os.path.dirname(vf))
    body = open(vf).read()
    for block in re.finditer(r'variable\s+"([^"]+)"\s*\{(.*?)\n\}', body, re.S):
        name, inner = block.group(1), block.group(2)
        declared[module].add(name)
        default = re.search(r"^\s*default\s*=\s*(\S+)\s*$", inner, re.M)
        defaults[(module, name)] = default.group(1) if default else None

# Every argument any caller passes to each module, from stacks and from modules
# that call other modules.
passed = collections.defaultdict(set)
for f in glob.glob("deploy/tofu/**/*.tf", recursive=True):
    for block in re.finditer(r'module\s+"[^"]+"\s*\{(.*?)\n\}', open(f).read(), re.S):
        inner = block.group(1)
        source = re.search(r'source\s*=\s*"[^"]*modules/([A-Za-z0-9_-]+)"', inner)
        if not source:
            continue
        for key in re.findall(r"^\s*([a-z0-9_]+)\s*=", inner, re.M):
            passed[source.group(1)].add(key)

unreachable = []
for module in sorted(declared):
    for f in sorted(glob.glob(f"deploy/tofu/modules/{module}/*.tf")):
        for count in re.findall(r"^\s*count\s*=\s*(.+)$", open(f).read(), re.M):
            for var in re.findall(r"var\.([a-z0-9_]+)", count):
                if var not in declared[module] or var in passed.get(module, set()):
                    continue
                # Only a falsy default can make the gate permanently closed.
                if defaults.get((module, var)) not in ("false", "0", '""'):
                    continue
                unreachable.append(
                    f"  {module}: count = {count.strip()}\n"
                    f"    var.{var} defaults to {defaults[(module, var)]} and no stack passes it,\n"
                    f"    so this resource can never be created."
                )

if unreachable:
    print("Configuration that cannot take effect:\n")
    print("\n".join(sorted(set(unreachable))))
    print("\nPass the variable from every stack that uses the module, or delete it.")
    sys.exit(1)

print("tofu: no resource is gated on a variable no caller can set")

# `deletion_protection` on Cloud Run defaults to true in the provider, so a
# resource that says nothing is a resource `terraform destroy` refuses to
# remove — discovered at teardown, in an environment someone meant to tear down.
# The migrate job set it and said why; the service beside it said nothing and
# inherited the opposite. Explicit either way, so the choice is in this
# repository rather than in a provider release note.
missing = []
for f in sorted(glob.glob("deploy/tofu/modules/*/*.tf")):
    body = open(f).read()
    for block in re.finditer(
        r'resource\s+"(google_cloud_run_v2_[a-z]+)"\s+"([^"]+)"\s*\{(.*?)\n\}',
        body,
        re.S,
    ):
        kind, name, inner = block.groups()
        if not re.search(r"^\s*deletion_protection\s*=", inner, re.M):
            missing.append(f"  {f}: {kind}.{name} does not set deletion_protection")

if missing:
    print("\nCloud Run resources inheriting the provider's deletion_protection:\n")
    print("\n".join(missing))
    print("\nThe provider defaults it to true, which makes `terraform destroy` fail.")
    sys.exit(1)

print("tofu: every Cloud Run resource states its own deletion_protection")

# A health check aimed at a listener that will refuse it.
#
# `oag`'s admin listener defaults to `127.0.0.1:8081` — deliberately loopback,
# so the admin API is not reachable off the machine by accident. A health check
# arriving from a load balancer therefore needs the module to bind it wider,
# and Helm and compose both do. A module that health-checks 8081 and does not
# is a module where every target is unhealthy on a green apply, which is the
# same shape as the H10 defect two commits before this one.
loopback = []
for f in sorted(glob.glob("deploy/tofu/modules/*/*.tf")):
    body = open(f).read()
    checks_admin_port = re.search(r"^\s*port(_value)?\s*=\s*\"?8081\"?", body, re.M)
    if checks_admin_port and "OAG_SERVER__ADMIN_ADDR" not in body:
        loopback.append(f"  {f}: health-checks 8081 without setting OAG_SERVER__ADMIN_ADDR")

if loopback:
    print("\nHealth checks aimed at a listener bound to loopback:\n")
    print("\n".join(loopback))
    print(
        "\n`server.admin_addr` defaults to 127.0.0.1:8081. A check from a load\n"
        "balancer is refused, every target goes unhealthy, and the apply is green."
    )
    sys.exit(1)

print("tofu: every module health-checking 8081 binds the admin listener for it")

# A private endpoint with no private DNS zone resolves to the public address it
# was created to stop using. Azure does not link the two for you: without a
# `private_dns_zone_group` the hostname still answers with the public IP, which
# `public_network_access_enabled = false` has just blocked. Green apply, dead
# dependency.
undns = []
for f in sorted(glob.glob("deploy/tofu/modules/*/*.tf")):
    body = open(f).read()
    for block in re.finditer(
        r'resource\s+"azurerm_private_endpoint"\s+"([^"]+)"\s*\{(.*?)\n\}',
        body,
        re.S,
    ):
        name, inner = block.groups()
        if "private_dns_zone_group" not in inner:
            undns.append(f"  {f}: azurerm_private_endpoint.{name} has no private_dns_zone_group")

if undns:
    print("\nPrivate endpoints whose hostname still resolves publicly:\n")
    print("\n".join(undns))
    print("\nWithout the zone group the name resolves to the IP the endpoint replaced.")
    sys.exit(1)

print("tofu: every private endpoint carries a DNS zone group")

# Structured logs, on every platform that ships them somewhere structured.
#
# `OAG_TELEMETRY__LOG_JSON` is what makes a log line queryable in Log Analytics
# or Cloud Logging. Cloud Run set it and Container Apps did not, so one
# platform's logs arrived as prose — and the review's own note for this, "missing
# Azure LOG_JSON", was answered against the wrong claim the first time.
unstructured = [
    d
    for d in sorted(glob.glob("deploy/tofu/modules/compute-*/"))
    if not any(
        "OAG_TELEMETRY__LOG_JSON" in open(f).read() for f in glob.glob(f"{d}*.tf")
    )
]

if unstructured:
    print("\nCompute modules that never set OAG_TELEMETRY__LOG_JSON:\n")
    for d in unstructured:
        print(f"  {d}")
    print("\nTheir logs arrive as prose, and nothing can query them by field.")
    sys.exit(1)

print("tofu: every compute module asks for structured logs")

# D11. The guarded number and the deployed number are the same number.
#
# `stream_keepalive_interval_seconds` reaches the Cloudflare module, which
# preconditions on it staying under Cloudflare's ~100s Proxy Read Timeout. The
# gateway read its own `OAG_GATEWAY__STREAM_KEEPALIVE_INTERVAL` out of
# `gateway_env`, so the two were independent: raise the real one and you get the
# 524s the precondition promised to prevent, with the precondition still green.
#
# Every stack has to merge the *same variable* the precondition sees into the
# compute env. A literal, or a second variable, would apply cleanly and re-open
# the gap.
ungated = []
for f in sorted(glob.glob("deploy/tofu/stacks/*/main.tf")):
    body = open(f).read()
    if "keepalive_interval_seconds" not in body:
        continue  # a stack with no edge in front of it has nothing to reconcile
    merged = re.search(
        r"OAG_GATEWAY__STREAM_KEEPALIVE_INTERVAL\s*=\s*tostring\("
        r"var\.stream_keepalive_interval_seconds\)",
        body,
    )
    guarded = re.search(
        r"keepalive_interval_seconds\s*=\s*var\.stream_keepalive_interval_seconds", body
    )
    if not merged:
        ungated.append(
            f"  {f}: the compute env does not carry var.stream_keepalive_interval_seconds"
        )
    elif not guarded:
        ungated.append(f"  {f}: the edge module is not given the variable the env carries")

if ungated:
    print("\nStacks where the guarded keepalive is not the deployed keepalive:\n")
    print("\n".join(ungated))
    print("\nThe precondition then guards a number nothing runs on.")
    sys.exit(1)

print("tofu: every stack guards the keepalive it actually deploys")

# H11. A secret pinned by ARN alone is not pinned.
#
# ECS resolves a bare ARN to AWSCURRENT at task start, so rotating a secret left
# the task definition byte-identical: no new revision, no deployment, every
# running task keeping the old value. The break arrives weeks later, when an
# unrelated image bump finally rolls the tasks onto a KEK that cannot decrypt
# anything sealed under the old one — with nothing in the change log between
# then and now that touched credentials.
#
# `:::${version_id}` is the ARN's own syntax for "no label, this version".
bare = []
for f in sorted(glob.glob("deploy/tofu/stacks/*/main.tf")):
    body = open(f).read()
    # Any indent, so a `terraform fmt` that re-indents the stack does not
    # quietly turn this into a scan that iterates nothing.
    block = re.search(r"secret_env\s*=\s*\{(.*?)\n\s*\}", body, re.S)
    if not block:
        continue
    for line in block.group(1).splitlines():
        if "=" not in line or not line.strip():
            continue
        value = line.split("=", 1)[1].strip()
        # Only Secrets Manager ARNs need this pin; a Cloud Run secret reference
        # or a Key Vault id is a different shape and pins its own way.
        if "secretsmanager_secret" in value and ":::" not in value:
            bare.append(f"  {f}: {line.strip()}")

if bare:
    print("\nSecrets referenced by a bare ARN, which ECS resolves at task start:\n")
    print("\n".join(bare))
    print("\nA rotation then changes nothing in the task definition, and nothing rolls.")
    sys.exit(1)

print("tofu: every Secrets Manager reference pins a version")
PY

# H10. Envoy health-checks the port readiness is served on.
#
# `/health/ready` lives on 8081 and traffic on 8080. An endpoint without its own
# `health_check_config` is checked on its traffic port, where that path does not
# exist — so either every endpoint fails and the cluster has no healthy hosts,
# or the check passes against the wrong handler and a replica that cannot reach
# Postgres keeps taking work.
python3 - <<'ENVOY'
import re
import sys

body = open("deploy/envoy/envoy.yaml").read()
starts = [m.start() for m in re.finditer(r"^\s*- endpoint:", body, re.M)]
if not starts:
    print("envoy: no endpoints found, so this assertion checks nothing")
    sys.exit(1)

missing = []
for i, start in enumerate(starts):
    end = starts[i + 1] if i + 1 < len(starts) else len(body)
    block = body[start:end]
    # Flow or block style — `health_check_config: { port_value: 8081 }` and the
    # two-line spelling are the same YAML, and a reformat must not un-guard this.
    if not re.search(r"health_check_config:.*?port_value:\s*8081\b", block, re.S):
        address = next(
            (l.strip() for l in block.splitlines() if "socket_address" in l), block.strip()
        )
        missing.append(address)

if missing:
    print("\nEnvoy endpoints health-checked on their traffic port:\n")
    for m in missing:
        print(f"  {m}")
    print("\n/health/ready is on 8081; on 8080 the check tests the wrong handler.")
    sys.exit(1)

print(f"envoy: all {len(starts)} endpoints health-check port 8081")
ENVOY
