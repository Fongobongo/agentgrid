# TLS & reverse proxy

The control plane listens on plain HTTP (`AGENTGRID_LISTEN`, default
`0.0.0.0:7800`). MVP 0.1 ships **no native TLS** (ADR #10): terminate TLS at a
reverse proxy and forward to the control plane. The node daemon reaches the
control plane over HTTPS using `rustls`, so a CA-trusted certificate is
required (self-signed needs extra trust configuration on the nodes).

Auth is independent of TLS: every `/v1` call carries a `Bearer` JWT (or a
node credential for `/v1/node/*`), so the proxy only needs to pass traffic
through. Enrollment tokens travel in the bootstrap `POST` and must go over TLS.

## Caddy (recommended — automatic Let's Encrypt)

```caddyfile
agentgrid.example.com {
    reverse_proxy 127.0.0.1:7800
}
```

Caddy obtains and renews the certificate automatically. For a LAN/homelab
without a public name, use Caddy's internal CA and distribute its root to the
nodes, or front with a real cert.

## nginx

```nginx
server {
    listen 443 ssl;
    server_name agentgrid.example.com;

    ssl_certificate     /etc/ssl/agentgrid/fullchain.pem;
    ssl_certificate_key /etc/ssl/agentgrid/privkey.pem;

    location / {
        proxy_pass         http://127.0.0.1:7800;
        proxy_http_version 1.1;
        proxy_set_header   Host              $host;
        proxy_set_header   X-Real-IP         $remote_addr;
        proxy_set_header   X-Forwarded-For   $proxy_add_x_forwarded_for;
        proxy_set_header   X-Forwarded-Proto $scheme;
    }
}
```

## Binding behind the proxy

In production bind the control plane to localhost so it is not reachable
directly:

```bash
AGENTGRID_LISTEN=127.0.0.1:7800 docker compose up -d control-plane
```

Nodes then use the public name:

```bash
AGENTGRID_SERVER=https://agentgrid.example.com ./install-node.sh --server https://agentgrid.example.com --token <enroll-token>
```

## Docker Compose

The bundled `docker-compose.yml` already publishes port `7800`. Put the proxy
on the host (or in its own container on the same network) and point it at
`control-plane:7800`; the published host port can then be dropped.
