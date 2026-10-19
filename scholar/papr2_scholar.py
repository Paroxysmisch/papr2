"""Google Scholar helper for papr2, run with `uv run`.

papr2 talks to this script over stdin/stdout using JSON:

    search:  argv = ["search", <query>, <max results>]
             stdout = {"results": [Result, ...]}
    bibtex:  argv = ["bibtex"], stdin = a Result's "raw" object
             stdout = {"bibtex": "@article{...}"}

On failure it prints {"error": "..."} and exits with status 1.
"""

import json
import sys
from itertools import islice


def fail(message):
    print(json.dumps({"error": message}))
    sys.exit(1)


def as_list(authors):
    if isinstance(authors, list):
        return [a.strip() for a in authors if a and a.strip()]
    if isinstance(authors, str):
        return [a.strip() for a in authors.split(" and ") if a.strip()]
    return []


def http_only(url):
    # Scholar sometimes reports "javascript:void(0)" as the PDF link.
    return url if isinstance(url, str) and url.startswith(("http://", "https://")) else ""


def search(query, limit):
    from scholarly import scholarly

    results = []
    for pub in islice(scholarly.search_pubs(query), limit):
        bib = pub.get("bib", {})
        results.append(
            {
                "title": bib.get("title", ""),
                "authors": as_list(bib.get("author")),
                "year": str(bib.get("pub_year", "")),
                "venue": bib.get("venue", ""),
                "abstract": bib.get("abstract", ""),
                "pub_url": http_only(pub.get("pub_url", "")),
                "eprint_url": http_only(pub.get("eprint_url", "")),
                "num_citations": pub.get("num_citations", 0),
                # Everything scholarly needs to fetch the BibTeX later.
                "raw": pub,
            }
        )
    return {"results": results}


def bibtex(raw):
    """Google Scholar's own BibTeX for a search result, unmodified.

    (scholarly.bibtex() re-serialises its parsed fields, which adds
    non-standard ones like `pub_year` and truncated `venue`s.)
    """
    from scholarly import scholarly
    from scholarly.publication_parser import PublicationParser

    url = raw.get("url_scholarbib")
    if not url:
        fail("Google Scholar didn't provide a BibTeX link for this result")
    # Same steps as scholarly's fill(): find the BibTeX link, then fetch it.
    nav = scholarly._Scholarly__nav
    bibtex_url = PublicationParser(nav)._get_bibtex(url)
    return {"bibtex": nav._get_page(bibtex_url).strip()}


def main():
    if len(sys.argv) < 2:
        fail("usage: papr2_scholar.py search <query> [limit] | bibtex < raw.json")
    try:
        if sys.argv[1] == "search":
            limit = int(sys.argv[3]) if len(sys.argv) > 3 else 10
            out = search(sys.argv[2], limit)
        elif sys.argv[1] == "bibtex":
            out = bibtex(json.load(sys.stdin))
        else:
            fail(f"unknown command {sys.argv[1]!r}")
    except Exception as e:  # report every failure (e.g. Scholar blocking us) as JSON
        fail(f"{type(e).__name__}: {e}")
    print(json.dumps(out, default=str))


if __name__ == "__main__":
    main()
