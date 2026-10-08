#!/usr/bin/env python3
import argparse, json, pathlib, subprocess, sys

HERE = pathlib.Path(__file__).resolve().parent
LOCK = HERE / "sources.lock.json"
DEFAULT_ROOT = pathlib.Path(".cache/apple-pvg-re")

def run(cmd, cwd=None):
    print("+", " ".join(cmd))
    subprocess.run(cmd, cwd=cwd, check=True)

def safe_id(s):
    return s.replace("/", "__")

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", type=pathlib.Path, default=DEFAULT_ROOT)
    ap.add_argument("--include-reference", action="store_true")
    ap.add_argument("--list", action="store_true")
    ns = ap.parse_args()
    data = json.loads(LOCK.read_text())
    sources = data["sources"]
    for s in sources:
        if ns.list:
            print(f'{s["id"]}\t{s["class"]}\t{s.get("commit", "-")}\t{s.get("repo", s.get("url",""))}')
    if ns.list:
        return
    ns.root.mkdir(parents=True, exist_ok=True)
    for s in sources:
        if "repo" not in s:
            print(f'skip url-only: {s["id"]}')
            continue
        if s["class"] == "reference-only" and not ns.include_reference:
            print(f'skip reference-only: {s["id"]}')
            continue
        dst = ns.root / safe_id(s["id"])
        if not dst.exists():
            run(["git","clone","--filter=blob:none","--no-checkout",s["repo"],str(dst)])
        run(["git","fetch","--depth=1","origin",s["commit"]], cwd=dst)
        run(["git","checkout","--detach",s["commit"]], cwd=dst)
        paths = s.get("paths") or []
        if paths:
            run(["git","sparse-checkout","init","--cone"], cwd=dst)
            run(["git","sparse-checkout","set",*paths], cwd=dst)
            run(["git","checkout","--detach",s["commit"]], cwd=dst)

if __name__ == "__main__":
    main()
