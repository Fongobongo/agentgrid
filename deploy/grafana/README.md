# Grafana

`agentgrid.json` is a Grafana 9+/10+/11+ dashboard covering the control
plane's Prometheus endpoint (`/metrics`).

Import:
1. Grafana → Dashboards → New → Import → paste the contents of
   `deploy/grafana/agentgrid.json`.
2. Pick any Prometheus datasource scraping the CP `/metrics` path
   (scrape interval 5–15 s is plenty; the CP computes snapshot values on
   each scrape, no exporter layer needed).

The dashboard uses a `DS_PROMETHEUS` template so the datasource is picked
at import time.
