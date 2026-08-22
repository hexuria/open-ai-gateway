output "url" {
  value = "https://${azurerm_container_app.this.ingress[0].fqdn}"
}
output "fqdn" {
  value = azurerm_container_app.this.ingress[0].fqdn
}
