#!/usr/bin/env python3
"""A local git "smart HTTP" server for tests, built on git's own `git http-backend`
(the same program real git hosting uses).

Usage: git-http-server.py <root folder of bare repositories>
Prints the port it listens on (127.0.0.1 only), then serves until killed.

Sign-in: pushing anywhere, and reading anything under /private/, needs HTTP Basic
auth as tester / s3cret. Everything else can be read anonymously, like a public
GitHub repository.

It's also a Git LFS server (the batch API, "basic" transfers), as GitHub is:
  POST <repo>/info/lfs/objects/batch   uploads need the same sign-in as pushing
  GET/PUT /lfs-store/<oid>             contents (PUT needs the token the batch reply gave)
  POST /lfs-verify                     confirms an upload arrived
Uploaded contents are checked against their SHA-256 before they're kept.
"""
import hashlib
import json
import base64
import http.server
import os
import re
import subprocess
import sys

ROOT = os.path.abspath(sys.argv[1])
GOOD = "Basic " + base64.b64encode(b"tester:s3cret").decode()


LFS = os.path.join(ROOT, ".lfs-store")
TOKEN = "RemoteAuth upload-token-123"
os.makedirs(LFS, exist_ok=True)


def store_path(oid):
    """Where an LFS object's contents live, or None if `oid` isn't an LFS id (64 hex
    digits). Nothing else ever reaches the file system."""
    if not isinstance(oid, str) or not re.fullmatch(r"[0-9a-f]{64}", oid):
        return None
    path = os.path.normpath(os.path.join(LFS, oid))
    if not path.startswith(LFS + os.sep):  # (the check CodeQL recognizes)
        return None
    return path


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path.startswith("/lfs-store/"):
            return self.lfs_get()
        self.run_backend()

    def do_POST(self):
        if self.path.endswith("/info/lfs/objects/batch"):
            return self.lfs_batch()
        if self.path == "/lfs-verify":
            return self.lfs_verify()
        self.run_backend()

    def do_PUT(self):
        if self.path.startswith("/lfs-store/"):
            return self.lfs_put()
        self.reply(405, b"")

    def reply(self, status, body, ctype="application/vnd.git-lfs+json", extra=()):
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        for k, v in extra:
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def body(self):
        return self.rfile.read(int(self.headers.get("Content-Length") or 0))

    def lfs_batch(self):
        req = json.loads(self.body())
        op = req.get("operation")
        if (op == "upload" or self.path.startswith("/private/")) and self.headers.get("Authorization") != GOOD:
            return self.reply(401, b'{"message":"sign in"}', extra=[("WWW-Authenticate", 'Basic realm="test"')])
        base = "http://127.0.0.1:%d" % self.server.server_address[1]
        out = []
        for o in req.get("objects", []):
            oid, size = o["oid"], o["size"]
            path = store_path(oid)
            if path is None:
                out.append({"oid": str(oid), "size": size, "error": {"code": 422, "message": "not an LFS id"}})
                continue
            have = os.path.exists(path)
            item = {"oid": oid, "size": size, "authenticated": True}
            if op == "download":
                if have:
                    item["actions"] = {"download": {"href": base + "/lfs-store/" + oid}}
                else:
                    item["error"] = {"code": 404, "message": "Object does not exist"}
            elif not have:
                item["actions"] = {
                    "upload": {"href": base + "/lfs-store/" + oid, "header": {"Authorization": TOKEN}},
                    "verify": {"href": base + "/lfs-verify", "header": {"Authorization": TOKEN}},
                }
            out.append(item)
        self.reply(200, json.dumps({"transfer": "basic", "objects": out}).encode())

    def lfs_get(self):
        path = store_path(self.path.rsplit("/", 1)[-1])
        if path is None or not os.path.exists(path):
            return self.reply(404, b"")
        with open(path, "rb") as f:
            self.reply(200, f.read(), "application/octet-stream")

    def lfs_put(self):
        if self.headers.get("Authorization") != TOKEN:
            return self.reply(401, b"")
        oid = self.path.rsplit("/", 1)[-1]
        path = store_path(oid)
        data = self.body()
        if path is None or hashlib.sha256(data).hexdigest() != oid:
            return self.reply(422, b'{"message":"contents do not match the oid"}')
        with open(path, "wb") as f:
            f.write(data)
        self.reply(200, b"")

    def lfs_verify(self):
        if self.headers.get("Authorization") != TOKEN:
            return self.reply(401, b"")
        req = json.loads(self.body())
        path = store_path(req.get("oid"))
        ok = path is not None and os.path.exists(path) and os.path.getsize(path) == req.get("size")
        self.reply(200 if ok else 404, b"")

    def run_backend(self):
        path, _, query = self.path.partition("?")
        pushing = "git-receive-pack" in self.path
        user = None
        if pushing or path.startswith("/private/"):
            if self.headers.get("Authorization") != GOOD:
                self.send_response(401)
                self.send_header("WWW-Authenticate", 'Basic realm="test"')
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            user = "tester"
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length)
        env = dict(
            os.environ,
            GIT_PROJECT_ROOT=ROOT,
            GIT_HTTP_EXPORT_ALL="1",
            PATH_INFO=path,
            QUERY_STRING=query,
            REQUEST_METHOD=self.command,
            CONTENT_TYPE=self.headers.get("Content-Type", ""),
            CONTENT_LENGTH=str(length),
            REMOTE_ADDR="127.0.0.1",
        )
        if user:
            env["REMOTE_USER"] = user  # http-backend only allows pushes from signed-in users
        out = subprocess.run(["git", "http-backend"], input=body, env=env, capture_output=True).stdout
        sep = b"\r\n\r\n" if b"\r\n\r\n" in out else b"\n\n"
        head, _, payload = out.partition(sep)
        status, headers = 200, []
        for line in head.decode("latin-1").splitlines():
            key, _, value = line.partition(":")
            if key.lower() == "status":
                status = int(value.strip().split()[0])
            elif key:
                headers.append((key, value.strip()))
        self.send_response(status)
        for key, value in headers:
            self.send_header(key, value)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *args):
        pass  # quiet


server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
print(server.server_address[1], flush=True)
server.serve_forever()
