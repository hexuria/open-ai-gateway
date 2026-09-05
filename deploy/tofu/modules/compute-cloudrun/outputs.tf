output "url" { value = google_cloud_run_v2_service.this.uri }
output "service_name" { value = google_cloud_run_v2_service.this.name }
output "service_account" { value = var.service_account_email }
output "migrate_job" { value = try(google_cloud_run_v2_job.migrate[0].name, "") }

output "migrate_execution" {
  description = "The execution this apply created. Grep for it in Cloud Logging."
  value       = try(google_cloud_run_v2_job.migrate[0].latest_created_execution[0].name, "")
}
