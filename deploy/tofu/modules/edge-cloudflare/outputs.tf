output "hostname" { value = cloudflare_record.this.hostname }
output "url" { value = "https://${var.hostname}" }
