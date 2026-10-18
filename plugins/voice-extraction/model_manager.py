"""明示導入専用。通常のworker起動からネットワーク取得を呼ばない。"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import sys
import tempfile
import time
import urllib.request
import uuid
import zipfile

CATALOG = json.loads(Path(__file__).with_name("models.json").read_text(encoding="utf-8"))
PINNED_SHA256 = json.loads(Path(__file__).with_name("model-sha256.json").read_text(encoding="utf-8"))
for _model in CATALOG["models"]:
    for _file in _model["files"]:
        if _file["name"] in PINNED_SHA256:
            _file["sha256"] = PINNED_SHA256[_file["name"]]


def default_root():
    base = os.environ.get("LOCALAPPDATA")
    if not base:
        raise ValueError("--root または model_root で保存先を指定してください")
    return Path(base) / "VoiceMixer" / "models" / "voice-extraction"


def descriptor(method):
    return next(item for item in CATALOG["models"] if item["id"] == method)


def digest(path):
    h = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for data in iter(lambda: stream.read(1024 * 1024), b""):
            h.update(data)
    return h.hexdigest()


def emit(state, **values):
    print(json.dumps({"state": state, **values}, ensure_ascii=False), flush=True)


def validate_file(path, entry):
    if path.stat().st_size != entry["bytes"]:
        raise ValueError(f"容量不一致: {entry['name']}")
    actual = digest(path)
    expected = entry.get("sha256") or entry["sha256_prefix"]
    if not actual.startswith(expected):
        raise ValueError(f"SHA256不一致: {entry['name']}")
    return actual


def check_cancel(cancel_file):
    if cancel_file and Path(cancel_file).exists():
        raise InterruptedError("導入を取り消しました")


def download(entry, output, cancel_file=None):
    check_cancel(cancel_file)
    request = urllib.request.Request(entry["url"], headers={"User-Agent": "VoiceMixer/0.4.6"})
    count = 0
    with urllib.request.urlopen(request, timeout=30) as response, output.open("xb") as stream:
        for block in iter(lambda: response.read(256 * 1024), b""):
            check_cancel(cancel_file)
            count += len(block)
            if count > entry["bytes"]:
                raise ValueError("公開容量を超える応答を拒否しました")
            stream.write(block)
            emit("downloading", file=entry["name"], received_bytes=count, total_bytes=entry["bytes"])
        stream.flush()
        os.fsync(stream.fileno())
    emit("verifying", file=entry["name"])
    return validate_file(output, entry)


def extract_checked(archive, target, maximum_bytes):
    with zipfile.ZipFile(archive) as zip_file:
        if sum(item.file_size for item in zip_file.infolist()) > maximum_bytes:
            raise ValueError("展開容量が公開上限を超えています")
        for item in zip_file.infolist():
            relative = Path(item.filename)
            destination = (target / relative).resolve()
            if relative.is_absolute() or not destination.is_relative_to(target.resolve()) or ((item.external_attr >> 16) & 0o170000) == 0o120000:
                raise ValueError("安全でないZIPパスです")
        zip_file.extractall(target)


def installed(root, method, verify=False):
    folder = root / method
    marker = folder / "installed.json"
    if not marker.is_file():
        if folder.exists():
            raise ValueError("モデル導入が不完全です。削除操作で退避してから再導入してください")
        return None
    data = json.loads(marker.read_text(encoding="utf-8"))
    if data.get("schema_version") != 1 or data.get("method") != method:
        raise ValueError("モデル台帳が不正です")
    for entry in data["files"]:
        path = (folder / entry["path"]).resolve()
        if not path.is_relative_to(folder.resolve()) or not path.is_file():
            raise ValueError("導入済みモデルファイルがありません")
        if verify and digest(path) != entry["sha256"]:
            raise ValueError("導入済みモデルが変更されています")
    return data


def install(root, method, cancel_file=None, fetch=download):
    model = descriptor(method)
    root.mkdir(parents=True, exist_ok=True)
    destination = root / method
    if destination.exists():
        installed(root, method, verify=True)
        return {"state": "installed", "method": method, "already_installed": True}
    check_cancel(cancel_file)
    required = model["download_bytes"] + model["extracted_model_bytes"] + 16 * 1024 * 1024
    if shutil.disk_usage(root).free < required:
        raise OSError("モデル取得と展開に必要な空き容量がありません")
    staging = Path(tempfile.mkdtemp(prefix=f".install-{method}-", dir=root))
    try:
        hashes = []
        for entry in model["files"]:
            check_cancel(cancel_file)
            path = staging / entry["name"]
            actual = fetch(entry, path, cancel_file)
            hashes.append({"name": entry["name"], "sha256": actual, "published_sha256": entry.get("sha256"), "published_sha256_prefix": entry.get("sha256_prefix")})
            if entry.get("archive"):
                extract_checked(path, staging, model["extracted_model_bytes"])
                path.unlink()
        if method == "htdemucs_ft":
            (staging / "htdemucs_ft.yaml").write_text("models: ['f7e0c4bc', 'd12395a8', '92cfc3b6', '04573f0d']\nweights: [[1,0,0,0],[0,1,0,0],[0,0,1,0],[0,0,0,1]]\n", encoding="utf-8")
        data = {"schema_version": 1, "method": method, "installed_at": time.time(), "download_bytes": model["download_bytes"], "source": model["source"], "license": model["license"], "downloads": hashes,
                "files": [{"path": str(path.relative_to(staging)), "bytes": path.stat().st_size, "sha256": digest(path)} for path in staging.rglob("*") if path.is_file()]}
        data["installed_bytes"] = sum(file["bytes"] for file in data["files"])
        (staging / "installed.json").write_text(json.dumps(data, ensure_ascii=False, indent=2), encoding="utf-8")
        check_cancel(cancel_file)
        staging.rename(destination)
        return {"state": "installed", "method": method, "path": str(destination), **data}
    finally:
        if staging.exists():
            shutil.rmtree(staging)


def main():
    sys.stdout.reconfigure(encoding="utf-8")
    parser = argparse.ArgumentParser(description="人声抽出モデルの一覧・明示導入・復元可能な削除")
    parser.add_argument("--root", type=Path)
    parser.add_argument("action", choices=["list", "install", "remove"])
    parser.add_argument("method", nargs="?", choices=[m["id"] for m in CATALOG["models"]])
    parser.add_argument("--accept-download", action="store_true")
    parser.add_argument("--confirm-remove", action="store_true")
    parser.add_argument("--cancel-file", type=Path)
    args = parser.parse_args()
    root = (args.root or default_root()).resolve()
    try:
        if args.action == "list":
            rows = []
            for model in CATALOG["models"]:
                try:
                    local = installed(root, model["id"])
                    rows.append({**model, "state": "installed" if local else "not_installed", "path": str(root / model["id"]), "installation": local})
                except Exception as error:
                    rows.append({**model, "state": "failed", "error": str(error)})
            print(json.dumps({**CATALOG, "models": rows}, ensure_ascii=False))
        elif args.action == "install":
            if not args.method or not args.accept_download:
                raise ValueError("対象methodと--accept-downloadが必要です。先にlistで提供元・容量を確認してください")
            emit(**install(root, args.method, args.cancel_file))
        else:
            if not args.method or not args.confirm_remove:
                raise ValueError("対象methodと--confirm-removeが必要です。使用中のノードは先に停止してください")
            descriptor(args.method)
            source = root / args.method
            if not source.is_dir():
                raise ValueError("導入済みモデルがありません")
            trash = root / ".trash"
            trash.mkdir(exist_ok=True)
            target = trash / f"{args.method}-{uuid.uuid4()}"
            source.rename(target)
            emit("removed", method=args.method, recovery_path=str(target))
        return 0
    except (Exception, KeyboardInterrupt) as error:
        emit("cancelled" if isinstance(error, (InterruptedError, KeyboardInterrupt)) else "failed", message=str(error))
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
