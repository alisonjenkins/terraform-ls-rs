variable "used" {
  type = string
}

variable "unused" {
  type = string
}

locals {
  greeting = var.used
}

output "broken" {
  value = local.missing
}
