# ECS Fargate behind an Application Load Balancer.
#
# Fargate rather than Lambda because Lambda stops at 15 minutes and API Gateway
# at 29 seconds — both well under a 30-minute streamed completion. The ALB's
# idle timeout goes to 4000 seconds, and it is an *inactivity* timeout: the
# gateway's 10-second keepalive is what keeps a quiet stream alive under it.
#
# Unlike Cloud Run, Fargate has no single-port restriction, so the two-listener
# shape survives: the ALB fronts 8080 and nothing routes to 8081.

terraform {
  required_providers {
    aws = { source = "hashicorp/aws", version = "~> 6.0" }
  }
}

resource "aws_ecs_cluster" "this" {
  name = var.name
  setting {
    name  = "containerInsights"
    value = "enabled"
  }
}

resource "aws_cloudwatch_log_group" "this" {
  name              = "/ecs/${var.name}"
  retention_in_days = var.log_retention_days
}

resource "aws_lb" "this" {
  name               = var.name
  load_balancer_type = "application"
  internal           = var.internal
  # An internal load balancer belongs in the private subnets.
  #
  # This always used `public_subnet_ids`, so `internal = true` produced a
  # load balancer with no public IPs sitting in subnets that route to an
  # internet gateway — which applies cleanly and defeats the point of asking for
  # an internal one. The variable's own description said it was ignored when
  # internal; nothing ignored it.
  subnets         = var.internal ? var.private_subnet_ids : var.public_subnet_ids
  security_groups = [var.lb_security_group_id]

  # Inactivity, not total duration. Must exceed the gateway's keepalive
  # interval by a wide margin; it comfortably does at the default of 10s.
  idle_timeout = var.idle_timeout_seconds

  drop_invalid_header_fields = true
}

resource "aws_lb_target_group" "this" {
  name        = var.name
  port        = 8080
  protocol    = "HTTP"
  target_type = "ip"
  vpc_id      = var.vpc_id

  # LEAST OUTSTANDING REQUESTS, not round robin.
  #
  # Completions vary by two orders of magnitude in duration. Round robin
  # distributes arrivals evenly, which distributes load very unevenly: one task
  # ends up holding every long stream while its neighbours idle.
  load_balancing_algorithm_type = "least_outstanding_requests"

  # Long enough for in-flight streams to finish before the target is removed.
  # The default of 300s would sever them on every deploy.
  deregistration_delay = var.deregistration_delay_seconds

  health_check {
    # Readiness, on the admin port. The ALB routes traffic to 8080 and health-
    # checks 8081, which is what `port` below is for.
    #
    # These two probes were the wrong way round, and the consequence is not
    # symmetric. A failing CONTAINER health check makes ECS kill and replace the
    # task; a failing ALB check only stops routing to it. With the deep check on
    # the container, an RDS failover made every task's `/health/ready` fail at
    # once and ECS recycled the entire fleet — while the ALB, checking only
    # `/health/live`, kept routing to tasks that could not serve.
    #
    # So: the load balancer takes the deep check, because "stop sending this
    # replica work" is the proportionate response to a dependency being down,
    # and the container takes liveness, because "replace this task" is only ever
    # right for a process that is actually dead. Every other platform in this
    # repository is arranged that way.
    port                = "8081"
    path                = "/health/ready"
    interval            = 15
    timeout             = 5
    healthy_threshold   = 2
    unhealthy_threshold = 3
    matcher             = "200"
  }

  lifecycle {
    precondition {
      condition     = var.deregistration_delay_seconds >= var.max_stream_duration_seconds
      error_message = "deregistration_delay_seconds must be at least max_stream_duration_seconds, or a deploy removes a target while it is still streaming."
    }
    precondition {
      condition     = var.idle_timeout_seconds <= 4000
      error_message = "The ALB caps idle_timeout at 4000 seconds."
    }
  }
}

resource "aws_lb_listener" "this" {
  load_balancer_arn = aws_lb.this.arn
  port              = var.certificate_arn == "" ? 80 : 443
  protocol          = var.certificate_arn == "" ? "HTTP" : "HTTPS"
  certificate_arn   = var.certificate_arn == "" ? null : var.certificate_arn

  default_action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.this.arn
  }

  lifecycle {
    precondition {
      condition     = var.certificate_arn != "" || var.allow_plaintext_listener
      error_message = "No certificate_arn: the public listener would be plaintext HTTP on port 80, exposing every API key and prompt. Provide a certificate, or set allow_plaintext_listener = true for a deployment that terminates TLS in front of this ALB."
    }
  }
}

locals {
  # The admin listener has to be reachable from the ALB, because the ALB is what
  # health-checks it.
  #
  # `server.admin_addr` defaults to `127.0.0.1:8081` — deliberately loopback, so
  # the admin API is not exposed off the host by an oversight. The container's
  # own health check curls `127.0.0.1:8080` and never noticed. But the target
  # group checks `/health/ready` on 8081, and that request arrives on the task's
  # ENI: a loopback bind refuses it, every target goes unhealthy, and the ALB
  # serves 503 on an apply that reported success.
  #
  # Helm sets this on the Deployment and `compose/stack.yml` sets it on `oag-1`;
  # this module health-checks the port and did not, which is the same shape as
  # the Envoy defect fixed two commits ago. The module's own requirement wins
  # over `var.env`, because the health check it configures depends on it.
  #
  # Not an exposure: 8081 is reachable only from the load balancer's security
  # group, which is what the ingress rule beside the health check is for.
  #
  # `LOG_JSON` rides along for the reason Cloud Run has always set it: CloudWatch
  # turns a JSON line into queryable fields and leaves prose as prose. The review
  # named Azure; the check that enforces this found two platforms shipping
  # unstructured logs, and fixing one would have left the other doing it under a
  # check that says otherwise.
  container_env = [
    for k, v in merge(var.env, {
      OAG_SERVER__ADMIN_ADDR  = "0.0.0.0:8081"
      OAG_TELEMETRY__LOG_JSON = "true"
    }) :
    { name = k, value = v }
  ]
  # Never plain environment: these come from Secrets Manager or SSM, so they are
  # absent from the task definition and from state.
  container_secrets = [for k, v in var.secret_env : { name = k, valueFrom = v }]

  # Migrations run as a container in the same task, not as a separate one-off.
  # The AWS provider has no run-task *resource*; the `aws_ecs_task_execution`
  # DATA SOURCE looks like one but is read at PLAN time — a read-only plan on a
  # pull request would fire RunTask at the production database — and it never
  # calls DescribeTasks, so a failed migration is never noticed at all.
  migrate_container = {
    name    = "migrate"
    image   = var.image
    command = ["migrate"]

    # MUST be false. An essential container that exits — even successfully —
    # stops the whole task, so an essential migrate container would kill the
    # task at the exact moment it succeeded.
    essential = false

    # The full set, not just the database URL. `settings::load` runs
    # Config::validate before the subcommand match, so a migrate container
    # missing the signing secret or the KEK exits 1 on config validation and
    # never reaches Postgres — with an error that looks nothing like a
    # migration failure.
    environment = local.container_env
    secrets     = local.container_secrets

    # No startTimeout on purpose. It is set on the depended-ON container and
    # Fargate caps it at 120 seconds, which would cap every migration at two
    # minutes. The service's own timeout bounds the wait instead.

    logConfiguration = {
      logDriver = "awslogs"
      options = {
        "awslogs-group"  = aws_cloudwatch_log_group.this.name
        "awslogs-region" = var.region
        # Its own stream: when an apply fails on a migration, this is where the
        # SQL error is.
        "awslogs-stream-prefix" = "migrate"
      }
    }
  }

  gateway_container = merge(
    {
      name      = "gateway"
      image     = var.image
      command   = ["serve"]
      essential = true

      portMappings = [
        { containerPort = 8080, protocol = "tcp" },
        { containerPort = 8081, protocol = "tcp" },
      ]

      environment = local.container_env
      secrets     = local.container_secrets

      healthCheck = {
        # Liveness, not readiness — see the target group's health check above.
        # ECS replaces a task whose check fails, and a task whose database is
        # unreachable is not a task worth replacing: every replacement finds the
        # same database.
        command     = ["CMD-SHELL", "curl -fsS http://127.0.0.1:8080/health/live || exit 1"]
        interval    = 15
        timeout     = 5
        retries     = 3
        startPeriod = 30
      }

      logConfiguration = {
        logDriver = "awslogs"
        options = {
          "awslogs-group"         = aws_cloudwatch_log_group.this.name
          "awslogs-region"        = var.region
          "awslogs-stream-prefix" = "gateway"
        }
      }
    },
    # merge() rather than a ternary yielding null: jsonencode would emit
    # "dependsOn": null, which ECS never returns, giving the task definition a
    # perpetual diff and forcing a redeploy on every apply.
    var.run_migrations ? {
      dependsOn = [{ containerName = "migrate", condition = "SUCCESS" }]
    } : {},
  )
}

resource "aws_ecs_task_definition" "this" {
  family                   = var.name
  requires_compatibilities = ["FARGATE"]
  network_mode             = "awsvpc"
  cpu                      = var.cpu
  memory                   = var.memory
  execution_role_arn       = var.execution_role_arn
  task_role_arn            = var.task_role_arn

  container_definitions = jsonencode(concat(
    var.run_migrations ? [local.migrate_container] : [],
    [local.gateway_container],
  ))
}

resource "aws_ecs_service" "this" {
  name            = var.name
  cluster         = aws_ecs_cluster.this.id
  task_definition = aws_ecs_task_definition.this.arn
  launch_type     = "FARGATE"
  desired_count   = var.desired_count

  # One at a time with a spare, for the same reason as the Kubernetes rollout:
  # every stopping task holds live streams for up to the drain budget.
  deployment_minimum_healthy_percent = 100
  deployment_maximum_percent         = 200

  # Long enough that a task finishes draining before ECS kills it.
  #
  # There is no 120s cap. This comment used to assert one, and told the reader
  # to size the gateway's drain budget around it — advice derived from a limit
  # that does not exist. ECS accepts a grace period up to 2,147,483,647 seconds;
  # what actually bounds a drain here is the target group's
  # `deregistration_delay`, which this module sets from
  # `deregistration_delay_seconds` and which AWS does cap, at 3,600.
  health_check_grace_period_seconds = var.health_check_grace_period_seconds

  # Without this the apply returns the moment ECS accepts the deployment, which
  # is exactly the green-apply-over-an-unmigrated-database failure the migrate
  # container exists to prevent.
  wait_for_steady_state = var.wait_for_steady_state

  # The wait is now tens of minutes of real wall clock, so Ctrl-C and CI job
  # timeouts are routine. Roll the deployment back rather than abandoning it
  # half-applied.
  sigint_rollback = true

  deployment_circuit_breaker {
    enable = true
    # TRUE. The provider's stability waiter reads the deployment status and
    # errors on ROLLBACK_SUCCESSFUL / ROLLBACK_FAILED / STOPPED, surfacing
    # ECS's own statusReason as the diagnostic — so a failed migration fails
    # the apply loudly *and* restores the last good revision. With `false` the
    # service parks on a FAILED deployment that launches no tasks.
    rollback = true
  }

  timeouts {
    # Must outlast a real migration plus the rolling replacement of every task.
    create = "45m"
    update = "45m"
  }

  network_configuration {
    subnets          = var.private_subnet_ids
    security_groups  = [var.task_security_group_id]
    assign_public_ip = false
  }

  load_balancer {
    target_group_arn = aws_lb_target_group.this.arn
    container_name   = "gateway"
    container_port   = 8080
  }

  depends_on = [aws_lb_listener.this]
}

resource "aws_appautoscaling_target" "this" {
  service_namespace  = "ecs"
  resource_id        = "service/${aws_ecs_cluster.this.name}/${aws_ecs_service.this.name}"
  scalable_dimension = "ecs:service:DesiredCount"
  min_capacity       = var.min_count
  max_capacity       = var.max_count
}

resource "aws_appautoscaling_policy" "cpu" {
  name               = "${var.name}-cpu"
  policy_type        = "TargetTrackingScaling"
  service_namespace  = aws_appautoscaling_target.this.service_namespace
  resource_id        = aws_appautoscaling_target.this.resource_id
  scalable_dimension = aws_appautoscaling_target.this.scalable_dimension

  target_tracking_scaling_policy_configuration {
    predefined_metric_specification {
      predefined_metric_type = "ECSServiceAverageCPUUtilization"
    }
    target_value = var.target_cpu
    # Slow to scale in: every removed task drains live streams first, so
    # thrashing costs far more than a spare task.
    scale_in_cooldown  = 600
    scale_out_cooldown = 60
  }
}
