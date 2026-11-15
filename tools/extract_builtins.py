#!/usr/bin/env python3
"""Extract the compiler builtin API from ``roc docs --builtins`` HTML.

The generated ``data/builtins.json`` is embedded in ``hardroc`` so hover,
completion, and signature help for builtins work without a compiler
subprocess at runtime.

Usage::

    python3 tools/extract_builtins.py
    python3 tools/extract_builtins.py --input /tmp/roc-builtins/out \
        --output data/builtins.json

Only the Python standard library is used.
"""

from __future__ import annotations

import argparse
import html
import json
import re
import subprocess
import sys
from html.parser import HTMLParser
from pathlib import Path

DEFAULT_INPUT = "/tmp/roc-builtins/out"
DEFAULT_OUTPUT = "data/builtins.json"

VOID_TAGS = {
    "area",
    "base",
    "br",
    "col",
    "embed",
    "hr",
    "img",
    "input",
    "link",
    "meta",
    "param",
    "source",
    "track",
    "wbr",
}


class Node:
    """A tiny HTML element node; text children are plain ``str`` values."""

    __slots__ = ("tag", "attrs", "children")

    def __init__(self, tag: str, attrs: dict[str, str | None], children: list) -> None:
        self.tag = tag
        self.attrs = attrs
        self.children = children

    def classes(self) -> list[str]:
        return (self.attrs.get("class") or "").split()

    def has_class(self, name: str) -> bool:
        return name in self.classes()

    def get(self, name: str) -> str | None:
        return self.attrs.get(name)


class TreeBuilder(HTMLParser):
    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self.root = Node("#document", {}, [])
        self.stack = [self.root]

    def handle_starttag(self, tag: str, attrs) -> None:
        node = Node(tag, dict(attrs), [])
        self.stack[-1].children.append(node)
        if tag not in VOID_TAGS:
            self.stack.append(node)

    def handle_startendtag(self, tag: str, attrs) -> None:
        self.stack[-1].children.append(Node(tag, dict(attrs), []))

    def handle_endtag(self, tag: str) -> None:
        for index in range(len(self.stack) - 1, 0, -1):
            if self.stack[index].tag == tag:
                del self.stack[index:]
                return

    def handle_data(self, data: str) -> None:
        if not data:
            return
        children = self.stack[-1].children
        if children and isinstance(children[-1], str):
            children[-1] += data
        else:
            children.append(data)


def parse_fragment(source: str) -> Node:
    builder = TreeBuilder()
    builder.feed(source)
    builder.close()
    return builder.root


def iter_nodes(node: Node):
    for child in node.children:
        if isinstance(child, Node):
            yield child
            yield from iter_nodes(child)


def find_first(node: Node, predicate, include_self: bool = False) -> Node | None:
    if include_self and isinstance(node, Node) and predicate(node):
        return node
    for child in node.children:
        if isinstance(child, Node):
            if predicate(child):
                return child
            found = find_first(child, predicate)
            if found is not None:
                return found
    return None


def direct_child(node: Node, predicate) -> Node | None:
    for child in node.children:
        if isinstance(child, Node) and predicate(child):
            return child
    return None


def text_content(node: Node | str) -> str:
    if isinstance(node, str):
        return node
    return "".join(text_content(child) for child in node.children)


def normalize_inline(text: str) -> str:
    text = text.replace("\r\n", "\n").replace("\r", "\n")
    text = re.sub(r"[ \t]*\n[ \t]*", "\n", text)
    text = re.sub(r"[ \t]+", " ", text)
    return text.strip()


def render_inline(node: Node | str) -> str:
    if isinstance(node, str):
        return node
    tag = node.tag
    if tag == "code":
        inner = normalize_inline(text_content(node))
        return f"`{inner}`" if inner else ""
    if tag == "br":
        return "\n"
    if tag in ("strong", "b"):
        inner = normalize_inline(render_inline_children(node))
        return f"**{inner}**" if inner else ""
    if tag in ("em", "i"):
        inner = normalize_inline(render_inline_children(node))
        return f"*{inner}*" if inner else ""
    if tag == "pre":
        return normalize_inline(text_content(node))
    return render_inline_children(node)


def render_inline_children(node: Node) -> str:
    return "".join(render_inline(child) for child in node.children)


def rcode_block(pre: Node) -> str:
    code = text_content(pre).replace("\r\n", "\n").replace("\r", "\n")
    code = code.strip("\n")
    code = "\n".join(line.rstrip() for line in code.split("\n"))
    if not code.strip():
        return ""
    return f"```roc\n{code}\n```"


def direct_list_items(node: Node) -> list[Node]:
    return [child for child in node.children if isinstance(child, Node) and child.tag == "li"]


def render_list(node: Node) -> str:
    ordered = node.tag == "ol"
    lines: list[str] = []
    for index, item in enumerate(direct_list_items(node)):
        content = "\n".join(render_blocks(item.children))
        prefix = f"{index + 1}. " if ordered else "- "
        if not content.strip():
            lines.append(prefix.rstrip())
            continue
        first, *rest = content.splitlines()
        lines.append(prefix + first)
        indent = " " * len(prefix)
        lines.extend(indent + line for line in rest)
    return "\n".join(lines)


def cell_text(cell: Node) -> str:
    return normalize_inline(render_inline_children(cell)).replace("|", "\\|").replace("\n", " ")


def render_table(table: Node) -> str:
    rows: list[list[str]] = []
    for node in iter_nodes(table):
        if node.tag != "tr":
            continue
        cells = [
            cell_text(child)
            for child in node.children
            if isinstance(child, Node) and child.tag in ("th", "td")
        ]
        if cells:
            rows.append(cells)
    if not rows:
        return ""
    width = max(len(row) for row in rows)
    rows = [row + [""] * (width - len(row)) for row in rows]
    header = rows[0]
    lines = [
        "| " + " | ".join(header) + " |",
        "| " + " | ".join("---" for _ in header) + " |",
    ]
    for row in rows[1:]:
        lines.append("| " + " | ".join(row) + " |")
    return "\n".join(lines)


def render_blocks(nodes: list) -> list[str]:
    blocks: list[str] = []
    inline: list[str] = []

    def flush_inline() -> None:
        if not inline:
            return
        text = normalize_inline("".join(inline))
        inline.clear()
        if text:
            blocks.append(text)

    for child in nodes:
        if isinstance(child, str):
            inline.append(child)
            continue
        tag = child.tag
        if tag == "p":
            flush_inline()
            text = normalize_inline(render_inline_children(child))
            if text:
                blocks.append(text)
        elif tag in ("div", "section", "article", "main"):
            flush_inline()
            blocks.extend(render_blocks(child.children))
        elif tag == "pre":
            flush_inline()
            code = rcode_block(child)
            if code:
                blocks.append(code)
        elif tag in ("ul", "ol"):
            flush_inline()
            text = render_list(child)
            if text:
                blocks.append(text)
        elif tag == "table":
            flush_inline()
            text = render_table(child)
            if text:
                blocks.append(text)
        elif re.fullmatch(r"h[1-6]", tag):
            flush_inline()
            level = int(tag[1])
            text = normalize_inline(render_inline_children(child))
            if text:
                blocks.append("#" * level + " " + text)
        elif tag == "blockquote":
            flush_inline()
            inner = "\n\n".join(render_blocks(child.children))
            if inner:
                blocks.append("\n".join("> " + line for line in inner.splitlines()))
        elif tag == "hr":
            flush_inline()
            blocks.append("---")
        elif tag == "br":
            inline.append("\n")
        else:
            inline.append(render_inline(child))
    flush_inline()
    return blocks


def render_document(node: Node) -> str | None:
    blocks = [block for block in render_blocks(node.children) if block.strip()]
    text = "\n\n".join(block.strip("\n") for block in blocks)
    text = re.sub(r"\n{3,}", "\n\n", text).strip()
    return text or None


def main_content(source: str) -> str:
    start = source.find('<div class="main-content">')
    if start == -1:
        start = source.find("<main>")
    if start == -1:
        return source
    end = source.find("<footer", start)
    if end == -1:
        end = source.find("</main>", start)
    if end == -1:
        end = len(source)
    return source[start:end]


def load_module_page(path: Path) -> tuple[str | None, str | None, str | None, list[Node]]:
    source = path.read_text(encoding="utf-8")
    tree = parse_fragment(main_content(source))
    main = find_first(
        tree,
        lambda node: node.tag == "div" and node.has_class("main-content"),
        include_self=True,
    )
    if main is None:
        main = tree

    heading = find_first(main, lambda node: node.tag == "h1" and node.has_class("module-name"))
    module_name = normalize_inline(text_content(heading)) if heading is not None else None

    doc_node = direct_child(main, lambda node: node.tag == "div" and node.has_class("module-doc"))
    module_doc = render_document(doc_node) if doc_node is not None else None

    type_def_node = direct_child(
        main,
        lambda node: node.tag == "pre" and node.has_class("entry-type-def"),
    )
    module_type_def = None
    if type_def_node is not None:
        module_type_def = text_content(type_def_node).strip() or None

    articles = [
        node
        for node in iter_nodes(main)
        if node.tag == "article"
        and (node.has_class("entry-type") or node.has_class("entry-value"))
        and node.get("id")
    ]
    return module_name, module_doc, module_type_def, articles


def strip_signature_name(signature: str, short_name: str) -> str | None:
    text = signature.replace("\r\n", "\n").replace("\r", "\n").lstrip()
    if text.startswith(short_name):
        rest = text[len(short_name) :].lstrip()
        if rest.startswith(":"):
            text = rest[1:].lstrip(" \t")
    elif ":" in text:
        text = text.split(":", 1)[1].lstrip(" \t")
    text = text.rstrip()
    return text or None


def extract_item(article: Node, module_name: str) -> dict | None:
    entry_id = article.get("id")
    if not entry_id:
        return None
    kind = "type" if article.has_class("entry-type") else "value"
    full_name = f"{module_name}.{entry_id}"
    short_name = entry_id.rsplit(".", 1)[-1]

    signature = None
    signature_wrapper = direct_child(
        article,
        lambda node: node.tag == "div" and node.has_class("entry-signature"),
    )
    signature_pre = None
    if signature_wrapper is not None:
        signature_pre = find_first(
            signature_wrapper,
            lambda node: node.tag == "pre" and node.has_class("entry-signature-pre"),
        )
    if signature_pre is not None:
        signature = strip_signature_name(text_content(signature_pre), short_name)

    type_def = None
    if kind == "type":
        type_def_pre = direct_child(
            article,
            lambda node: node.tag == "pre" and node.has_class("entry-type-def"),
        )
        if type_def_pre is not None:
            type_def = text_content(type_def_pre).strip() or None

    doc_node = direct_child(
        article,
        lambda node: node.tag == "div" and node.has_class("entry-doc"),
    )
    doc = render_document(doc_node) if doc_node is not None else None

    return {
        "name": full_name,
        "module": module_name,
        "short_name": short_name,
        "kind": kind,
        "signature": signature,
        "doc": doc,
        "type_def": type_def,
    }


def load_search_docs(input_dir: Path) -> dict[str, str]:
    path = input_dir / "search-index.html"
    if not path.is_file():
        return {}
    source = path.read_text(encoding="utf-8")
    docs: dict[str, str] = {}
    pattern = re.compile(
        r'<article\b[^>]*data-title="([^"]+)"[^>]*data-href="[^"]*"[^>]*>(.*?)</article>',
        re.DOTALL,
    )
    for match in pattern.finditer(source):
        title = html.unescape(match.group(1))
        body = match.group(2).strip()
        body = re.sub(r"^<p>", "", body)
        body = re.sub(r"</p>$", "", body).strip()
        body = re.sub(r"<[^>]+>", "", body)
        body = html.unescape(body).strip()
        if body:
            docs[title] = body
    return docs


def extract_module(path: Path, search_docs: dict[str, str]) -> dict | None:
    module_name, module_doc, module_type_def, articles = load_module_page(path / "index.html")
    if not module_name:
        return None

    if module_doc is None:
        module_doc = search_docs.get(module_name)

    items: dict[str, dict] = {
        module_name: {
            "name": module_name,
            "module": module_name,
            "short_name": module_name,
            "kind": "type",
            "signature": None,
            "doc": module_doc,
            "type_def": module_type_def,
        }
    }

    for article in articles:
        item = extract_item(article, module_name)
        if item is None:
            continue
        if item["doc"] is None:
            item["doc"] = search_docs.get(item["name"])
        items[item["name"]] = item

    return {
        "name": module_name,
        "doc": module_doc,
        "items": [items[name] for name in sorted(items)],
    }


def detect_version(explicit: str | None) -> str:
    if explicit:
        return explicit.strip()
    for command in (["roc", "version"], ["roc", "--version"]):
        try:
            result = subprocess.run(
                command,
                capture_output=True,
                text=True,
                timeout=10,
                check=False,
            )
        except (OSError, subprocess.SubprocessError):
            continue
        if result.returncode != 0:
            continue
        output = result.stdout.strip() or result.stderr.strip()
        if not output:
            continue
        match = re.search(r"version\s+(\S+)", output, re.IGNORECASE)
        if match:
            return match.group(1)
        return output.splitlines()[0].strip()
    return "unknown"


def ensure_input(input_dir: Path) -> None:
    if input_dir.is_dir() and any((child / "index.html").is_file() for child in input_dir.iterdir()):
        return
    print(f"{input_dir} is missing builtin docs; running `roc docs --builtins`...", file=sys.stderr)
    try:
        subprocess.run(
            ["roc", "docs", "--builtins", f"--output={input_dir}"],
            check=True,
        )
    except (OSError, subprocess.SubprocessError) as error:
        raise SystemExit(
            f"could not generate builtin docs in {input_dir}: {error}; "
            "run `roc docs --builtins --output=DIR` first"
        ) from error


def extract(input_dir: Path) -> list[dict]:
    search_docs = load_search_docs(input_dir)
    modules: list[dict] = []
    for child in sorted(input_dir.iterdir()):
        if not child.is_dir() or not (child / "index.html").is_file():
            continue
        module = extract_module(child, search_docs)
        if module is not None:
            modules.append(module)
    modules.sort(key=lambda module: module["name"])
    return modules


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Extract roc builtin documentation into data/builtins.json"
    )
    parser.add_argument("--input", default=DEFAULT_INPUT, help="directory containing roc docs HTML")
    parser.add_argument("--output", default=DEFAULT_OUTPUT, help="output JSON path")
    parser.add_argument(
        "--version",
        default=None,
        help="override the Roc version string (defaults to `roc version` output)",
    )
    args = parser.parse_args(argv)

    input_dir = Path(args.input)
    output = Path(args.output)
    ensure_input(input_dir)

    version = detect_version(args.version)
    modules = extract(input_dir)
    if not modules:
        print(f"error: no builtin modules found in {input_dir}", file=sys.stderr)
        return 1

    database = {"version": version, "modules": modules}
    output.parent.mkdir(parents=True, exist_ok=True)
    with output.open("w", encoding="utf-8", newline="\n") as handle:
        json.dump(database, handle, indent=2, ensure_ascii=False)
        handle.write("\n")

    total_items = sum(len(module["items"]) for module in modules)
    print(f"Extracted {len(modules)} modules and {total_items} items to {output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
