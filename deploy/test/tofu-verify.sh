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
PY
