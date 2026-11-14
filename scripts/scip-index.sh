#!/usr/bin/env bash
# Generate SCIP indexes for every language in this repository and print the
# CODEGRAPH_SCIP_INDEX value to use.
#
# Why: `codegraph index` without a SCIP index produces an AST-only graph.
# On a 10k-file project that is 21 minutes; importing a pre-built SCIP index took
# 2.4s + a 3.9s import on the same tree, and adds definition edges AST-only
# never produces.
#
# Usage:
#   scripts/scip-index.sh                 # generate into .codegraph/scip/
#   export CODEGRAPH_SCIP_INDEX="$(scripts/scip-index.sh --export)"
#   codegraph index . --index-tier balanced
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="${CODEGRAPH_SCIP_DIR:-$ROOT/.codegraph/scip}"
EXPORT_ONLY=0
[ "${1:-}" = "--export" ] && EXPORT_ONLY=1

mkdir -p "$OUT"
entries=()

have() { command -v "$1" >/dev/null 2>&1; }

# --- Go (must run where the go module lives; scip-go needs the Go toolchain) ---
while IFS= read -r mod; do
  dir="$(dirname "$mod")"
  rel="${dir#"$ROOT"}"; rel="${rel#/}"
  name="$(echo "$rel" | tr '/' '_')"; [ -z "$name" ] && name="root"
  out="$OUT/go-$name.scip"
  if have scip-go; then
    ( cd "$dir" && scip-go --output "$out" >/dev/null 2>&1 ) && entries+=("$out${rel:+@$rel}")
  elif have docker; then
    ( cd "$dir" && docker run --rm -v "$dir:/src" -v "$OUT:/out" \
        -v "$(command -v scip-go):/usr/local/bin/scip-go:ro" -w /src \
        -e GOPROXY=https://goproxy.cn,direct golang:1.23-alpine \
        sh -c "apk add --no-cache git >/dev/null 2>&1; scip-go --output /out/go-$name.scip" >/dev/null 2>&1 ) \
      && entries+=("$out${rel:+@$rel}")
  else
    echo "warn: $rel has go.mod but neither scip-go nor docker is available" >&2
  fi
done < <(find "$ROOT" -name go.mod -not -path "*/vendor/*" -not -path "*/target/*" 2>/dev/null)

# --- Rust (rust-analyzer ships the exporter) ---
if [ -f "$ROOT/Cargo.toml" ] || find "$ROOT" -maxdepth 3 -name Cargo.toml -print -quit | grep -q .; then
  out="$OUT/rust.scip"
  if have rust-analyzer; then
    ( cd "$ROOT" && rust-analyzer scip . --output "$out" >/dev/null 2>&1 ) || \
    ( cd "$ROOT" && rust-analyzer scip . >/dev/null 2>&1 && mv "$ROOT/index.scip" "$out" )
    entries+=("$out")
  else
    echo "warn: rust-analyzer not found; Rust stays AST-only" >&2
  fi
fi

# --- TypeScript / JavaScript (per workspace package) ---
while IFS= read -r pkg; do
  dir="$(dirname "$pkg")"
  rel="${dir#"$ROOT"}"; rel="${rel#/}"
  name="$(echo "$rel" | tr '/' '_')"; [ -z "$name" ] && name="root"
  out="$OUT/ts-$name.scip"
  if have scip-typescript; then
    ( cd "$dir" && NODE_OPTIONS="--max-old-space-size=16384" \
        scip-typescript index --pnpm-workspaces >/dev/null 2>&1 ) || \
    ( cd "$dir" && NODE_OPTIONS="--max-old-space-size=16384" \
        scip-typescript index >/dev/null 2>&1 ) || true
    [ -f "$dir/index.scip" ] && mv "$dir/index.scip" "$out" && entries+=("$out${rel:+@$rel}")
  fi
done < <(find "$ROOT" -maxdepth 3 -name package.json -not -path "*/node_modules/*" \
           -exec grep -l '"workspaces"\|"name"' {} \; 2>/dev/null | head -20)

if [ ${#entries[@]} -eq 0 ]; then
  echo "no SCIP index produced; the index will be AST-only" >&2
  exit 1
fi

joined="$(IFS=,; echo "${entries[*]}")"
if [ "$EXPORT_ONLY" = 1 ]; then
  printf '%s\n' "$joined"
else
  echo "SCIP indexes:" >&2
  for e in "${entries[@]}"; do echo "  $e" >&2; done
  printf '\nexport CODEGRAPH_SCIP_INDEX="%s"\n' "$joined"
fi
