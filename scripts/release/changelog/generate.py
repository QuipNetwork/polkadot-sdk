#!/usr/bin/env python3
"""Generate the release changelog outputs from a folder of prdoc files.

Produces three files in --output-dir:
  changelog_body.md  Markdown fragment placed into the GitHub release body by
                     build-changelogs.sh (substituted into the rendered draft AFTER
                     tera runs, so prdoc content is never parsed as a template).
                     Kept under --max-body-chars via a deterministic degradation
                     ladder; the driving script computes that budget from the size
                     of the surrounding document so the final body stays under
                     GitHub's 125000-char cap. Named distinctly from CHANGELOG.md
                     so the two can coexist on case-insensitive filesystems (macOS).
  CHANGELOG.md       The complete, untruncated changelog, attached as a release asset:
                     same topic structure plus a per-audience appendix.
  changelog.json     Machine-readable changelog, attached as a release asset. The format
                     is described by schema.json next to this script.

Topics are resolved per prdoc: an explicit `topic:` field (matched case-insensitively
against an id or label from --topics) wins; otherwise the topic is derived from the
affected crates via the ordered glob patterns in --topics; otherwise the last topic
(Misc) is used.

Malformed prdocs (empty files, non-mapping YAML, unknown audiences) are tolerated with
a warning so a single bad file cannot abort a release; --check turns warnings into a
non-zero exit for CI.

Only needs python3 + PyYAML. No network access, no containers.
"""

import argparse
import fnmatch
import json
import re
import sys
from pathlib import Path

import yaml

REPO_URL = "https://github.com/paritytech/polkadot-sdk"
SCHEMA_VERSION = 1
CANONICAL_AUDIENCES = ("Node Dev", "Runtime Dev", "Node Operator", "Runtime User")
AUDIENCE_ALIASES = {re.sub(r"[\W_]", "", a).lower(): a for a in CANONICAL_AUDIENCES}
# Description truncation limits tried in order until the body fits the budget; the
# final (0, drop-all) rung in main() removes descriptions entirely and always converges.
TRUNCATION_LADDER = (700, 550, 450, 400, 350, 300, 250)
BUMP_RANK = {"major": 3, "minor": 2, "patch": 1, "none": 0}
CRATE_LIST_CAP = 6
# Within each topic section (release body and CHANGELOG.md alike), entries are
# subgrouped by audience ("🛠️ `Runtime Dev`"), so the label is read once per
# subgroup instead of once per entry.
# Stable icon per audience for the subgroup headings; the text label always stays
# alongside so the heading remains searchable.
AUDIENCE_ICONS = {
    "Node Dev": "🔧",
    "Runtime Dev": "🛠️",
    "Node Operator": "🖥️",
    "Runtime User": "👤",
}
# Legacy prdocs escaped tera-significant text for the old pipeline ({{ "{{" }}closure...);
# the new pipeline never tera-processes prdoc content, so unescape it back.
TERA_ESCAPE_RE = re.compile(r'\{\{\s*"(\{\{|\}\})"\s*\}\}')
FENCE_RE = re.compile(r"^ {0,3}(`{3,}|~{3,})")


def log(msg):
    print(msg, file=sys.stderr)


def text_or_none(value):
    """YAML scalars can parse as dates/bools/ints; the JSON model wants strings."""
    return None if value is None else str(value)


def one_line(value):
    """Titles are interpolated into single-line markdown constructs (headings, index
    bullets); collapse any whitespace, including newlines from block scalars."""
    return " ".join(str(value).split())


def clean_description(value):
    return TERA_ESCAPE_RE.sub(r"\1", str(value or "")).strip()


def normalize_audiences(value, pr, warnings):
    if value is None or value == "":
        warnings.append(f"pr_{pr}: empty audience")
        return ["Unknown"]
    values = value if isinstance(value, list) else [value]
    seen = []
    for v in values:
        key = re.sub(r"[\W_]", "", str(v)).lower()
        aud = AUDIENCE_ALIASES.get(key)
        if aud is None:
            aud = one_line(v) or "Unknown"
            warnings.append(f"pr_{pr}: unknown audience {str(v)!r}")
        if aud not in seen:
            seen.append(aud)
    return seen or ["Unknown"]


def ordered_audience_union(docs):
    all_auds = [a for d in docs for a in d["audiences"]]
    ordered = [a for a in CANONICAL_AUDIENCES if a in all_auds]
    ordered += [a for a in all_auds if a not in ordered]
    return list(dict.fromkeys(ordered))


def load_topics(path):
    data = yaml.safe_load(Path(path).read_text(encoding="utf-8"))
    topics = data.get("topics") if isinstance(data, dict) else None
    if not topics:
        sys.exit(f"error: no topics found in {path}")
    for t in topics:
        if not t.get("id") or not t.get("label"):
            sys.exit(f"error: topic without id/label in {path}: {t}")
        t.setdefault("patterns", [])
    return topics


def resolve_topic(explicit, crates, topics, pr, warnings):
    fallback = topics[-1]
    if explicit:
        wanted = str(explicit).strip().lower()
        for t in topics:
            if wanted in (t["id"].lower(), t["label"].lower()):
                return t, "explicit"
        warnings.append(f"pr_{pr}: unknown topic {explicit!r}, falling back to derivation")
    best = None
    for crate in crates:
        for idx, t in enumerate(topics):
            if any(fnmatch.fnmatch(crate["name"], p) for p in t["patterns"]):
                if best is None or idx < best:
                    best = idx
                break
    # The catch-all last topic counts as fallback, not a real derivation.
    if best is not None and topics[best] is not fallback:
        return topics[best], "derived"
    return fallback, "fallback"


def max_bump_rank(crates):
    return max((BUMP_RANK.get(c["bump"], 0) for c in crates), default=0)


def pr_number_from_filename(name):
    """PR number from pr_NNNN.prdoc; legacy files may carry a suffix (pr_1234-some-title.prdoc)."""
    m = re.match(r"pr_(\d+)", name)
    return int(m.group(1)) if m else None


def load_prdocs(prdoc_dir, topics, warnings):
    """Returns (entries, expected_prs): entries for every parseable prdoc, and the PR
    numbers that made it in (skipped files are warned about, not silently dropped)."""
    entries = []
    files = []
    for f in Path(prdoc_dir).glob("pr_*.prdoc"):
        pr = pr_number_from_filename(f.name)
        if pr is not None:
            files.append((pr, f))
        else:
            warnings.append(f"skipping unrecognized file name: {f.name}")
    for pr, f in sorted(files):
        try:
            content = yaml.safe_load(f.read_text(encoding="utf-8"))
        except yaml.YAMLError as e:
            warnings.append(f"skipping {f.name}: YAML parse error: {e}")
            continue
        if not isinstance(content, dict):
            warnings.append(f"skipping {f.name}: not a YAML mapping (empty file?)")
            continue

        docs = []
        for item in content.get("doc") or []:
            if not isinstance(item, dict):
                continue
            title = item.get("title")
            docs.append({
                "audiences": normalize_audiences(item.get("audience"), pr, warnings),
                "title": one_line(title) if title is not None else None,
                "description": clean_description(item.get("description")),
            })

        crates = []
        for c in content.get("crates") or []:
            if isinstance(c, str):
                crates.append({"name": c, "bump": None, "note": None})
            elif isinstance(c, dict) and c.get("name"):
                bump = c.get("bump")
                crates.append({
                    "name": str(c["name"]),
                    "bump": str(bump).strip().lower() if bump else None,
                    "note": text_or_none(c.get("note")),
                })

        raw_migrations = content.get("migrations") or {}
        migrations = {
            "db": [
                {"name": text_or_none(m.get("name")) or "",
                 "description": clean_description(m.get("description"))}
                for m in (raw_migrations.get("db") or []) if isinstance(m, dict)
            ],
            "runtime": [
                {"reference": text_or_none(m.get("reference")),
                 "description": clean_description(m.get("description"))}
                for m in (raw_migrations.get("runtime") or []) if isinstance(m, dict)
            ],
        }

        host_functions = []
        for hf in content.get("host_functions") or []:
            if isinstance(hf, str):
                host_functions.append({"name": hf, "description": None})
            elif isinstance(hf, dict) and hf.get("name"):
                host_functions.append({
                    "name": str(hf["name"]),
                    "description": text_or_none(hf.get("description")),
                })

        topic, source = resolve_topic(content.get("topic"), crates, topics, pr, warnings)
        title = content.get("title")
        entries.append({
            "pr": pr,
            "title": one_line(title) if title is not None else f"PR #{pr}",
            "url": f"{REPO_URL}/pull/{pr}",
            "author": text_or_none(content.get("author")),
            "audiences": ordered_audience_union(docs) if docs else [],
            "topic": {"id": topic["id"], "label": topic["label"], "source": source},
            "breaking": any(c["bump"] == "major" for c in crates),
            "docs": docs,
            "crates": crates,
            "migrations": migrations,
            "host_functions": host_functions,
        })
    return entries, sorted(e["pr"] for e in entries)


def load_audience_descriptions(schema_path):
    """Audience descriptions from the local prdoc schema (JSON with // comment lines)."""
    try:
        raw = Path(schema_path).read_text(encoding="utf-8")
        schema = json.loads(re.sub(r"^\s*//.*$", "", raw, flags=re.MULTILINE))
        return {
            item["const"]: item["description"]
            for item in schema["$defs"]["audience_id"]["oneOf"]
            if "const" in item and "description" in item
        }
    except (OSError, KeyError, json.JSONDecodeError):
        return {}


def scan_fences(lines):
    """Yield (line, in_code) tracking ```/~~~ fenced blocks CommonMark-ish: a fence
    opens with 3+ backticks or tildes at <=3-space indent (info string allowed) and
    closes only on a matching-char run at least as long with nothing else on the line.
    Both fence delimiter lines report in_code=True."""
    fence = None  # (char, run length) of the open fence
    for line in lines:
        m = FENCE_RE.match(line)
        if fence is None:
            if m:
                fence = (m.group(1)[0], len(m.group(1)))
                yield line, True
            else:
                yield line, False
        else:
            yield line, True
            stripped = line.strip()
            if (m and m.group(1)[0] == fence[0] and set(stripped) == {fence[0]}
                    and len(stripped) >= fence[1]):
                fence = None


def demote_headings(text, min_level=5):
    """Demote markdown headings inside prdoc descriptions below our entry headings (####),
    so a description's own `# Description` doesn't outrank the changelog structure."""
    out = []
    for line, in_code in scan_fences(text.splitlines()):
        if not in_code:
            m = re.match(r"^(#{1,6})\s", line)
            if m and len(m.group(1)) < min_level:
                line = "#" * min_level + line[len(m.group(1)):]
        out.append(line)
    return "\n".join(out)


# Lines that start (or are) a markdown block construct and must never be merged
# into the preceding line: lists, headings, blockquotes, tables, HTML, reference
# definitions, and setext-underline/thematic-break lines.
BLOCK_LINE_RE = re.compile(
    r"^\s*(?:[-*+]\s|\d+[.)]\s|#{1,6}\s|>|\||<|\[[^\]]+\]:)"
)
RULE_LINE_RE = re.compile(r"^\s*[=\-_*]{2,}\s*$")


def unwrap_soft_breaks(text):
    """Join hard-wrapped lines inside plain paragraphs. GitHub renders every single
    newline in a release body as a hard line break (comment-style rendering), so a
    prdoc description wrapped at 80-100 columns shows as a ragged, narrow paragraph.
    Joining soft-wrapped lines matches CommonMark's soft-break semantics and lets
    the text reflow to the reader's window. Structural lines (lists, headings,
    quotes, tables, HTML, code fences, explicit hard breaks) are left untouched."""
    out = []
    prev_in_code = True  # never join into a line we have not seen
    for line, in_code in scan_fences(text.splitlines()):
        joinable = (
            out
            and not in_code
            and not prev_in_code
            and line.strip()
            and out[-1].strip()
            and not BLOCK_LINE_RE.match(line)
            and not RULE_LINE_RE.match(line)
            and not BLOCK_LINE_RE.match(out[-1])
            and not out[-1].endswith("  ")  # markdown two-space hard break
            and not out[-1].rstrip().lower().endswith("<br>")
            and not out[-1].rstrip().lower().endswith("<br/>")
        )
        if joinable:
            out[-1] = out[-1].rstrip() + " " + line.strip()
        else:
            out.append(line)
        prev_in_code = in_code
    return "\n".join(out)


# The linked forms `[![alt](img)](target)` and `[<img ...>](target)` (badge /
# clickable-screenshot patterns) must match before the plain image forms, or the
# inner image alone would be rewritten and leave a nested link behind. PR
# descriptions pasted from GitHub also carry raw HTML `<img ... src="...">` tags
# (single-line; that is how GitHub emits attachments).
LINKED_IMAGE_RE = re.compile(r"\[!\[([^\]]*)\]\([^)]*\)\]\(([^)\s]+)(?:\s+\"[^\"]*\")?\)")
IMAGE_RE = re.compile(r"!\[([^\]]*)\]\(([^)\s]+)(?:\s+\"[^\"]*\")?\)")
LINKED_IMG_TAG_RE = re.compile(
    r"\[\s*(<img\b[^>]*>)\s*\]\(([^)\s]+)(?:\s+\"[^\"]*\")?\)", re.IGNORECASE
)
IMG_TAG_RE = re.compile(r"<img\b[^>]*>", re.IGNORECASE)
IMG_SRC_RE = re.compile(r"\bsrc\s*=\s*[\"']([^\"']+)[\"']", re.IGNORECASE)
IMG_ALT_RE = re.compile(r"\balt\s*=\s*[\"']([^\"']*)[\"']", re.IGNORECASE)
CODE_SPAN_RE = re.compile(r"`[^`]*`")
INDENTED_CODE_RE = re.compile(r"^(?: {4,}|\t)")


def replace_images(text):
    """Turn embedded images into plain links. Images throw off the entry layout on
    the release page and do nothing for non-rendering consumers; worse, GitHub
    attachment URLs resolve to expiring signed links, so an embedded image in a
    release body eventually 404s. A link to the attachment keeps the reference
    without the noise. Fenced/indented code blocks and inline code spans are left
    untouched."""
    def link(m):
        label = m.group(1).strip() or "attached image"
        return f"[📷 {label}]({m.group(2)})"

    def img_label(tag):
        alt = IMG_ALT_RE.search(tag)
        return (alt.group(1).strip() if alt else "") or "attached image"

    def html_link(m):
        src = IMG_SRC_RE.search(m.group(0))
        if not src:
            return m.group(0)
        return f"[📷 {img_label(m.group(0))}]({src.group(1)})"

    def linked_html(m):
        return f"[📷 {img_label(m.group(1))}]({m.group(2)})"

    def replace_in(segment):
        segment = IMAGE_RE.sub(link, LINKED_IMAGE_RE.sub(link, segment))
        return IMG_TAG_RE.sub(html_link, LINKED_IMG_TAG_RE.sub(linked_html, segment))

    out = []
    for line, in_code in scan_fences(text.splitlines()):
        if not in_code and not INDENTED_CODE_RE.match(line):
            # Rewrite between inline code spans only, so literal syntax examples
            # like `![alt](url)` survive verbatim.
            parts = CODE_SPAN_RE.split(line)
            spans = CODE_SPAN_RE.findall(line)
            line = "".join(
                replace_in(part) + (spans[i] if i < len(spans) else "")
                for i, part in enumerate(parts)
            )
        out.append(line)
    return "\n".join(out)


def rendered_description(text):
    """Prdoc description prepared for markdown rendering (raw text stays in the JSON)."""
    return demote_headings(replace_images(unwrap_soft_breaks(text)))


UNSAFE_PREFIX_RE = re.compile(r"```|~~~|<!--|<details\b", re.IGNORECASE)

# Non-void tags GitHub's HTML sanitizer lets through (minus <details>, tracked
# separately). A cut that leaves one of these open bleeds its formatting into
# every entry that follows on the release page, since GitHub only auto-closes
# dangling tags at the end of the whole document.
_HTML_FORMATTING_TAGS = (
    "a|abbr|b|bdo|blockquote|caption|cite|code|dd|del|dfn|div|dl|dt|em|figcaption|"
    "figure|h[1-6]|i|ins|kbd|li|mark|ol|p|pre|q|rp|rt|ruby|s|samp|small|span|strike|"
    "strong|sub|summary|sup|table|tbody|td|tfoot|th|thead|time|tr|tt|ul|var"
)
HTML_TAG_RE = re.compile(rf"<(/?)(?:{_HTML_FORMATTING_TAGS})\b[^>]*?(/?)>", re.IGNORECASE)
OPEN_TAG_TAIL_RE = re.compile(r"<[A-Za-z][^>]*$")
HEADING_LINE_RE = re.compile(r"^\s*#{1,6}\s")


def html_tag_balance(text):
    """Net count of formatting tags opened but not closed in `text` (stray closers
    can drive it negative; only a positive balance makes a cut unsafe)."""
    balance = 0
    for m in HTML_TAG_RE.finditer(text):
        if not m.group(2):  # self-closing tags open nothing
            balance += -1 if m.group(1) else 1
    return balance


def truncate_markdown(text, limit):
    """Cut at the last paragraph boundary within `limit` that is *safe*: outside code
    fences, HTML comments, and <details> blocks — a cut must never corrupt the markdown
    that follows the entry. Returns (snippet, truncated); the snippet is empty when no
    safe cut exists (e.g. the text opens with a huge code block)."""
    if len(text) <= limit:
        return text, False
    lines = text.splitlines()
    fence_states = [in_code for _, in_code in scan_fences(lines)]
    safe_end = 0
    pos = 0
    comment_depth = 0
    details_depth = 0
    html_depth = 0
    for i, line in enumerate(lines):
        end = pos + len(line)
        if end > limit:
            break
        if not fence_states[i]:
            comment_depth += line.count("<!--") - line.count("-->")
            details_depth += (len(re.findall(r"<details\b", line, re.IGNORECASE))
                              - line.lower().count("</details>"))
            html_depth += html_tag_balance(line)
        clean = (not fence_states[i] and comment_depth <= 0 and details_depth <= 0
                 and html_depth <= 0)
        # A heading is never a cut point: cutting there would orphan the heading
        # right before the content it introduces.
        at_paragraph_end = (line.strip()
                            and (i + 1 == len(lines) or not lines[i + 1].strip())
                            and not HEADING_LINE_RE.match(line))
        if clean and at_paragraph_end:
            safe_end = end
        pos = end + 1
    if safe_end:
        return text[:safe_end].rstrip(), True
    # No complete paragraph fits: cut mid-paragraph at a sentence/word boundary, but
    # only when the prefix holds none of the constructs a cut could break.
    prefix = text[:limit]
    if UNSAFE_PREFIX_RE.search(prefix):
        return "", True
    cut = prefix.rfind(". ")
    if cut >= limit * 0.4:
        snippet = prefix[: cut + 1].rstrip()
    else:
        cut = prefix.rfind(" ")
        snippet = prefix[: cut if cut > 0 else limit - 1].rstrip()
    # A word-boundary cut can land on or inside a heading line, stranding a
    # (possibly mangled) heading as the snippet's last content; drop it.
    while snippet and HEADING_LINE_RE.match(snippet.splitlines()[-1]):
        snippet = "\n".join(snippet.splitlines()[:-1]).rstrip()
    # Bail out rather than leave an HTML tag open (or cut through one): a dangling
    # tag reformats everything after the entry on the release page.
    if snippet and (html_tag_balance(snippet) > 0 or OPEN_TAG_TAIL_RE.search(snippet)):
        return "", True
    return snippet, True


def with_ellipsis(text):
    """Mark a truncation cut with an ellipsis at the end of the cut line itself.
    A structural last line (list, table, quote, heading, HTML, indented code) gets
    the marker as its own paragraph instead so the construct is not corrupted; cuts
    never end inside code fences (see truncate_markdown), so inline appending is
    safe."""
    last = text.splitlines()[-1]
    if BLOCK_LINE_RE.match(last) or RULE_LINE_RE.match(last) or INDENTED_CODE_RE.match(last):
        return text + "\n\n…"
    return text + " …"


def fmt_crate(crate):
    bump = f" ({crate['bump']})" if crate["bump"] else ""
    return f"`{crate['name']}`{bump}"


def fmt_crate_list(crates, cap=CRATE_LIST_CAP, bumps=True):
    """bumps=False renders names only — used where the bump level is implied by
    context (e.g. the Breaking list, whose crates are all major by construction).
    A falsy cap means unlimited."""
    listed = crates[:cap] if cap else crates
    shown = ", ".join(fmt_crate(c) if bumps else f"`{c['name']}`" for c in listed)
    extra = len(crates) - len(listed)
    return shown + (f" and {extra} more" if extra > 0 else "")


def migration_flags(entry):
    flags = []
    if entry["migrations"]["runtime"]:
        flags.append("🗄️ *runtime migration*")
    if entry["migrations"]["db"]:
        flags.append("🗄️ *db migration*")
    return flags


def first_line(text, limit=200):
    line = text.strip().splitlines()[0] if text.strip() else ""
    return line if len(line) <= limit else line[: limit - 1].rstrip() + "…"


def audience_heading(audience):
    icon = AUDIENCE_ICONS.get(audience)
    return f"{icon + ' ' if icon else ''}`{audience}`"


def doc_for_audience(entry, audience):
    """The doc item written for `audience` (prdocs carry one per audience), falling
    back to the first one."""
    for doc in entry["docs"]:
        if audience in doc["audiences"]:
            return doc
    return entry["docs"][0] if entry["docs"] else None


def group_by_audience(group):
    """Partition one topic's entries into audience subgroups, largest audience first.
    Every entry appears exactly once: under the most common of its own audiences within
    this topic, so a multi-audience entry lands in the topic's dominant subgroup rather
    than being duplicated a few lines apart. Entries without any audience come last,
    keyed None (rendered without a subgroup heading)."""
    counts = {}
    for e in group:
        for a in e["audiences"]:
            counts[a] = counts.get(a, 0) + 1

    def rank(audience):
        canon = (CANONICAL_AUDIENCES.index(audience) if audience in CANONICAL_AUDIENCES
                 else len(CANONICAL_AUDIENCES))
        return (-counts[audience], canon, audience)

    buckets = {a: [] for a in sorted(counts, key=rank)}
    orphans = []
    for e in group:
        if e["audiences"]:
            buckets[min(e["audiences"], key=rank)].append(e)
        else:
            orphans.append(e)
    subgroups = [(a, entries) for a, entries in buckets.items() if entries]
    if orphans:
        subgroups.append((None, orphans))
    return subgroups


SCHEMA_PATH = "scripts/release/changelog/schema.json"


def schema_url(tag):
    """URL of the schema describing changelog.json, pinned at the release tag so it
    matches the document even after the schema evolves on master."""
    return f"https://raw.githubusercontent.com/paritytech/polkadot-sdk/{tag or 'master'}/{SCHEMA_PATH}"


def asset_note(tag):
    if tag:
        base = f"{REPO_URL}/releases/download/{tag}"
        changelog, data = f"{base}/CHANGELOG.md", f"{base}/changelog.json"
        # Plain link text on purpose: code spans nested in link text render fine on
        # GitHub but are mangled by some editors/formatters and simpler renderers.
        return (
            f"> 📦 The complete changelog with full descriptions is attached to this release as "
            f"[CHANGELOG.md]({changelog}), and in machine-readable form as "
            f"[changelog.json]({data})."
        )
    return (
        "> 📦 The complete changelog with full descriptions is attached to this release as "
        "`CHANGELOG.md`, and in machine-readable form as `changelog.json`."
    )


def render_breaking_index(entries, cap=4):
    lines = []
    for e in (e for e in entries if e["breaking"]):
        majors = [c for c in e["crates"] if c["bump"] == "major"]
        shown = fmt_crate_list(majors, cap=cap, bumps=False)
        flags = " ".join(migration_flags(e))
        lines.append(
            f"- **{e['topic']['label']}**: [#{e['pr']}]({e['url']}) {e['title']} ({shown})"
            + (f" {flags}" if flags else "")
        )
    return lines


def render_breaking_section(entries, cap=4):
    breaking_count = sum(1 for e in entries if e["breaking"])
    lines = ["## 💥 Breaking Changes", ""]
    if breaking_count:
        lines += [
            f"{breaking_count} change{'s are' if breaking_count != 1 else ' is'} breaking "
            "according to [Rust SemVer](https://doc.rust-lang.org/cargo/reference/semver.html) "
            "(each bumps the major version of at least one crate). "
            "Details in the 💥-marked entries below.",
            "",
        ]
        lines += render_breaking_index(entries, cap=cap) + [""]
    else:
        lines += ["None in this release.", ""]
    return lines


def render_migrations_section(entries, full=False):
    runtime, db = [], []
    for e in entries:
        for m in e["migrations"]["runtime"]:
            desc = m["description"] if full else first_line(m["description"])
            ref = f" ({m['reference']})" if m["reference"] else ""
            runtime.append(f"- [#{e['pr']}]({e['url']}){ref}: {desc}")
        for m in e["migrations"]["db"]:
            desc = m["description"] if full else first_line(m["description"])
            name = f" **{m['name']}**" if m["name"] else ""
            db.append(f"- [#{e['pr']}]({e['url']}){name}: {desc}")
    if not runtime and not db:
        return []
    lines = ["### 🗄️ Migrations", ""]
    if runtime:
        lines += ["**Runtime:**", ""] + runtime + [""]
    if db:
        lines += ["**DB:**", ""] + db + [""]
    return lines


def render_entry_body(entry, limit, drop_low, audience=None):
    """One condensed entry. `audience` is the enclosing subgroup: it selects which doc
    item's description to show and, when the entry targets further audiences, adds a
    small cross-reference (multi-audience entries render once, not per subgroup)."""
    lines = [f"#### {'💥 ' if entry['breaking'] else ''}[#{entry['pr']}]({entry['url']}): {entry['title']}"]
    if audience is not None:
        others = [a for a in entry["audiences"] if a != audience]
        if others:
            lines.append("<sub>Also for " + ", ".join(f"`{a}`" for a in others) + "</sub>")
    lines.append("")

    doc = doc_for_audience(entry, audience) if audience else (entry["docs"][0] if entry["docs"] else None)
    description = doc["description"] if doc else ""
    if (limit > 0 and description
            and not (drop_low and max_bump_rank(entry["crates"]) <= BUMP_RANK["patch"])):
        text, truncated = truncate_markdown(rendered_description(description), limit)
        if text and truncated:
            lines += [with_ellipsis(text), ""]
        elif text:
            lines += [text, ""]
        elif truncated:
            # Nothing safe to show (e.g. the description opens with a huge code
            # block); the bare marker still signals there is more in CHANGELOG.md.
            lines += ["…", ""]

    breaking_crates = [c for c in entry["crates"] if c["bump"] == "major"]
    other_crates = [c for c in entry["crates"] if c["bump"] != "major"]
    if breaking_crates:
        # Names only: everything in this list is a major bump by definition.
        lines.append(f"**Breaking**: {fmt_crate_list(breaking_crates, bumps=False)}")
    for m in entry["migrations"]["runtime"]:
        lines.append(f"🗄️ **Runtime migration**: {first_line(m['description'])}")
    for m in entry["migrations"]["db"]:
        lines.append(f"🗄️ **DB migration**: {first_line(m['description'] or m['name'])}")
    if entry["host_functions"]:
        names = ", ".join(f"`{hf['name']}`" for hf in entry["host_functions"])
        lines.append(f"⚙️ **Host functions**: {names}")
    if other_crates:
        label = "Other crates" if breaking_crates else "Crates"
        lines.append(f"<sub>{label}: {fmt_crate_list(other_crates)}</sub>")
    lines.append("")
    return lines


def group_by_topic(entries, topics):
    groups = []
    for t in topics:
        group = [e for e in entries if e["topic"]["id"] == t["id"]]
        if group:
            groups.append((t, group))
    return groups


def render_topic_sections(entries, topics, render_entry):
    """Topic sections shared by the body and CHANGELOG.md. `render_entry(entry, audience)`
    renders one entry; `audience` is the enclosing subgroup (None for entries whose
    prdoc names no audience, rendered last without a subgroup heading)."""
    lines = []
    for topic, group in group_by_topic(entries, topics):
        lines += [f"### {topic['label']}", ""]
        for audience, sub in group_by_audience(group):
            if audience is not None:
                lines += [audience_heading(audience), ""]
            for e in sub:
                lines += render_entry(e, audience)
    return lines


def render_body_fragment(entries, topics, tag, limit, drop_low):
    lines = [asset_note(tag), ""]
    lines += render_breaking_section(entries)
    # Migrations are orthogonal to breaking changes (they routinely ship under
    # minor/patch bumps), so the index renders regardless of the breaking count.
    lines += render_migrations_section(entries)
    lines += ["## Changelog", ""]
    lines += render_topic_sections(
        entries, topics,
        lambda e, audience: render_entry_body(e, limit, drop_low, audience),
    )
    return "\n".join(lines).rstrip() + "\n"


def render_full_entry(entry):
    lines = [f"#### {'💥 ' if entry['breaking'] else ''}[#{entry['pr']}]({entry['url']}): {entry['title']}", ""]
    multi = len(entry["docs"]) > 1
    for doc in entry["docs"]:
        if multi or doc["title"]:
            heading = doc["title"] or entry["title"]
            lines += [f"**For {' '.join(f'`{a}`' for a in doc['audiences'])}**: {heading}", ""]
        if doc["description"]:
            lines += [rendered_description(doc["description"]), ""]
    if entry["crates"]:
        crate_lines = []
        for c in entry["crates"]:
            crate_lines.append(f"  - {fmt_crate(c)}" + (f": {c['note']}" if c["note"] else ""))
        lines += ["**Crates**:"] + crate_lines + [""]
    for m in entry["migrations"]["runtime"]:
        ref = f" ({m['reference']})" if m["reference"] else ""
        lines += [f"🗄️ **Runtime migration**{ref}: {m['description']}", ""]
    for m in entry["migrations"]["db"]:
        name = f" (**{m['name']}**)" if m["name"] else ""
        lines += [f"🗄️ **DB migration**{name}: {m['description']}", ""]
    for hf in entry["host_functions"]:
        desc = f": {hf['description']}" if hf["description"] else ""
        lines += [f"⚙️ **Host function** `{hf['name']}`{desc}", ""]
    return lines


def render_full_changelog(entries, topics, tag, previous_tag, audience_descriptions):
    title_ref = tag or "unreleased"
    lines = [f"# Polkadot SDK {title_ref}: Complete Changelog", ""]
    if tag and previous_tag:
        lines += [f"This release contains the changes from `{previous_tag}` to `{tag}`.", ""]
    lines += [
        "The release page shows a condensed version of this changelog. A machine-readable",
        "version is attached to the release as `changelog.json`.",
        "",
    ]
    lines += render_breaking_section(entries, cap=0)
    lines += render_migrations_section(entries, full=True)
    lines += ["## Changes by Topic", ""]
    lines += render_topic_sections(entries, topics, lambda e, _audience: render_full_entry(e))
    lines += ["## Appendix: Changes by Audience", ""]
    audiences = [a for a in CANONICAL_AUDIENCES if any(a in e["audiences"] for e in entries)]
    audiences += sorted(
        {a for e in entries for a in e["audiences"]} - set(CANONICAL_AUDIENCES)
    )
    for audience in audiences:
        lines += [f"### Changelog for `{audience}`", ""]
        if audience in audience_descriptions:
            lines += [f"ℹ️ These changes are relevant to: {audience_descriptions[audience]}", ""]
        for e in sorted(entries, key=lambda e: e["pr"]):
            for doc in e["docs"]:
                if audience in doc["audiences"]:
                    heading = doc["title"] or e["title"]
                    lines += [f"#### [#{e['pr']}]({e['url']}): {heading}", ""]
                    if doc["description"]:
                        lines += [rendered_description(doc["description"]), ""]
    return "\n".join(lines).rstrip() + "\n"


def build_json(entries, topics, tag, previous_tag, version, generated_at):
    return {
        "$schema": schema_url(tag),
        "schema_version": SCHEMA_VERSION,
        "release": {
            "tag": tag,
            "previous_tag": previous_tag,
            "version": version,
            "generated_at": generated_at,
        },
        "topics": [{"id": t["id"], "label": t["label"]} for t in topics],
        "changes": entries,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--prdoc-dir", required=True, help="folder containing pr_*.prdoc files")
    parser.add_argument("--topics", required=True, help="path to the topic taxonomy (topics.yaml)")
    parser.add_argument("--output-dir", required=True)
    parser.add_argument("--tag", default=None,
                        help="FINAL release tag (no -rcN suffix), e.g. polkadot-stable2606; "
                             "used for asset download URLs and the changelog.json identity")
    parser.add_argument("--previous-tag", default=None)
    parser.add_argument("--version", default=None, help="release version, e.g. stable2606")
    parser.add_argument("--schema", default=None,
                        help="path to prdoc schema_user.json (for audience descriptions); "
                             "defaults to schema_user.json next to --prdoc-dir's parent")
    parser.add_argument("--generated-at", default=None,
                        help="ISO timestamp recorded in changelog.json (omitted -> null, "
                             "keeping reruns byte-identical)")
    parser.add_argument("--max-body-chars", type=int, default=115000,
                        help="budget for changelog_body.md; build-changelogs.sh passes the "
                             "exact budget measured from the rendered outer document")
    parser.add_argument("--check", action="store_true",
                        help="fail (exit 1) on any warning-level problem, e.g. a skipped "
                             "prdoc, unknown topic/audience, or body over budget")
    args = parser.parse_args()

    prdoc_dir = Path(args.prdoc_dir)
    if not prdoc_dir.is_dir():
        sys.exit(f"error: {prdoc_dir} is not a directory")

    warnings = []
    topics = load_topics(args.topics)
    entries, expected_prs = load_prdocs(prdoc_dir, topics, warnings)
    if not entries:
        sys.exit(f"error: no usable pr_*.prdoc files found in {prdoc_dir}")

    topic_order = {t["id"]: i for i, t in enumerate(topics)}
    entries.sort(key=lambda e: (topic_order[e["topic"]["id"]], not e["breaking"], e["pr"]))

    schema_path = args.schema or (prdoc_dir.parent / "schema_user.json"
                                  if (prdoc_dir.parent / "schema_user.json").is_file()
                                  else prdoc_dir / "schema_user.json")
    audience_descriptions = load_audience_descriptions(schema_path)

    # Degradation ladder: shrink descriptions until the fragment fits the budget, then
    # drop descriptions of entries without any major/minor bump, and as the guaranteed
    # terminal rung drop all descriptions (titles/tags/crates always fit in practice).
    steps = [(limit, False) for limit in TRUNCATION_LADDER]
    steps += [(TRUNCATION_LADDER[-1], True), (0, True)]
    body = None
    for step, (limit, drop_low) in enumerate(steps):
        body = render_body_fragment(entries, topics, args.tag, limit, drop_low)
        if len(body) <= args.max_body_chars:
            if step > 0:
                log(f"warning: body over budget, degraded to step {step} "
                    f"(limit={limit}, drop_low={drop_low}), {len(body)} chars")
            break
    else:
        warnings.append(
            f"body fragment still over budget after full degradation: "
            f"{len(body)} > {args.max_body_chars} chars"
        )

    full = render_full_changelog(entries, topics, args.tag, args.previous_tag,
                                 audience_descriptions)
    data = build_json(entries, topics, args.tag, args.previous_tag, args.version,
                      args.generated_at)

    # Completeness invariants; a violation here is a generator bug, never ship it.
    output_prs = sorted(c["pr"] for c in data["changes"])
    if expected_prs != output_prs:
        sys.exit(f"error: loaded/output PR mismatch: {set(expected_prs) ^ set(output_prs)}")
    missing_in_body = [pr for pr in expected_prs if f"[#{pr}]" not in body]
    if missing_in_body:
        sys.exit(f"error: entries missing from the body fragment: {missing_in_body}")
    json.loads(json.dumps(data))  # round-trip sanity

    out = Path(args.output_dir)
    out.mkdir(parents=True, exist_ok=True)
    (out / "changelog_body.md").write_text(body, encoding="utf-8")
    (out / "CHANGELOG.md").write_text(full, encoding="utf-8")
    (out / "changelog.json").write_text(
        json.dumps(data, indent=1, ensure_ascii=False) + "\n", encoding="utf-8"
    )

    by_source = {}
    by_topic = {}
    for e in entries:
        by_source[e["topic"]["source"]] = by_source.get(e["topic"]["source"], 0) + 1
        by_topic[e["topic"]["label"]] = by_topic.get(e["topic"]["label"], 0) + 1
    log(f"{len(entries)} changes | breaking: {sum(1 for e in entries if e['breaking'])} | "
        f"body: {len(body)} chars (budget {args.max_body_chars})")
    log(f"topic sources: {by_source}")
    log("entries per topic: " + ", ".join(f"{k}: {v}" for k, v in by_topic.items()))
    for w in warnings:
        log(f"warning: {w}")

    if warnings and args.check:
        sys.exit(1)


if __name__ == "__main__":
    main()
