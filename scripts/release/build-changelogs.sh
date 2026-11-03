#!/usr/bin/env bash
set -euo pipefail

# Builds the release draft (RELEASE_DRAFT.md) plus the CHANGELOG.md/changelog.json
# release assets.
#
# Flow:
#  1. tera renders the outer document (release info, runtimes, docker, free notes)
#     from templates/ with a placeholder marker where the changelog goes.
#  2. The changelog fragment budget is computed from the measured size of that
#     outer document, so the final body provably stays under GitHub's 125000-char
#     release-body cap (including manual free notes).
#  3. changelog/generate.py (python3 + PyYAML, no network, no container) renders
#     the changelog from the prdoc files within that budget.
#  4. The fragment is substituted for the marker AFTER tera, so prdoc content is
#     never parsed as a tera template.
#
# The final tag for asset URLs and the changelog identity is REF2 with any -rcN
# suffix stripped: drafts are built from rc tags but published under the final tag.

export PRODUCT=polkadot
export VERSION=${VERSION:-stable2606}
export REF1=${REF1:-'HEAD'}
export REF2=${REF2:-}
export RUSTC_STABLE=${RUSTC_STABLE:-'1.0'}
export NO_RUNTIMES=${NO_RUNTIMES:-'false'}
export CRATES_ONLY=${CRATES_ONLY:-'false'}
# Exported by the release workflow; defaulted here so local runs render too.
export STABLE_VERSION=${STABLE_VERSION:-${VERSION%%-*}}
export NODE_VERSION=${NODE_VERSION:-v0.0.0-local}

GITHUB_BODY_CAP=125000
CHANGELOG_MARKER='<!-- POLKADOT-SDK-CHANGELOG-BODY -->'

PROJECT_ROOT=$(git rev-parse --show-toplevel)
RELEASE_DIR="$PROJECT_ROOT/scripts/release"
TEMPLATES_DIR="$RELEASE_DIR/templates"
# Private work dir: never reuse or delete caller-provided paths.
WORKDIR=$(mktemp -d)

if ! python3 -c 'import yaml' 2>/dev/null; then
	echo "error: PyYAML is required: python3 -m pip install pyyaml" >&2
	exit 1
fi

# The draft is generated from an rc tag but published under the final tag; asset
# download URLs are tag-scoped, so everything user-facing uses the final tag.
FINAL_TAG="${REF2%-rc*}"
if [[ "$FINAL_TAG" != "$REF2" ]]; then
	echo "Using final tag '$FINAL_TAG' for asset URLs (draft tag: '$REF2')"
	export REF2="$FINAL_TAG"
fi

# Assemble the srtool digest context for the runtimes section.
if [[ "$NO_RUNTIMES" == "false" && "$CRATES_ONLY" == "false" ]]; then
  ASSET_HUB_WESTEND_DIGEST=${ASSET_HUB_WESTEND_DIGEST:-"$RELEASE_DIR/digests/asset-hub-westend-srtool-digest.json"}
  BRIDGE_HUB_WESTEND_DIGEST=${BRIDGE_HUB_WESTEND_DIGEST:-"$RELEASE_DIR/digests/bridge-hub-westend-srtool-digest.json"}
  COLLECTIVES_WESTEND_DIGEST=${COLLECTIVES_WESTEND_DIGEST:-"$RELEASE_DIR/digests/collectives-westend-srtool-digest.json"}
  CORETIME_WESTEND_DIGEST=${CORETIME_WESTEND_DIGEST:-"$RELEASE_DIR/digests/coretime-westend-srtool-digest.json"}
  GLUTTON_WESTEND_DIGEST=${GLUTTON_WESTEND_DIGEST:-"$RELEASE_DIR/digests/glutton-westend-srtool-digest.json"}
  PEOPLE_WESTEND_DIGEST=${PEOPLE_WESTEND_DIGEST:-"$RELEASE_DIR/digests/people-westend-srtool-digest.json"}
  WESTEND_DIGEST=${WESTEND_DIGEST:-"$RELEASE_DIR/digests/westend-srtool-digest.json"}

  jq \
        --slurpfile srtool_asset_hub_westend "$ASSET_HUB_WESTEND_DIGEST" \
        --slurpfile srtool_bridge_hub_westend "$BRIDGE_HUB_WESTEND_DIGEST" \
        --slurpfile srtool_collectives_westend "$COLLECTIVES_WESTEND_DIGEST" \
        --slurpfile srtool_coretime_westend "$CORETIME_WESTEND_DIGEST" \
        --slurpfile srtool_glutton_westend "$GLUTTON_WESTEND_DIGEST" \
        --slurpfile srtool_people_westend "$PEOPLE_WESTEND_DIGEST" \
        --slurpfile srtool_westend "$WESTEND_DIGEST" \
        -n '{
            srtool: [
              { order: 10, name: "Westend", data: $srtool_westend[0] },
              { order: 11, name: "Westend AssetHub", data: $srtool_asset_hub_westend[0] },
              { order: 12, name: "Westend BridgeHub", data: $srtool_bridge_hub_westend[0] },
              { order: 13, name: "Westend Collectives", data: $srtool_collectives_westend[0] },
              { order: 14, name: "Westend Coretime", data: $srtool_coretime_westend[0] },
              { order: 15, name: "Westend Glutton", data: $srtool_glutton_westend[0] },
              { order: 16, name: "Westend People", data: $srtool_people_westend[0] }
        ] }' > "$WORKDIR/context.json"
else
  echo '{}' > "$WORKDIR/context.json"
fi

# Render the outer document (still carrying the changelog marker).
tera --env --env-key env \
	--include-path "$TEMPLATES_DIR" \
	--template "$TEMPLATES_DIR/template.md.tera" \
	"$WORKDIR/context.json" > "$WORKDIR/draft_outer.md"

if ! grep -qF "$CHANGELOG_MARKER" "$WORKDIR/draft_outer.md"; then
	echo "error: changelog marker missing from rendered draft (templates/changes.md.tera)" >&2
	exit 1
fi

# Budget for the changelog fragment = the real cap minus everything around it.
OUTER_CHARS=$(wc -m < "$WORKDIR/draft_outer.md")
BODY_BUDGET=$((GITHUB_BODY_CAP - OUTER_CHARS + ${#CHANGELOG_MARKER}))
echo "Outer document: $OUTER_CHARS chars -> changelog budget: $BODY_BUDGET chars"

# Generate the changelog: body fragment + the two release assets.
CHANGELOG_OUT="$WORKDIR/changelog"
python3 "$RELEASE_DIR/changelog/generate.py" \
	--prdoc-dir "$PROJECT_ROOT/prdoc/$VERSION" \
	--topics "$PROJECT_ROOT/prdoc/topics.yaml" \
	--output-dir "$CHANGELOG_OUT" \
	--tag "$FINAL_TAG" \
	--previous-tag "$REF1" \
	--version "$VERSION" \
	--max-body-chars "$BODY_BUDGET" \
	${GENERATED_AT:+--generated-at "$GENERATED_AT"}

# Substitute the fragment for the marker (plain text replacement, not tera).
python3 - "$WORKDIR/draft_outer.md" "$CHANGELOG_OUT/changelog_body.md" \
	"$RELEASE_DIR/RELEASE_DRAFT.md" "$CHANGELOG_MARKER" <<'PY'
import sys
outer_path, fragment_path, out_path, marker = sys.argv[1:5]
outer = open(outer_path, encoding="utf-8").read()
# Trailing newline: tera's include trimming can leave the next section heading
# directly after the marker, and the fragment must not butt against it.
fragment = open(fragment_path, encoding="utf-8").read().rstrip("\n") + "\n"
assert marker in outer, "changelog marker vanished between render and substitution"
open(out_path, "w", encoding="utf-8").write(outer.replace(marker, fragment, 1))
PY

# Place the outputs where the release workflow picks them up.
cp "$WORKDIR/context.json" "$RELEASE_DIR/context.json"
cp "$CHANGELOG_OUT/CHANGELOG.md" "$RELEASE_DIR/CHANGELOG.md"
cp "$CHANGELOG_OUT/changelog.json" "$RELEASE_DIR/changelog.json"

DRAFT_CHARS=$(wc -m < "$RELEASE_DIR/RELEASE_DRAFT.md")
echo "Release draft: $RELEASE_DIR/RELEASE_DRAFT.md ($DRAFT_CHARS chars, cap $GITHUB_BODY_CAP)"
echo "Release assets: $RELEASE_DIR/CHANGELOG.md, $RELEASE_DIR/changelog.json"
if (( DRAFT_CHARS > GITHUB_BODY_CAP )); then
	echo "::warning::release draft exceeds the GitHub body cap ($DRAFT_CHARS > $GITHUB_BODY_CAP)" >&2
fi
