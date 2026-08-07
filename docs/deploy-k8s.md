# Deploying AgentGrid to Kubernetes

This guide covers production deployment of AgentGrid v0.4+ using Helm charts on Kubernetes.

## Prerequisites

- Kubernetes cluster v1.25+ (tested on 1.28+)
- `kubectl` configured with admin access
- `helm` v3.12+
- Storage class available (or use NFS/EBS)
- Optional: PostgreSQL instance, Redis cache, S3 bucket

## Quick Start

### Option A: SQLite (Development/Testing)

```bash
# Install control plane with local SQLite storage
helm install agentgrid ./charts \
  --namespace agentgrid --create-namespace \
  --set externalDatabase.enabled=false \
  --set localDatabase.enabled=true
```

### Option B: External PostgreSQL (Production Recommended)

```bash
# Create database credentials secret
kubectl create secret generic agentgrid-db-secret \
  --from-literal=password='your-strong-password' \
  --namespace agentgrid

# Install with PostgreSQL backend
helm install agentgrid ./charts \
  --namespace agentgrid --create-namespace \
  --set externalDatabase.enabled=true \
  --set externalDatabase.host=<your-db-host> \
  --set externalDatabase.passwordSecret.value='your-strong-password' \
  --set autoscaling.minReplicas=3 \
  --set autoscaling.maxReplicas=10
```

### Option C: Full Production Setup (PostgreSQL + Redis + S3)

```bash
helm install agentgrid ./charts \
  --namespace agentgrid --create-namespace \
  --set externalDatabase.enabled=true \
  --set redis.enabled=true \
  --set redis.host=redis-service.agentgrid.svc.cluster.local \
  --set artifacts.storageType=s3 \
  --set artifacts.s3.bucket=my-artifacts-bucket \
  --set autoscaling.minReplicas=3 \
  --set autoscaling.maxReplicas=20
```

## Configuration Reference

### Control Plane Settings

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `replicaCount` | int | `3` | Number of replicas (for non-HPA setups) |
| `autoscaling.minReplicas` | int | `3` | Minimum HA replicas |
| `autoscaling.maxReplicas` | int | `10` | Maximum scale limit |
| `externalDatabase.enabled` | bool | `false` | Use PostgreSQL vs SQLite |
| `artifacts.storageType` | string | `"local"` | `"local"`, `"s3"`, `"gcs"`, `"azure"` |

### Node Daemon Settings

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `keda.enabled` | bool | `true` | Enable KEDA-based autoscaling |
| `keda.minReplicas` | int | `1` | Minimum node replicas |
| `keda.maxReplicas` | int | `100` | Max concurrent agents |
| `podman.enabled` | bool | `true` | Use Podman container isolation |
| `transport.mode` | string | `"auto"` | `"poll"`, `"ws"`, `"auto"` |

## Monitoring & Observability

### Prometheus Integration

The charts include a ServiceMonitor for Prometheus Operator:

```yaml
# metrics/serviceMonitor.yaml
apiVersion: monitoring.coreos.com/v1
kind: ServiceMonitor
metadata:
  name: agentgrid-control-plane
spec:
  endpoints:
    - port: http
      path: /metrics
      interval: 30s
```

### Key Metrics

| Metric | Type | Description |
|--------|------|-------------|
| `agentgrid_pending_tasks` | Gauge | Tasks waiting in queue |
| `agentgrid_task_duration_seconds` | Histogram | Task execution time |
| `agentgrid_node_active_sandboxes` | Gauge | Active agent containers |
| `agentgrid_queue_latency_ms` | Gauge | Queue processing delay |

### Grafana Dashboards

Import dashboards from `./docs/grafana/` after installing Grafana:

- **AgentGrid Overview**: Cluster-wide health view
- **Task Performance**: Latency percentiles, throughput
- **Node Autoscaling**: Scale events, utilization

## High Availability Best Practices

### 1. Multi-Zone Deployment

```yaml
affinity:
  podAntiAffinity:
    preferredDuringSchedulingIgnoredDuringExecution:
      - weight: 100
        podAffinityTerm:
          labelSelector:
            matchLabels:
              app.kubernetes.io/name: agentgrid-control-plane
          topologyKey: kubernetes.io/hostname
```

### 2. Pod Disruption Budget

```yaml
podDisruptionBudget:
  minAvailable: 2  # Ensure at least 2 pods always running during maintenance
```

### 3. Resource Limits

Set appropriate limits based on workload:

- **CPU**: Request 250m, Limit 2 cores
- **Memory**: Request 512Mi, Limit 2Gi
- **Storage**: 10Gi per node for worktrees

## Scaling Under Load

### Horizontal Pod Autoscaler (HPA)

Auto-scale control plane based on CPU utilization:

```yaml
autoscaling:
  enabled: true
  minReplicas: 3
  maxReplicas: 10
  targetCPUUtilizationPercentage: 70
```

### KEDA-based Node Autoscaling

Scale nodes based on pending task count:

```yaml
apiVersion: keda.sh/v1alpha1
kind: ScaledObject
metadata:
  name: agentgrid-nodes
spec:
  minReplicaCount: 1
  maxReplicaCount: 100
  triggers:
    - type: custom
      metadata:
        metricName: agentgrid_queued_tasks
        threshold: "5"
```

### Manual Scale Commands

```bash
# Scale to specific size
kubectl scale deploy agentgrid-node --replicas=10

# Or update HPA directly
kubectl autoscale deployment agentgrid-control-plane \
  --min=3 --max=15 --cpu-percent=65
```

## Migration from Docker Compose

### Step 1: Export Existing Data

```bash
# Backup SQLite database
docker exec agentgrid-control-plane-1 cp /data/agentgrid.db ./backup.db

# Backup artifacts if using S3
aws s3 sync s3://my-bucket/ ./local-backup/
```

### Step 2: Import to PostgreSQL

```sql
-- Create new database structure
CREATE DATABASE agentgrid;
CREATE USER agentgrid WITH PASSWORD 'strong-password';
GRANT ALL PRIVILEGES ON DATABASE agentgrid TO agentgrid;

-- Migrate schema using sqlx-migrate
sqlx migrate run --database-url postgresql://agentgrid:password@localhost:5432/agentgrid
```

### Step 3: Install Helm Charts

See "Quick Start" section above.

## Troubleshooting

### Pods not starting

```bash
# Check logs
kubectl logs -n agentgrid deployment/agentgrid-control-plane

# Describe event
kubectl describe pod -n agentgrid <pod-name>

# Common issues:
# - Image pull failed: verify image tag exists
# - CrashLoopBackOff: check resource limits (OOMKilled)
# - Pending: insufficient resources or affinity conflicts
```

### Database connection errors

```bash
# Test DB connectivity
kubectl exec -it deployment/agentgrid-control-plane -- psql -h <host> -U agentgrid -d agentgrid

# Verify secrets
kubectl get secret agentgrid-db-secret -o jsonpath='{.data.password}' | base64 -d
```

### Autoscaling not triggering

```bash
# Check KEDA status
kubectl get scaledobject -n agentgrid

# Inspect metric source
kubectl describe scaledobject agentgrid-nodes -n agentgrid

# Verify Prometheus has scraped metrics
curl http://prometheus-server:9090/api/v1/query?query=agentgrid_pending_tasks
```

## Security Hardening

### 1. Network Policies

Restrict ingress/egress traffic:

```yaml
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: agentgrid-restrict-access
  namespace: agentgrid
spec:
  podSelector: {}
  policyTypes:
    - Ingress
    - Egress
  ingress:
    - from:
        - namespaceSelector:
            matchLabels:
              name: gateway
      ports:
        - port: 8080
  egress:
    - to:
        - namespaceSelector:
            matchLabels:
              name: database
      ports:
        - port: 5432
```

### 2. RBAC Configuration

```yaml
# Restrict control plane API access
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: agentgrid-reader
rules:
  - apiGroups: [""]
    resources: ["pods", "services"]
    verbs: ["get", "list"]
```

### 3. Secret Management

Use external secret managers instead of plain Kubernetes secrets:

- HashiCorp Vault Transit Engine
- AWS Secrets Manager
- Azure Key Vault

Example Vault integration:

```yaml
vault:
  enabled: true
  address: https://vault.example.com:8200
  role: agentgrid
  secretsEngine: kv
  mountPath: kubernetes/agentgrid
```

## Upcoming Features (v0.4+)

- [ ] Multi-tenant namespaces support
- [ ] Webhook integrations (GitHub, GitLab)
- [ ] Slack notifications for task events
- [ ] Batched task assignment algorithm
- [ ] Read replica offloading for PostgreSQL

For detailed architecture decisions, see [`docs/plans/0.4-production-ready.md`](./plans/0.4-production-ready.md).
