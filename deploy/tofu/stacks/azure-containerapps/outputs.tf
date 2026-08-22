output "url" {
  value = module.gateway.url
}

output "fqdn" {
  value = module.gateway.fqdn
}

output "resource_group" {
  value = azurerm_resource_group.this.name
}

output "migrate_check" {
  description = "Revision to check after every apply — this platform cannot fail the apply on a failed migration. See the module output of the same name."
  value       = module.gateway.migrate_check
}
