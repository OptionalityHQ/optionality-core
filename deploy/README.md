# Server Deployment & Vultr Sizing Guide

## Vultr 1-Month Sizing Strategy ($200 Expiring Credit)

Since the $200 credit expires in 30 days, picking a cheap $24/mo server wastes $176 in unused credits. Maximize performance:

| Plan | Specs | Monthly Cost | Credit Utilized | Verdict |
| :--- | :--- | :--- | :--- | :--- |
| **Max Performance (Recommended)** | **8 vCPU / 32 GB RAM / 450 GB NVMe** (High Performance AMD EPYC) | **$192/mo** | **96% ($192 / $200)** | Blazing builds (12s), dedicated threads for feed/pricing/sandbox, 10Gbps uplink |
| **Balanced High-Perf** | **8 vCPU / 16 GB RAM / 100 GB NVMe** (Dedicated CPU) | **$120/mo** | **60% ($120 / $200)** | Zero CPU steal, guaranteed compute for order book & SSE |
| **Standard High-Perf** | **4 vCPU / 16 GB RAM / 350 GB NVMe** (High Performance AMD EPYC) | **$96/mo** | **48% ($96 / $200)** | Very fast compile, leaves buffer |

OS: **Ubuntu 24.04 LTS x64**

---

## Live Engine Benchmarks (Release Build)

Ran with `node scripts/bench.mjs` against `target/release/engine`:

- **/health**: 10,556 req/s | avg: 0.95ms | p95: 1.09ms | p99: 4.25ms
- **/feed** (live aggregate ticker): 11,817 req/s | avg: 0.84ms | p95: 0.98ms | p99: 4.10ms
- **/state?asset=ETH** (full option ladder + implied vol + Greeks): 2,157 req/s | avg: 4.66ms | p95: 4.85ms
- **/plan?asset=ETH** (ladder listing plan): 11,994 req/s | avg: 0.83ms | p95: 0.88ms
- **Engine Process Footprint**: ~6 MB RSS RAM.

---

## Option 1: Native Systemd (Fastest, zero container overhead)

1. SSH into Vultr server:
```bash
ssh root@<SERVER_IP>
```

2. Clone repository into `/opt/optionality`:
```bash
git clone <REPO_URL> /opt/optionality
cd /opt/optionality/near-mvp
```

3. Run automated setup:
```bash
# For IP-only (HTTP on port 80):
sudo ./deploy/setup-vultr.sh

# Or with custom domain (automatic free HTTPS):
DOMAIN=options.example.com sudo ./deploy/setup-vultr.sh
```

4. Service management:
```bash
systemctl status optionality
journalctl -u optionality -f
systemctl restart optionality
```

---

## Option 2: Docker Compose

1. Build & run containers:
```bash
cd /opt/optionality/near-mvp
docker compose up -d --build
```

2. View status & logs:
```bash
docker compose ps
docker compose logs -f venue
```

---

## Resource Optimization (Active User Gating)

Simulation trades (`run_activity` taker + `run_maker`) are gated on active user presence:
- **Active state**: Active browser session on `/stream` (SSE) or HTTP API request within 90s.
- **Idle state**: No visitors connected -> simulation trades pause automatically.
- **Bandwidth & CPU**: Drops to near-zero load during idle.
- Check presence state: `curl http://127.0.0.1:8790/presence`
- Force always-active (optional): `ALWAYS_ACTIVE=1 ./scripts/venue.sh`
