output "url" {
  value = "${var.certificate_arn == "" ? "http" : "https"}://${aws_lb.this.dns_name}"
}
output "lb_dns_name" { value = aws_lb.this.dns_name }
output "lb_zone_id" { value = aws_lb.this.zone_id }
output "cluster_name" { value = aws_ecs_cluster.this.name }
