#!/usr/bin/env python3
"""vpn-ctl 分享订阅服务：TLS 静态服务，只回两条路由，禁目录列举。"""
import argparse, http.server, json, os, socket, socketserver, ssl, threading, time


def load_token(dirpath):
    p = os.path.join(dirpath, "share.json")
    if os.path.isfile(p):
        with open(p, "r", encoding="utf-8") as f:
            return json.load(f)["token"]
    return os.path.basename(os.path.normpath(dirpath))


class Handler(http.server.SimpleHTTPRequestHandler):
    token = ""
    dirpath = ""
    failures = {}
    lock = threading.Lock()

    def log_message(self, *a):
        pass

    def list_directory(self, path):
        self.send_error(404)
        return None

    def _rate_ok(self):
        ip = self.client_address[0]
        now = time.time()
        with self.lock:
            count, started = self.failures.get(ip, (0, now))
            if now - started > 60:
                count, started = 0, now
            if count >= 30:
                return False
            self.failures[ip] = (count + 1, started)
            return True

    def _record(self, fmt):
        try:
            now = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
            tmp = os.path.join(self.dirpath, "last_access.json.tmp")
            with open(tmp, "w", encoding="utf-8") as f:
                json.dump(
                    {"at": now, "ip": self.client_address[0], "format": fmt},
                    f,
                    ensure_ascii=False,
                )
            os.replace(tmp, os.path.join(self.dirpath, "last_access.json"))
            log = os.path.join(self.dirpath, "access.log")
            with open(log, "a", encoding="utf-8") as f:
                f.write(f"{now} {self.client_address[0]} {fmt} 200\n")
            if os.path.getsize(log) > 1024 * 1024:
                os.truncate(log, 0)
        except OSError:
            pass

    def _serve(self, path, content_type, fmt, head=False):
        if not os.path.isfile(path):
            self.send_error(404)
            return
        data = open(path, "rb").read()
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", "no-store")
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("Referrer-Policy", "no-referrer")
        self.end_headers()
        if not head:
            self.wfile.write(data)
            self._record(fmt)

    def do_GET(self):
        self._route(False)

    def do_HEAD(self):
        self._route(True)

    def _route(self, head):
        path, _, query = self.path.partition("?")
        yaml = f"/sub/{self.token}"
        singbox = f"/singbox/{self.token}"
        if path == yaml:
            if "format=singbox" in query:
                self._serve(
                    os.path.join(self.dirpath, "sub", self.token + ".json"),
                    "application/json; charset=utf-8",
                    "singbox",
                    head,
                )
            else:
                self._serve(
                    os.path.join(self.dirpath, "sub", self.token),
                    "text/yaml; charset=utf-8",
                    "yaml",
                    head,
                )
        elif path == singbox:
            self._serve(
                os.path.join(self.dirpath, "sub", self.token + ".json"),
                "application/json; charset=utf-8",
                "singbox",
                head,
            )
        else:
            if not self._rate_ok():
                self.send_error(429)
            else:
                self.send_error(404)


class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True

    def __init__(self, addr, handler, dirpath):
        self.dirpath = dirpath
        self._sem = threading.BoundedSemaphore(64)
        super().__init__(addr, handler)

    def process_request(self, request, client_address):
        self._sem.acquire()
        try:
            super().process_request(request, client_address)
        except Exception:
            self._sem.release()
            raise

    def process_request_thread(self, request, client_address):
        try:
            super().process_request_thread(request, client_address)
        finally:
            self._sem.release()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("port", type=int)
    ap.add_argument("dir")
    ap.add_argument("--bind", default="0.0.0.0")
    ap.add_argument("--cert")
    ap.add_argument("--key")
    args = ap.parse_args()
    Handler.token = load_token(args.dir)
    Handler.dirpath = os.path.abspath(args.dir)
    if ":" in args.bind:
        Server.address_family = socket.AF_INET6
    srv = Server((args.bind, args.port), Handler, Handler.dirpath)
    if args.cert and args.key:
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        ctx.load_cert_chain(args.cert, args.key)
        srv.socket = ctx.wrap_socket(srv.socket, server_side=True)
    srv.serve_forever()


if __name__ == "__main__":
    main()
