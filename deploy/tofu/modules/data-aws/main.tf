terraform {
  required_providers {
    aws    = { source = "hashicorp/aws", version = "~> 6.0" }
    random = { source = "hashicorp/random", version = ">= 3.5" }
  }
}

resource "random_password" "db" {
  length  = 32
  special = false
}

resource "aws_db_subnet_group" "this" {
  name       = "${var.name}-pg"
  subnet_ids = var.private_subnet_ids
}

resource "aws_security_group" "db" {
  name   = "${var.name}-pg"
  vpc_id = var.vpc_id
  ingress {
    from_port       = 5432
    to_port         = 5432
    protocol        = "tcp"
    security_groups = [var.client_security_group_id]
  }
}

resource "aws_db_instance" "this" {
  identifier     = "${var.name}-pg"
  engine         = "postgres"
  engine_version = "16"
  instance_class = var.db_instance_class

  allocated_storage     = var.db_storage_gb
  max_allocated_storage = var.db_storage_gb * 4
  storage_encrypted     = true

  db_name  = "oag"
  username = "oag"
  password = random_password.db.result

  db_subnet_group_name   = aws_db_subnet_group.this.name
  vpc_security_group_ids = [aws_security_group.db.id]
  publicly_accessible    = false

  multi_az                = var.highly_available
  backup_retention_period = var.backup_retention_days

  # This instance holds the credential store. A final snapshot is the
  # difference between a mistake and a disaster.
  deletion_protection       = var.deletion_protection
  skip_final_snapshot       = false
  final_snapshot_identifier = "${var.name}-pg-final"

  apply_immediately = false
}

resource "aws_elasticache_subnet_group" "this" {
  name       = "${var.name}-redis"
  subnet_ids = var.private_subnet_ids
}

resource "aws_security_group" "redis" {
  name   = "${var.name}-redis"
  vpc_id = var.vpc_id
  ingress {
    from_port       = 6379
    to_port         = 6379
    protocol        = "tcp"
    security_groups = [var.client_security_group_id]
  }
}

resource "aws_elasticache_replication_group" "this" {
  replication_group_id = "${var.name}-redis"
  description          = "open-ai-gateway coordination"
  engine               = "redis"
  engine_version       = "7.1"
  node_type            = var.redis_node_type
  port                 = 6379

  # Everything here is expendable — slots, session pins, the auth cache. A
  # single node is a defensible choice; failover only buys a shorter window of
  # extra database reads.
  num_cache_clusters         = var.highly_available ? 2 : 1
  automatic_failover_enabled = var.highly_available

  subnet_group_name  = aws_elasticache_subnet_group.this.name
  security_group_ids = [aws_security_group.redis.id]

  at_rest_encryption_enabled = true
  transit_encryption_enabled = var.tls

  # An AUTH token, so reaching the cache is not enough to read it.
  #
  # The GCP module sets an auth string; this one set nothing, so anything that
  # could open a TCP connection to the node could read every session pin and
  # cached identity in it. The security group is the only thing that was
  # stopping that, and a security group is one misconfigured rule from not
  # stopping it.
  #
  # Gated on `tls` because ElastiCache refuses an auth token without transit
  # encryption — sending it in clear would be worse than not setting one — and
  # generated rather than taken as a variable, so it never passes through a
  # tfvars file or a shell history.
  auth_token                 = var.tls ? random_password.redis_auth[0].result : null
  auth_token_update_strategy = "ROTATE"
}

resource "random_password" "redis_auth" {
  count = var.tls ? 1 : 0

  # ElastiCache requires 16-128 printable characters and rejects several
  # punctuation marks outright, so this keeps to the alphanumeric set rather
  # than discovering the exclusions during an apply.
  length  = 64
  special = false
}
