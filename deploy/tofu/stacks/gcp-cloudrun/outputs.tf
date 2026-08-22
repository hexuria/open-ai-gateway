output "gateway_url" { value = module.gateway.url }
output "public_url" {
  value = var.cloudflare_zone_id == "" ? module.gateway.url : module.edge[0].url
}
output "migrate_job" {
  description = "Run before the first request: gcloud run jobs execute <name> --region <region> --wait"
  value       = module.gateway.migrate_job
}
