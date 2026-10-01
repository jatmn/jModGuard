# Cost formulas

Use **local measurements** of your bot/API deployment and **current provider list prices**
at purchase time. This file does not invent a Bunny or AWS invoice. The repository
does not supply a production Discord-load measurement or a billed resource estimate.

## Measure your deployment

Record bot and API process RSS, CPU, database size, evidence growth and traffic under
representative load. Process RSS is not a container memory reservation or a billing unit.
Include frontend Storage and CDN traffic when using Bunny.

## Monthly estimate (operator fills prices)

```text
monthly ≈
  local_idle_power_watts × 730 h × electricity_price
  + bunny_database_price
  + bunny_magic_container_price     # bot + API
  + bunny_cdn_storage_and_pull      # frontend and evidence zones + traffic
  + aws_ec2_price                   # only for the EC2 topology
```

Include only the resources your chosen topology uses. Local SQLite on an existing
machine adds no Bunny Database charge. RDS/Postgres is not a supported database path.
Read current Bunny and AWS list prices on the day you buy; do not convert a debug
binary's RSS directly into a provider bill.

Live billed usage under production Discord load remains a provider check;
see [INSTALL.md](INSTALL.md).
