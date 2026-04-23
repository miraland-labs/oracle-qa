# Deploying oracle-qa on Ubuntu 24.04 (VPS)

`oracle-qa` is a **long-running** Tokio process (WebSocket log subscriber + HTTP API). It is **not** suited to Vercel-style serverless.

## 1. Build on the server (or CI)

```bash
sudo apt update && sudo apt install -y build-essential pkg-config libssl-dev
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
cd oracle-qa && cargo build --release
```

Binary: `target/release/oracle-qa`

## 2. Systemd unit

Create `/etc/systemd/system/oracle-qa.service`:

```ini
[Unit]
Description=oracle-qa SLA-Escrow API quality oracle
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=oracle
Group=oracle
WorkingDirectory=/opt/oracle-qa
EnvironmentFile=/etc/oracle-qa.env
ExecStart=/opt/oracle-qa/oracle-qa
Restart=on-failure
RestartSec=5
LimitNOFILE=65535

[Install]
WantedBy=multi-user.target
```

Place the binary at `/opt/oracle-qa/oracle-qa`, copy `.env.example` to `/etc/oracle-qa.env`, set permissions (`chmod 600 /etc/oracle-qa.env`), then:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now oracle-qa
sudo systemctl status oracle-qa
```

## 3. Reverse proxy (optional)

Bind `BIND_ADDR=127.0.0.1:4020` and expose **HTTPS** with nginx or Caddy in front for `/health` and operator-only `/evaluate` (firewall or mTLS recommended for manual triggers).

## 4. Operational checklist

- Oracle keypair funded with SOL on the target cluster for `ConfirmOracle` fees.
- `EVIDENCE_REGISTRY_URL` reachable from the VPS (same region reduces latency).
- `ESCROW_PROGRAM_ID` matches the deployment buyers/sellers use with pr402.
