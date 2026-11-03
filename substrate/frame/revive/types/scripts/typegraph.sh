#!/usr/bin/env bash
# Build an interactive type-graph SVG of every type defined in `pallet-revive-types`. Nodes are the
# structs, enums, and type aliases declared in this crate; edges are containment relationships
# (struct fields, enum variant payloads, type-alias targets). External types such as `Vec`,
# `Option`, `H256`, `Address`, etc. are filtered out.
#
# The resulting SVG embeds a small script that, when the file is opened in any browser, lets you
# click a node to highlight every type that transitively depends on it.
#
# Pipeline:
#   1. `cargo +nightly rustdoc` emits a JSON dump of the crate's public and private items.
#   2. `jq` walks the JSON, collects local nodes (`crate_id == 0`), and for each one chases the
#      field/variant indirection back to the referenced types, keeping only references that resolve
#      to other local nodes.
#   3. `dot` lays the graph out as SVG.
#   4. `awk` injects an inline `<style>` and `<script>` block before `</svg>` so the resulting file
#      is self-contained and clickable.
#
# Usage: ./typegraph.sh [output.svg]
#        Defaults to <workspace target>/types-typegraph.svg.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TYPES_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
TYPES_MANIFEST="$TYPES_DIR/Cargo.toml"

for tool in cargo jq dot awk; do
	command -v "$tool" >/dev/null 2>&1 || {
		echo "error: required tool \"$tool\" not found on PATH" >&2
		exit 1
	}
done

METADATA="$(cargo metadata --manifest-path "$TYPES_MANIFEST" --format-version 1 --no-deps)"
TARGET_DIR="$(jq -r '.target_directory' <<<"$METADATA")"

OUTPUT="${1:-$TARGET_DIR/types-typegraph.svg}"

TMP_PREFIX="${TMPDIR:-/tmp}/types-typegraph"
DOT_FILE="$(mktemp "$TMP_PREFIX.XXXXXX.dot")"
RAW_SVG="$(mktemp "$TMP_PREFIX.XXXXXX.svg")"
INJECT_FILE="$(mktemp "$TMP_PREFIX.XXXXXX.html")"
trap 'rm -f "$DOT_FILE" "$RAW_SVG" "$INJECT_FILE"' EXIT

echo "[1/4] Generating rustdoc JSON for pallet-revive-types..." >&2
cargo +nightly rustdoc \
	--manifest-path "$TYPES_MANIFEST" \
	-- -Z unstable-options --output-format json --document-private-items \
	>/dev/null

JSON="$TARGET_DIR/doc/pallet_revive_types.json"
[[ -f "$JSON" ]] || {
	echo "error: rustdoc JSON not found at $JSON" >&2
	exit 1
}

echo "[2/4] Extracting nodes and edges from JSON..." >&2
jq -r '
	# Collect every `resolved_path.id` reachable from a Type value.
	def type_refs($t):
		[$t | .. | objects | .resolved_path? // empty | .id];

	# Resolve a field-item ID to the resolved-path IDs found in its
	# `struct_field` type. Tuple-variant slots can be null and are skipped.
	def field_refs($idx; $fid):
		if $fid == null then []
		else
			($idx[$fid|tostring] // null) as $f
			| if $f == null then [] else type_refs($f.inner.struct_field // null) end
		end;

	# Walk a struct.kind value, returning all referenced type IDs.
	def struct_refs($idx; $k):
		if ($k|type) == "string" then []
		elif ($k|has("tuple")) then [$k.tuple[]        | field_refs($idx; .)] | flatten
		elif ($k|has("plain")) then [$k.plain.fields[] | field_refs($idx; .)] | flatten
		else [] end;

	# Walk a single enum variant by ID.
	def variant_refs($idx; $vid):
		($idx[$vid|tostring] // null) as $v
		| if $v == null then []
		  else
			($v.inner.variant.kind) as $k
			| if ($k|type) == "string" then []
			  elif ($k|has("tuple"))  then [$k.tuple[]         | field_refs($idx; .)] | flatten
			  elif ($k|has("struct")) then [$k.struct.fields[] | field_refs($idx; .)] | flatten
			  else [] end
		  end;

	def enum_refs($idx; $e):
		[$e.variants[] | variant_refs($idx; .)] | flatten;

	# Top-level dispatch for the three node kinds we care about.
	def item_refs($idx; $item):
		if   ($item.inner | has("struct"))     then struct_refs($idx; $item.inner.struct.kind)
		elif ($item.inner | has("enum"))       then enum_refs($idx; $item.inner.enum)
		elif ($item.inner | has("type_alias")) then type_refs($item.inner.type_alias.type)
		else [] end;

	.index as $idx
	| .paths as $paths
	# Every locally-declared struct, enum, and type alias.
	| [ $idx | to_entries[]
		| select(.value.crate_id == 0)
		| select(.value.name != null)
		| select(
			(.value.inner.struct // .value.inner.enum // .value.inner.type_alias) != null
		  )
		| ($paths[.key] // null) as $path
		| select($path != null)
		| { id: .key, name: .value.name, path: ($path.path[1:] | join("::")) } ]
	  | sort_by(.name) as $all_local
	| $all_local as $nodes
	| ( $nodes
		| group_by(.name)
		| map(select(length > 1) | .[].name)
		| unique
	  ) as $duplicate_names
	| ($nodes | map({ (.id): .path }) | add // {}) as $path_by_id
	# Edges: for every node, dedupe its referenced IDs and keep only those
	# that resolve back into the local set.
	| ( $nodes
		| map(
			. as $n
			| (item_refs($idx; $idx[$n.id]) | map(tostring) | unique)
			| map(select($path_by_id[.] != null))
			| map({ src: $n.path, dst: $path_by_id[.] })
		  )
		| flatten
		| unique_by("\(.src) \(.dst)")
	  ) as $edges
	| "digraph TypeGraph {",
	  "  graph [rankdir=LR, splines=true, overlap=false, fontname=\"Inter,Helvetica,Arial,sans-serif\", bgcolor=\"#fafafa\"];",
	  "  node  [shape=box, style=\"rounded,filled\", fillcolor=\"#ffffff\", color=\"#888888\", fontname=\"Inter,Helvetica,Arial,sans-serif\", fontsize=11];",
	  "  edge  [color=\"#aaaaaa\", arrowsize=0.7];",
	  ( $nodes[]
		| . as $node
		| (
			if (.name | (
				test("^[A-Z][A-Za-z0-9]*Versioned(?:Input|Output)Payload$")
				or test("^[A-Z][A-Za-z0-9]*(?:Input|Output)PayloadV[0-9]+$")
			  )) then "#cfe5ff"
			else "#ffe2c4"
			end
		  ) as $fill
		| (
			if ($duplicate_names | index($node.name)) == null then $node.name
			else $node.path
			end
		  ) as $label
		| "  \"\($node.path)\" [id=\"node-\($node.id)\", label=\"\($label)\", fillcolor=\"\($fill)\"];"
	  ),
	  ( $edges[] | "  \"\(.src)\" -> \"\(.dst)\";" ),
	  "}"
' "$JSON" >"$DOT_FILE"

echo "[3/4] Rendering SVG with dot..." >&2
dot -Tsvg "$DOT_FILE" >"$RAW_SVG"

echo "[4/4] Embedding click-highlight script..." >&2
cat >"$INJECT_FILE" <<'INJECT'
<style>
  svg.has-selection g.node:not(.selected):not(.related) > * { opacity: 0.12; }
  svg.has-selection g.edge:not(.related) > * { opacity: 0.06; }
  g.node { cursor: pointer; }
  g.node.selected polygon, g.node.selected ellipse,
  g.node.selected rect,    g.node.selected path { stroke: #ff5722; stroke-width: 2.5; }
  g.node.selected text { font-weight: bold; }
  g.node.related  polygon, g.node.related  ellipse,
  g.node.related  rect,    g.node.related  path { stroke: #ff9933; stroke-width: 1.8; }
  g.edge.related  path    { stroke: #ff9933; stroke-width: 1.8; }
  g.edge.related  polygon { fill:   #ff9933; stroke: #ff9933; }
</style>
<script type="text/ecmascript"><![CDATA[
  (function () {
    var svg = document.documentElement;
    var nodes = Array.prototype.slice.call(svg.querySelectorAll('g.node'));
    var edges = Array.prototype.slice.call(svg.querySelectorAll('g.edge'));
    var nodeByName = {};
    nodes.forEach(function (n) {
      var t = n.querySelector('title');
      if (t) nodeByName[t.textContent] = n;
    });
    var inEdges = {};
    edges.forEach(function (e) {
      var t = e.querySelector('title');
      if (!t) return;
      var m = t.textContent.match(/^(.+?)->(.+)$/);
      if (!m) return;
      var src = m[1], dst = m[2];
      (inEdges[dst]  = inEdges[dst]  || []).push({ edge: e, other: src });
    });
    function clearAll() {
      svg.classList.remove('has-selection');
      nodes.forEach(function (n) { n.classList.remove('selected', 'related'); });
      edges.forEach(function (e) { e.classList.remove('related'); });
    }
    function walk(start, edgesByKey) {
      var seen = {}; seen[start] = true;
      var queue = [start];
      while (queue.length) {
        var cur = queue.shift();
        var list = edgesByKey[cur] || [];
        for (var i = 0; i < list.length; i++) {
          var rec = list[i];
          rec.edge.classList.add('related');
          if (!seen[rec.other]) {
            seen[rec.other] = true;
            var on = nodeByName[rec.other];
            if (on) on.classList.add('related');
            queue.push(rec.other);
          }
        }
      }
    }
    function highlight(name) {
      clearAll();
      var n = nodeByName[name];
      if (!n) return;
      svg.classList.add('has-selection');
      n.classList.add('selected');
      walk(name, inEdges);
    }
    nodes.forEach(function (n) {
      n.addEventListener('click', function (ev) {
        ev.stopPropagation();
        var t = n.querySelector('title');
        if (t) highlight(t.textContent);
      });
    });
    svg.addEventListener('click', clearAll);
  })();
]]></script>
INJECT

mkdir -p "$(dirname "$OUTPUT")"
awk -v injf="$INJECT_FILE" '
	/<\/svg>/ {
		while ((getline line < injf) > 0) print line
		close(injf)
	}
	{ print }
' "$RAW_SVG" >"$OUTPUT"

NODE_COUNT="$(grep -c ' \[id="node-' "$DOT_FILE" || true)"
EDGE_COUNT="$(grep -c ' -> ' "$DOT_FILE" || true)"
echo "Wrote $OUTPUT  (nodes=$NODE_COUNT, edges=$EDGE_COUNT)" >&2
