#!/bin/bash
set -euo pipefail

# This script processes logs produced by nodes spawned using the zombienet-sdk framework.
# The logs are prepared for upload as GitHub artifacts.
# If Loki logging is available, the corresponding log URLs are also printed.
# NOTE: P982945308D3682D1 is the loki until we switch to loki.zombienet
LOKI_URL_FOR_NODE='https://grafana.teleport.parity.io/explore?schemaVersion=1&panes=%7B%22vxs%22:%7B%22datasource%22:%22P982945308D3682D1%22,%22queries%22:%5B%7B%22refId%22:%22A%22,%22expr%22:%22%7Bzombie_ns%3D%5C%22{{namespace}}%5C%22,zombie_node%3D%5C%22{{podName}}%5C%22%7D%22,%22queryType%22:%22range%22,%22datasource%22:%7B%22type%22:%22loki%22,%22uid%22:%22P982945308D3682D1%22%7D,%22editorMode%22:%22code%22,%22direction%22:%22forward%22%7D%5D,%22range%22:%7B%22from%22:%22{{from}}%22,%22to%22:%22{{to}}%22%7D,%22compact%22:false%7D%7D&orgId=1'

# Directory to send logs to loki
LOKI_DIR_FOR_NATIVE_LOGS="/var/log/shared/zombienet"

# JQ queries
JQ_QUERY_RELAY_V1='.relay[].name'
JQ_QUERY_RELAY_SDK='.relay.nodes[].name'

JQ_QUERY_PARA_NODES_V1='.paras[$pid].nodes[].name'
JQ_QUERY_PARA_NODES_SDK='.parachains[$pid][] .collators[].name'

# current time in milliseconds + 60 secs to allow loki to ingest logs
TO=$(($(date +%s%3N) + 60000))

make_url() {
  local name="$1"
  local to="$2"
  local url="${LOKI_URL_FOR_NODE//\{\{namespace\}\}/$NS}"
  url="${url//\{\{podName\}\}/$name}"
  url="${url//\{\{from\}\}/$FROM}"
  url="${url//\{\{to\}\}/$to}"
  echo "$url"
}

# Since we don't have the zombie.json file, we will make the best-effort to send the logs
process_logs_from_fallback() {
  local BASE_DIR="$1"
  local TARGET_DIR="$2"

  # Extract namespace from BASE_DIR (e.g., /tmp/zombie-abc123 -> zombie-abc123)
  NS=$(basename "$BASE_DIR")
  echo "Using fallback mode for namespace: $NS"

  # Use current time as FROM since we don't have zombie.json
  FROM=$(($(date +%s%3N) - 600000))  # 10 minutes ago

  # Find all logs with glob patterns
  local log_files=()

  # Search for SDK pattern: BASE_DIR/<name>/<name>.log
  if [[ -d "$BASE_DIR" ]]; then
    for node_dir in "$BASE_DIR"/*; do
      if [[ -d "$node_dir" && "$node_dir" != "$TARGET_DIR" ]]; then
        local node_name=$(basename "$node_dir")
        if [[ -f "$node_dir/$node_name.log" ]]; then
          log_files+=("$node_dir/$node_name.log")
        fi
      fi
    done
  fi

  # Search for v1 pattern: BASE_DIR/logs/<name>.log
  if [[ -d "$TARGET_DIR" ]]; then
    for log_file in "$TARGET_DIR"/*.log; do
      if [[ -f "$log_file" ]]; then
        log_files+=("$log_file")
      fi
    done
  fi

  if [[ ${#log_files[@]} -eq 0 ]]; then
    echo "::warning ::No log files found in $BASE_DIR using glob patterns"
    return 1
  fi

  echo "Found ${#log_files[@]} log file(s) using glob patterns"
  echo "Nodes:"

  for log_file in "${log_files[@]}"; do
    # Extract node name from log file path
    local name=$(basename "$log_file" .log)
    local_to=$TO

    # Copy log to target directory if not already there
    if [[ "$log_file" != "$TARGET_DIR/$name.log" ]]; then
      if ! cp "$log_file" "$TARGET_DIR/$name.log" 2>/dev/null; then
        echo "::warning ::Failed to copy log for $name"
        continue
      fi
    fi

    # Send logs to loki
    if [[ -d "$LOKI_DIR_FOR_NATIVE_LOGS" ]]; then
      if [[ -f "$TARGET_DIR/$name.log" ]]; then
        echo -e "sending logs to loki file: $LOKI_DIR_FOR_NATIVE_LOGS/to-loki.log"
        awk -v NS="$NS" -v NAME="$name" '{print NS" "NAME" " $0}' "$TARGET_DIR/$name.log" >> "$LOKI_DIR_FOR_NATIVE_LOGS/to-loki.log"
        local_to=$(($(date +%s%3N) + 60000))
      fi
    fi
    echo -e "\t$name: $(make_url "$name" "$local_to")"
  done
  echo ""
}

process_logs_from_zombie_file() {
  local BASE_DIR="$1"
  local TARGET_DIR="$2"
  local ZOMBIE_JSON="$3"

  # Extract namespace (ns in sdk / namespace in v1)
  NS=$(jq -r '.ns // .namespace' "$ZOMBIE_JSON")
  # test start time in milliseconds
  FROM=$(jq -r '.start_time_ts' "$ZOMBIE_JSON")

  echo "Relay nodes:"

  JQ_QUERY_RELAY=$JQ_QUERY_RELAY_V1
  JQ_QUERY_PARA_NODES=$JQ_QUERY_PARA_NODES_V1
  if [[ $(echo "$NS" | grep -E "zombie-[A-Fa-f0-9]+-") ]]; then
      JQ_QUERY_RELAY=$JQ_QUERY_RELAY_SDK
      JQ_QUERY_PARA_NODES=$JQ_QUERY_PARA_NODES_SDK
  fi;

  jq -r $JQ_QUERY_RELAY "$ZOMBIE_JSON" | while read -r name; do
    [[ -z "$name" ]] && continue
    local_to=$TO
    if [[ "${ZOMBIE_PROVIDER:-}" == "k8s" ]]; then
      # Fetching logs from k8s
      if ! kubectl logs "$name" -c "$name" -n "$NS" > "$TARGET_DIR/$name.log" 2>&1; then
        echo "::warning ::Failed to fetch logs for $name"
      fi
    else
      # zombienet v1 dump the logs to the `/logs` directory
      if [[ ! -f "$TARGET_DIR/$name.log" ]]; then
        # `sdk` use this pattern to store the logs in native provider
        if [[ -f "$BASE_DIR/$name/$name.log" ]]; then
          cp "$BASE_DIR/$name/$name.log" "$TARGET_DIR/$name.log"
        else
          echo "::warning ::Log file not found: $BASE_DIR/$name/$name.log"
          continue
        fi
      fi

      # send logs to loki
      if [[ -d "$LOKI_DIR_FOR_NATIVE_LOGS" && -f "$TARGET_DIR/$name.log" ]]; then
        awk -v NS="$NS" -v NAME="$name" '{print NS" "NAME" " $0}' "$TARGET_DIR/$name.log" >> "$LOKI_DIR_FOR_NATIVE_LOGS/to-loki.log"
        local_to=$(($(date +%s%3N) + 60000))
      fi
    fi
    echo -e "\t$name: $(make_url "$name" "$local_to")"
  done
  echo ""

  # Handle parachains grouped by paraId
  jq -r '.paras // .parachains | to_entries[] | "\(.key)"' "$ZOMBIE_JSON" | while read -r para_id; do
    echo "ParaId: $para_id"
    jq -r --arg pid "$para_id" "$JQ_QUERY_PARA_NODES" "$ZOMBIE_JSON" | while read -r name; do
      [[ -z "$name" ]] && continue
      local_to=$TO
      if [[ "${ZOMBIE_PROVIDER:-}" == "k8s" ]]; then
        # Fetching logs from k8s
        if ! kubectl logs "$name" -c "$name" -n "$NS" > "$TARGET_DIR/$name.log" 2>&1; then
          echo "::warning ::Failed to fetch logs for $name"
        fi
      else
        # zombienet v1 dump the logs to the `/logs` directory
        if [[ ! -f "$TARGET_DIR/$name.log" ]]; then
          # `sdk` use this pattern to store the logs in native provider
          if [[ -f "$BASE_DIR/$name/$name.log" ]]; then
            cp "$BASE_DIR/$name/$name.log" "$TARGET_DIR/$name.log"
          else
            echo "::warning ::Log file not found: $BASE_DIR/$name/$name.log"
            continue
          fi
        fi

        # send logs to loki
        if [[ -d "$LOKI_DIR_FOR_NATIVE_LOGS" && -f "$TARGET_DIR/$name.log" ]]; then
          echo -e "[para] sending logs to loki file: $LOKI_DIR_FOR_NATIVE_LOGS/to-loki.log"
          awk -v NS="$NS" -v NAME="$name" '{print NS" "NAME" " $0}' "$TARGET_DIR/$name.log" >> "$LOKI_DIR_FOR_NATIVE_LOGS/to-loki.log"
          local_to=$(($(date +%s%3N) + 60000))
        fi
      fi
      echo -e "\t$name: $(make_url "$name" "$local_to")"
    done
    echo ""
  done
}

# Main execution - Process all zombie-* directories (supports rstest with multiple tests per job)
BASE_DIRS=$(ls -dt /tmp/zombie-* 2>/dev/null || true)

if [[ -z "$BASE_DIRS" ]]; then
  echo "No zombie directories found in /tmp/zombie-*"
  exit 0
fi

for BASE_DIR in $BASE_DIRS; do
  echo "Processing directory: $BASE_DIR"

  # Make sure target directory exists
  TARGET_DIR="$BASE_DIR/logs"
  mkdir -p "$TARGET_DIR"
  ZOMBIE_JSON="$BASE_DIR/zombie.json"

  if [[ ! -f "$ZOMBIE_JSON" ]]; then
    echo "Zombie file $ZOMBIE_JSON not present, calling fallback"
    process_logs_from_fallback "$BASE_DIR" "$TARGET_DIR"
  else
    # we have a zombie.json file, let process it
    echo "Processing logs from zombie.json"
    process_logs_from_zombie_file "$BASE_DIR" "$TARGET_DIR" "$ZOMBIE_JSON"
  fi
  echo ""
done

# sleep for a minute to give alloy time to forward logs
sleep 60
