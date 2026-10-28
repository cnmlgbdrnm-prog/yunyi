#!/usr/bin/env python3
"""一次性输出服务器上所有分享的摘要；只读，不改任何文件。"""
import json, os, subprocess, sys


def read(path):
    try:
        with open(path, "r", encoding="utf-8") as f:
            return f.read()
    except OSError:
        return None


def read_json(path):
    raw = read(path)
    if raw is None:
        return None
    try:
        return json.loads(raw)
    except ValueError:
        return None


def unit_state(name):
    try:
        r = subprocess.run(
            ["systemctl", "is-active", name],
            capture_output=True,
            text=True,
            timeout=5,
        )
        return r.stdout.strip() or None
    except (OSError, subprocess.SubprocessError):
        return None


def main():
    root = sys.argv[1] if len(sys.argv) > 1 else "/srv/vpnshare"
    try:
        names = sorted(os.listdir(root))
    except OSError:
        names = []
    out = []
    for name in names:
        d = os.path.join(root, name)
        if not os.path.isdir(d):
            continue
        manifest = read(os.path.join(d, "share.json"))
        if manifest is not None:
            try:
                m = json.loads(manifest)
                mid = m["id"]
            except (ValueError, KeyError):
                continue
            units = {}
            for suffix in ("", "-gateway", "-xray", "-singbox", "-ss", "-meter"):
                units[f"vpnshare-{mid}{suffix}"] = unit_state(f"vpnshare-{mid}{suffix}")
            out.append(
                {
                    "kind": "new",
                    "id": mid,
                    "manifest": m,
                    "meter": read_json(os.path.join(d, "meter-status.json")),
                    "usage": read_json(os.path.join(d, "usage.json")),
                    "last_access": read_json(os.path.join(d, "last_access.json")),
                    "units": units,
                }
            )
        else:
            out.append(
                {
                    "kind": "legacy",
                    "id": name,
                    "unit": read(f"/etc/systemd/system/vpnshare-{name}.service"),
                    "snapshot": read(os.path.join(d, "sub", name)),
                    "active": unit_state(f"vpnshare-{name}") == "active",
                }
            )
    print(json.dumps(out, ensure_ascii=False))


if __name__ == "__main__":
    main()
