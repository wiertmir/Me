#!/usr/bin/env bash
# Runs auth-service, auth-web and Caddy on a local Kubernetes cluster (kind), as an alternative to the
# three N-run-*.sh scripts. Same address, same .env, same data directories, so stop those first.
# Run it again after a code change: it rebuilds the images and restarts the pod.
#   stop:   docker stop me-control-plane      (start again with: docker start me-control-plane)
#   remove: kind delete cluster --name me     (the data stays on the host)
set -euo pipefail
cd "$(dirname "$0")"
ROOT=$PWD
K="kubectl --context kind-me"

if ! kind get clusters 2>/dev/null | grep -qx me; then
  mkdir -p data auth-web/data/dp-keys "$HOME/.local/share/caddy"
  kind create cluster --name me --config - <<YAML
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
nodes:
  - role: control-plane
    extraPortMappings:      # published on every address of this machine
      - {containerPort: 80, hostPort: 80}
      - {containerPort: 443, hostPort: 443}
      - {containerPort: 18888, hostPort: 18888, listenAddress: 127.0.0.1}   # Aspire dashboard, this machine only
    extraMounts:            # the data directories of the plain processes
      - {hostPath: $ROOT/data, containerPath: /me/auth-data}
      - {hostPath: $ROOT/auth-web/data/dp-keys, containerPath: /me/dp-keys}
      - {hostPath: $HOME/.local/share/caddy, containerPath: /me/caddy}
YAML
fi

docker build -q -f auth-service/Dockerfile -t me/auth-service:dev .
docker build -q -f auth-web/Dockerfile -t me/auth-web:dev .
kind load docker-image --name me me/auth-service:dev me/auth-web:dev

$K create namespace me --dry-run=client -o yaml | $K apply -f -
TZ_NAME=$(timedatectl show -p Timezone --value 2>/dev/null || echo UTC)
OTLP_KEY=$(od -An -tx1 -N16 /dev/urandom | tr -d ' \n')   # lets only this pod's services send traces to the dashboard
$K -n me create secret generic me-env --from-env-file=<(cat .env; echo "TZ=$TZ_NAME"; echo "OTLP_KEY=$OTLP_KEY") \
  --dry-run=client -o yaml | $K apply -f -
$K -n me create configmap me-config --from-file=config.toml=auth-service/config.local.toml --from-file=Caddyfile \
  --dry-run=client -o yaml | $K apply -f -
$K apply -f k8s/me.yaml
$K -n me rollout restart deployment/auth
$K -n me rollout status deployment/auth --timeout=120s
echo "Logs:   $K -n me logs deploy/auth -c auth-service|auth-web|caddy -f"
echo "Traces: http://localhost:18888"
