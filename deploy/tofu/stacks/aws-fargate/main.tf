# AWS: ECS Fargate behind an ALB, with the data tier selectable.
#
#   managed  — RDS Postgres + ElastiCache Valkey, private subnets only.
#   neutral  — Neon + Upstash, supplied as URLs, so compute can move clouds
#              without the data moving with it.
#
# Terraform cannot select a module source dynamically, so both are declared and
# `count` picks one. It reads oddly and it is the standard way to do this.
#
# The ALB is the reason this platform can host 30-minute streams at all: its
# 4000s timeout is an *idle* timeout, not a wall clock, so a stream that emits
# anything at all inside the keepalive interval never trips it.

terraform {
  required_version = ">= 1.5"
  required_providers {
    aws = { source = "hashicorp/aws", version = ">= 5.0" }
    # Pinned to v4: v5 turned `rules` from a block into an attribute, so the
    # ruleset resources in the edge module do not parse against it.
    cloudflare = { source = "cloudflare/cloudflare", version = "~> 4.0" }
  }
}

provider "aws" {
  region = var.region
  default_tags {
    tags = var.tags
  }
}

locals {
  # ALB in front, tasks behind it, data behind them. Three groups rather than
  # one so each tier can only be reached by the tier above it.
  managed = var.data_mode == "managed"
}

resource "aws_security_group" "lb" {
  name        = "${var.name}-lb"
  description = "Ingress to the ${var.name} load balancer"
  vpc_id      = var.vpc_id
}

resource "aws_vpc_security_group_ingress_rule" "lb_https" {
  for_each          = toset(var.allowed_cidrs)
  security_group_id = aws_security_group.lb.id
  cidr_ipv4         = each.value
  from_port         = var.certificate_arn == "" ? 80 : 443
  to_port           = var.certificate_arn == "" ? 80 : 443
  ip_protocol       = "tcp"
}

resource "aws_vpc_security_group_egress_rule" "lb_to_tasks" {
  security_group_id            = aws_security_group.lb.id
  referenced_security_group_id = aws_security_group.task.id
  from_port                    = 8080
  to_port                      = 8080
  ip_protocol                  = "tcp"
}

resource "aws_security_group" "task" {
  name        = "${var.name}-task"
  description = "The ${var.name} tasks"
  vpc_id      = var.vpc_id
}

resource "aws_vpc_security_group_ingress_rule" "task_from_lb" {
  security_group_id            = aws_security_group.task.id
  referenced_security_group_id = aws_security_group.lb.id
  from_port                    = 8080
  to_port                      = 8080
  ip_protocol                  = "tcp"
}

# Unrestricted egress on purpose: the whole job of this service is to reach
# arbitrary model providers over TLS, and enumerating their address space is
# not something anyone can keep current.
resource "aws_vpc_security_group_egress_rule" "task_out" {
  security_group_id = aws_security_group.task.id
  cidr_ipv4         = "0.0.0.0/0"
  ip_protocol       = "-1"
}

module "data_managed" {
  count  = local.managed ? 1 : 0
  source = "../../modules/data-aws"

  name                     = var.name
  vpc_id                   = var.vpc_id
  private_subnet_ids       = var.private_subnet_ids
  client_security_group_id = aws_security_group.task.id
  highly_available         = var.highly_available
  deletion_protection      = var.deletion_protection
}

module "data_neutral" {
  count  = local.managed ? 0 : 1
  source = "../../modules/data-neutral"

  database_url = var.neutral_database_url
  redis_url    = var.neutral_redis_url
}

locals {
  database_url = local.managed ? module.data_managed[0].database_url : module.data_neutral[0].database_url
  redis_url    = local.managed ? module.data_managed[0].redis_url : module.data_neutral[0].redis_url
}

# Secrets Manager rather than the task definition itself: a task definition is
# readable by anyone with ecs:DescribeTaskDefinition, and it is versioned
# forever, so a secret placed there is a secret published to the account.
resource "aws_secretsmanager_secret" "this" {
  for_each = toset(["database-url", "redis-url", "signing-secret", "credential-kek"])
  name     = "${var.name}/${each.key}"
  # Rotating the same logical secret repeatedly during development otherwise
  # collides with the 30-day minimum recovery window.
  recovery_window_in_days = 7
}

resource "aws_secretsmanager_secret_version" "this" {
  for_each = {
    "database-url"   = local.database_url
    "redis-url"      = local.redis_url
    "signing-secret" = var.signing_secret
    "credential-kek" = var.credential_kek
  }
  secret_id     = aws_secretsmanager_secret.this[each.key].id
  secret_string = each.value
}

data "aws_iam_policy_document" "assume" {
  statement {
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["ecs-tasks.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "execution" {
  name               = "${var.name}-execution"
  assume_role_policy = data.aws_iam_policy_document.assume.json
}

resource "aws_iam_role_policy_attachment" "execution" {
  role       = aws_iam_role.execution.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AmazonECSTaskExecutionRolePolicy"
}

# The execution role reads the secrets at task start. Scoped to exactly these
# four ARNs rather than a wildcard.
data "aws_iam_policy_document" "read_secrets" {
  statement {
    actions   = ["secretsmanager:GetSecretValue"]
    resources = [for s in aws_secretsmanager_secret.this : s.arn]
  }
}

resource "aws_iam_role_policy" "read_secrets" {
  name   = "${var.name}-read-secrets"
  role   = aws_iam_role.execution.id
  policy = data.aws_iam_policy_document.read_secrets.json
}

# Deliberately empty. The gateway signs Bedrock requests with credentials held
# in its own account table, not with ambient task-role credentials, so granting
# bedrock:InvokeModel here would widen the blast radius without being used.
resource "aws_iam_role" "task" {
  name               = "${var.name}-task"
  assume_role_policy = data.aws_iam_policy_document.assume.json
}

module "gateway" {
  source = "../../modules/compute-fargate"

  name                   = var.name
  region                 = var.region
  image                  = var.image
  vpc_id                 = var.vpc_id
  public_subnet_ids      = var.public_subnet_ids
  private_subnet_ids     = var.private_subnet_ids
  lb_security_group_id   = aws_security_group.lb.id
  task_security_group_id = aws_security_group.task.id
  execution_role_arn     = aws_iam_role.execution.arn
  task_role_arn          = aws_iam_role.task.arn

  secret_env = {
    OAG_DATABASE__URL            = aws_secretsmanager_secret.this["database-url"].arn
    OAG_REDIS__URL               = aws_secretsmanager_secret.this["redis-url"].arn
    OAG_SECURITY__SIGNING_SECRET = aws_secretsmanager_secret.this["signing-secret"].arn
    OAG_SECURITY__CREDENTIAL_KEK = aws_secretsmanager_secret.this["credential-kek"].arn
  }

  env = var.gateway_env

  max_stream_duration_seconds = var.max_stream_duration_seconds
  desired_count               = var.desired_count
  min_count                   = var.min_count
  max_count                   = var.max_count
  internal                    = var.internal
  certificate_arn             = var.certificate_arn
  allow_plaintext_listener    = var.allow_plaintext_listener
  run_migrations              = var.run_migrations

  # The secret VERSIONS, not just the secrets: the migrate container resolves
  # `valueFrom` at task start, and a secret with no version yet fails the task
  # rather than the apply.
  depends_on = [aws_secretsmanager_secret_version.this]
}

module "edge" {
  count  = var.cloudflare_zone_id == "" ? 0 : 1
  source = "../../modules/edge-cloudflare"

  zone_id                    = var.cloudflare_zone_id
  hostname                   = var.hostname
  origin                     = module.gateway.lb_dns_name
  keepalive_interval_seconds = var.stream_keepalive_interval_seconds
}
