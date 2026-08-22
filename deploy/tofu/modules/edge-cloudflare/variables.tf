variable "zone_id" { type = string }
variable "hostname" {
  type        = string
  description = "Fully qualified, e.g. gateway.example.com"
}
variable "origin" {
  type        = string
  description = "The load balancer hostname or IP behind this record."
}
variable "origin_is_hostname" {
  type    = bool
  default = true
}
variable "proxied" {
  type        = bool
  default     = true
  description = "Set false (grey cloud) to bypass the edge entirely — the escape hatch if the Proxy Read Timeout ever becomes a problem."
}
variable "keepalive_interval_seconds" {
  type        = number
  default     = 10
  description = "Must match the gateway's gateway.stream_keepalive_interval. Checked against Cloudflare's read timeout."
}
variable "rate_limit_requests_per_minute" {
  type    = number
  default = 0
}
