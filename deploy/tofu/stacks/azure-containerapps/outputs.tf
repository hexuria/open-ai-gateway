output "url" {
  value = module.gateway.url
}

output "fqdn" {
  value = module.gateway.fqdn
}

output "resource_group" {
  value = azurerm_resource_group.this.name
}
