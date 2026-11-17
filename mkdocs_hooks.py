"""MkDocs hooks: publish repository Markdown files as site pages, and keep links that leave docs/ working.

SECURITY.md has to live at the repository root (GitHub looks for it there, and the "Report a vulnerability" button
depends on it), and ROADMAP.md, CONTRIBUTING.md and the build READMEs live beside what they describe. They are also
pages the site should carry, so they are added to the build from where they are rather than copied into docs/ and
left to drift. The site names them in `extra.repo_pages`, repository file -> page.

Those files, and the pages under docs/, link to other repository files with paths that work on GitHub
(`../ROADMAP.md`, `docs/design.md`, `../qemu/README.md`). The strict build would reject them, so each link that
leaves docs/ is pointed at the page that publishes its target, or else at the file on GitHub. Issue numbers in
running text (`#43`) become links to the issue, as they are on GitHub.
"""

import posixpath
import re
from pathlib import Path, PurePosixPath

from mkdocs.config.defaults import MkDocsConfig
from mkdocs.structure.files import File, Files
from mkdocs.structure.pages import Page

ROOT = Path(__file__).parent

# Inline Markdown links and images: `](target)` or `](target "title")`.
LINK = re.compile(r"\]\((?P<target>[^)\s]+)(?P<title>\s+\"[^\"]*\")?\)")
FENCE = re.compile(r"^\s*(```|~~~)")
# An issue or pull request number in running text: `(#43)`, `#39–#41`; not an anchor such as `](#1-download)`.
ISSUE = re.compile(r"(?<!\]\()(?<![\w/&#\[])#(\d{1,5})(?![\w-])")

# Page -> the repository file it was made from, for pages added from `extra.repo_pages`.
_sources: dict[str, str] = {}


def on_files(files: Files, config: MkDocsConfig) -> Files:
    """Add the site's repository pages to the build without copying them into docs/."""
    _sources.clear()
    for source, page in config.extra.get("repo_pages", {}).items():
        content = (ROOT / source).read_text(encoding="utf-8")
        files.append(File.generated(config, page, content=content))
        _sources[page] = source
    return files


def _rewrite(target: str, source: PurePosixPath, page: PurePosixPath, config: MkDocsConfig) -> str:
    """The link `target` in repository file `source`, as it should read on `page` of the site."""
    if re.match(r"^[a-z][a-z0-9+.-]*:", target, re.IGNORECASE) or target.startswith(("#", "/")):
        return target
    path, _, anchor = target.partition("#")
    anchor = f"#{anchor}" if anchor else ""
    resolved = posixpath.normpath(posixpath.join(str(source.parent), path))
    if resolved.startswith("../"):
        return target
    docs = PurePosixPath(config.docs_dir).name
    pages = {source: page for page, source in _sources.items()}
    if resolved in pages:
        destination = pages[resolved]
    elif resolved.startswith(f"{docs}/"):
        destination = resolved.removeprefix(f"{docs}/")
    else:
        kind = "tree" if (ROOT / resolved).is_dir() else "blob"
        return f"{config.repo_url.rstrip('/')}/{kind}/main/{resolved}{anchor}"
    return posixpath.relpath(destination, str(page.parent) or ".") + anchor


def _link_issues(text: str, config: MkDocsConfig) -> str:
    """Turn `#123` in running text into a link to that issue, as GitHub does; code spans are left alone."""
    issues = f"{config.repo_url.rstrip('/')}/issues"
    parts = re.split(r"(`+[^`]*`+)", text)
    for i in range(0, len(parts), 2):
        parts[i] = ISSUE.sub(lambda m: f"[#{m[1]}]({issues}/{m[1]})", parts[i])
    return "".join(parts)


def on_page_markdown(markdown: str, page: Page, config: MkDocsConfig, files: Files) -> str:
    """Point links that leave docs/ at the page that publishes their target, or at GitHub, and link issue numbers."""
    site_page = PurePosixPath(page.file.src_uri)
    docs = PurePosixPath(config.docs_dir).name
    source = PurePosixPath(_sources.get(page.file.src_uri, f"{docs}/{page.file.src_uri}"))
    lines, fenced = [], False
    for line in markdown.splitlines(keepends=True):
        if FENCE.match(line):
            fenced = not fenced
        # Raw HTML (the diagrams) is passed through as it is, so a Markdown link there would show as text.
        elif not fenced and not line.lstrip().startswith("<"):
            line = LINK.sub(
                lambda m: f"]({_rewrite(m['target'], source, site_page, config)}{m['title'] or ''})",
                line,
            )
            line = _link_issues(line, config)
        lines.append(line)
    return "".join(lines)
