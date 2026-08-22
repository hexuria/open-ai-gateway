output "url" {
  value = "https://${azurerm_container_app.this.ingress[0].fqdn}"
}
output "fqdn" {
  value = azurerm_container_app.this.ingress[0].fqdn
}

output "migrate_check" {
  description = <<-EOT
    REQUIRED after every apply. This platform cannot fail the apply on a failed
    migration: azurerm exposes no revision health, no runningStatus and no
    revision data source, so nothing in the Terraform graph can read whether
    the init container succeeded. Check it yourself:

      az containerapp revision show -n <app> -g <rg> \
        --revision <this output> --query 'properties.runningState'
  EOT
  value       = azurerm_container_app.this.latest_revision_name
}
