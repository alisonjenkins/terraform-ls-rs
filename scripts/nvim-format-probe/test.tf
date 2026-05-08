
variable "unused_z" {
  type = string
}

variable "unused_a" {
  type = string
}

output "out_z" {
  value = "z"
}

output "out_a" {
  value = "a"
}

module "vm" {
  source = "./modules/vm"

  count = var.asr_target && local.create_vm_infrastructure ? 1 : 0

  customer         = var.customer
  enable_public_ip = var.sql_server_public_ip
  environment      = var.environment
  region           = var.region
  resource_group   = module.azure[0].main_resource_group
  subnet_id        = module.azure[0].main_subnet
  tags             = local.tags
}

resource "aws_instance" "web" {
  ami           = "ami-0"
  instance_type = "t3.micro"
  bogus_attr    = "x"
}

locals {
  _dr_region_short_map = {
    "UK South" = "uks"
    "UK West"  = "ukw"
  }
  _dr_target_region_short = var.dr_failover_config != null ? local._dr_region_short_map[var.dr_failover_config.target_region] : ""
  unused_local_z          = 1
  unused_local_a          = 2
}

terraform {
  required_version = ">= 1.4.0"
}
