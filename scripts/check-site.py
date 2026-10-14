#!/usr/bin/env python3
"""Check static website links, assets, code markup, scripts, catalog and CLI coverage."""
from html.parser import HTMLParser
from pathlib import Path
from urllib.parse import unquote, urlsplit
import html
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
DOCS = ROOT / "docs"
REFERENCE = {"documentation.html", "models.html", "changelog.html", "installation.html"}

class Page(HTMLParser):
    def __init__(self, name):
        super().__init__()
        self.name, self.ids, self.links, self.errors = name, set(), [], []
        self.pre, self.code = 0, 0
    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if self.code and self.name in REFERENCE:
            self.errors.append(f"{self.name}: unescaped HTML <{tag}> inside a code block")
        if tag == "pre": self.pre += 1
        if tag == "code" and self.pre: self.code += 1
        if attrs.get("id"):
            if attrs["id"] in self.ids:
                self.errors.append(f"{self.name}: duplicate id {attrs['id']}")
            self.ids.add(attrs["id"])
        for key in ("href", "src"):
            if attrs.get(key): self.links.append(attrs[key])
    def handle_endtag(self, tag):
        if tag == "code" and self.code: self.code -= 1
        if tag == "pre" and self.pre: self.pre -= 1

def main():
    errors, pages = [], {}
    for path in DOCS.glob("*.html"):
        page = Page(path.name)
        page.feed(path.read_text(encoding="utf-8"))
        errors.extend(page.errors)
        pages[path.resolve()] = page
    for path, page in pages.items():
        for link in page.links:
            url = urlsplit(link)
            if url.scheme or url.netloc: continue
            target = (path.parent / unquote(url.path)).resolve() if url.path else path
            if not target.exists():
                errors.append(f"{path.name}: missing link/asset {link}")
            elif url.fragment and target in pages and unquote(url.fragment) not in pages[target].ids:
                errors.append(f"{path.name}: missing anchor {link}")
    for filename in ("reference.js", "models.js", "installation.js", "main.js", "packages.js"):
        path = DOCS / filename
        if not path.exists(): continue
        result = subprocess.run(["node", "--check", str(path)], capture_output=True, text=True)
        if result.returncode: errors.append(result.stderr)
    result = subprocess.run([sys.executable, str(ROOT / "scripts/generate-model-catalog.py"), "--check"], capture_output=True, text=True)
    if result.returncode: errors.append(result.stdout + result.stderr)
    source = (ROOT / "src/bin/mio.rs").read_text()
    parser = source.split("fn parse(", 1)[1].split("struct CliHost", 1)[0]
    flags = set()
    for line in parser.splitlines():
        if re.match(r'\s*"-', line) and "=>" in line:
            flags.update(re.findall(r'"(-[\w-]+)"', line.split("=>", 1)[0]))
    flags.update(("-h", "--help", "-V", "--version", "--"))
    document = (DOCS / "documentation.html").read_text()
    section = re.search(r'<section\b[^>]*id="cli-flags"[^>]*>(.*?)</section>', document, re.S)
    if not section:
        errors.append("Missing CLI parameter section")
    else:
        cells = re.findall(r"<tr\b[^>]*>\s*<td\b[^>]*>(.*?)</td>", section.group(1), re.S)
        documented = set()
        for cell in cells:
            documented.update(re.findall(r"(?<![\w-])--?[\w-]*(?![\w-])", html.unescape(re.sub("<[^>]+>", "", cell))))
        missing = flags - documented
        if missing: errors.append("Missing CLI flags: " + ", ".join(sorted(missing)))
    if errors:
        print("\n".join("FAIL: " + error for error in errors), file=sys.stderr)
        return 1
    print(f"PASS: {len(pages)} pages, local links/assets/anchors, unique IDs, code markup, 5 scripts, source catalog and {len(flags)} CLI flags/aliases.")
    return 0

if __name__ == "__main__":
    sys.exit(main())
