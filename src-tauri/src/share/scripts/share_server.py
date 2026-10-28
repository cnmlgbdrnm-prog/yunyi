#!/usr/bin/env python3
"""vpn-ctl 分享订阅服务：TLS 静态服务，只回两条路由，禁目录列举。"""
import argparse, http.server, json, os, socket, socketserver, ssl, threading, time

# 订阅内容里主选择组的名字（与 config/mod.rs 的 AUTO_GROUP、singbox.rs 的
# SELECTOR_TAG 一致）。不限额度时响应头只能表达 `total=0`，FlClash 会把整块
# 订阅信息隐藏，因此再把「已用 X / ∞」写进组名兜底；这里按字节整段替换，
# 组定义、成员与规则引用会同步改名。
GROUP_TAG = "自动选择"


def _fmt_bytes(n):
    """十进制单位，与应用内配额展示一致（1 GB = 1e9 字节）。"""
    for unit, scale in (("TB", 1e12), ("GB", 1e9), ("MB", 1e6), ("KB", 1e3)):
        if n >= scale:
            text = f"{n / scale:.1f}".rstrip("0").rstrip(".")
            return text + unit
    return f"{int(n)}B"


def load_token(dirpath):
    p = os.path.join(dirpath, "share.json")
    if os.path.isfile(p):
        with open(p, "r", encoding="utf-8") as f:
            return json.load(f)["token"]
    return os.path.basename(os.path.normpath(dirpath))


class Handler(http.server.SimpleHTTPRequestHandler):
    # WebSocket data must not be swallowed by the HTTP header reader.
    rbufsize = 0
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

    def _usage(self):
        """返回 (上行, 下行, 总额度, 到期)，口径与停用判定一致。

        meter 的公网实测（入站 + 出站）加历史折算；历史无法拆分方向，按既有
        迁移规则并入下行。旧分享没有 meter 时退回网关的方向计数与旧合计配额。
        """
        try:
            with open(os.path.join(self.dirpath, "share.json"), "r", encoding="utf-8") as f:
                manifest = json.load(f)
        except (OSError, ValueError):
            return None
        total = int(manifest.get("quota_total_bytes") or 0)
        expire = int(manifest.get("expires_at_unix") or 0)
        meter = None
        meter_path = os.path.join(self.dirpath, "meter-status.json")
        if os.path.isfile(meter_path):
            try:
                with open(meter_path, "r", encoding="utf-8") as f:
                    meter = json.load(f)
            except (OSError, ValueError):
                meter = None
        if meter is not None and meter.get("quota_total_bytes") is not None:
            total = int(meter.get("quota_total_bytes") or 0)
            up = int(meter.get("public_rx_bytes") or 0)
            down = int(meter.get("public_tx_bytes") or 0) + int(
                meter.get("history_charge_bytes") or 0
            )
        else:
            usage = {}
            usage_path = os.path.join(self.dirpath, "usage.json")
            if os.path.isfile(usage_path):
                try:
                    with open(usage_path, "r", encoding="utf-8") as f:
                        usage = json.load(f)
                except (OSError, ValueError):
                    usage = {}
            up = int(usage.get("used_up_bytes") or 0)
            down = int(usage.get("used_down_bytes") or 0)
            if total <= 0:
                total = int(manifest.get("quota_bytes") or 0)
        return up, down, total, expire

    def _userinfo(self):
        """订阅响应头 `subscription-userinfo`：Clash/mihomo 系客户端据此显示
        已用/剩余流量与到期时间。

        不限额度的分享发 `total=0`：支持无限语义的客户端据此显示「已用 / ∞」，
        不认识 0 的客户端由 _decorate() 把同样格式的文本写进组名兜底。
        """
        info = self._usage()
        if info is None:
            return None
        up, down, total, expire = info
        return f"upload={up}; download={down}; total={total}; expire={expire}"

    def _decorate(self, data):
        """不限额度时把「已用 X / ∞」写进主选择组名。

        FlClash 在 subscriptionInfo.total == 0 时整块隐藏订阅信息，CMFA 只在
        下载量非零时显示一个带尾部斜杠的已用量；组名是各客户端都会渲染的
        通用位置。只在 total=0 时启用，限额分享继续依赖响应头，组名保持不变。
        """
        info = self._usage()
        if info is None or info[2] > 0:
            return data
        old = GROUP_TAG.encode("utf-8")
        if old not in data:
            return data
        label = f"{GROUP_TAG} · 已用{_fmt_bytes(info[0] + info[1])} / ∞"
        return data.replace(old, label.encode("utf-8"))

    def _serve(self, path, content_type, fmt, head=False):
        if not os.path.isfile(path):
            self.send_error(404)
            return
        with open(path, "rb") as f:
            data = self._decorate(f.read())
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", "no-store")
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("Referrer-Policy", "no-referrer")
        userinfo = self._userinfo()
        if userinfo:
            self.send_header("subscription-userinfo", userinfo)
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
        if path.startswith("/cdn-ws/"):
            if head:
                self.send_error(405)
            else:
                self._websocket(path)
            return
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

    def _websocket(self, path):
        """TLS terminates here; all traffic still crosses the quota gateway."""
        try:
            with open(os.path.join(self.dirpath, "share.json"), encoding="utf-8") as f:
                manifest = json.load(f)
            if not manifest.get("cdn"):
                self.send_error(404)
                return
            node = next((n for n in manifest["nodes"]
                         if path == f"/cdn-ws/{n['public_port']}"
                         and n["protocol"] in ("vmess", "vless")
                         and n["transport"] == "tcp"), None)
            if node is None:
                self.send_error(404)
                return
            if (self.headers.get("Upgrade", "").lower() != "websocket"
                    or "upgrade" not in [v.strip().lower() for v in self.headers.get("Connection", "").split(",")]
                    or self.headers.get("Sec-WebSocket-Version") != "13"):
                self.send_error(400)
                return
            with open(os.path.join(self.dirpath, "xray.json"), encoding="utf-8") as f:
                config = json.load(f)
            inbound = next(i for i in config["inbounds"] if i["port"] == node["internal_port"])
            stream = inbound["streamSettings"]
            if stream.get("network") != "ws" or stream.get("security", "none") != "none":
                self.send_error(502)
                return
            target_path = stream["wsSettings"]["path"]
            key = self.headers.get("Sec-WebSocket-Key", "")
            if not target_path.startswith("/") or any(c.isspace() for c in target_path) or not key or any(c.isspace() for c in key):
                self.send_error(400)
                return
            # Never connect to internal_port: that would bypass quotas and expiry.
            upstream = socket.create_connection(("127.0.0.1", node["public_port"]), timeout=10)
        except (OSError, ValueError, KeyError, StopIteration):
            self.send_error(502)
            return
        self.close_connection = True
        try:
            request = (f"GET {target_path} HTTP/1.1\r\nHost: localhost\r\n"
                       "Connection: Upgrade\r\nUpgrade: websocket\r\n"
                       f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n")
            upstream.sendall(request.encode("ascii"))
            response = bytearray()
            while not response.endswith(b"\r\n\r\n") and len(response) < 65536:
                data = upstream.recv(1)
                if not data:
                    break
                response.extend(data)
            if not response.endswith(b"\r\n\r\n") or response.split(b"\r\n", 1)[0].split()[1:2] != [b"101"]:
                self.send_error(502)
                return
            self.connection.sendall(response)
            self.connection.settimeout(300)
            upstream.settimeout(300)

            def pipe(source, target):
                try:
                    while data := source.recv(65536):
                        target.sendall(data)
                except OSError:
                    pass
                finally:
                    for conn in (source, target):
                        try:
                            conn.shutdown(socket.SHUT_RDWR)
                        except OSError:
                            pass

            worker = threading.Thread(target=pipe, args=(self.connection, upstream), daemon=True)
            worker.start()
            pipe(upstream, self.connection)
            worker.join(timeout=1)
        except (OSError, UnicodeError):
            pass
        finally:
            upstream.close()


class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
    max_connections = 64
    handshake_timeout = 10
    request_timeout = 15

    def __init__(self, addr, handler, dirpath, ssl_context=None):
        self.dirpath = dirpath
        self.ssl_context = ssl_context
        self._sem = threading.BoundedSemaphore(self.max_connections)
        super().__init__(addr, handler)

    def process_request(self, request, client_address):
        # A full worker pool must not block the accept loop either.
        if not self._sem.acquire(blocking=False):
            self.shutdown_request(request)
            return
        try:
            super().process_request(request, client_address)
        except Exception:
            self._sem.release()
            raise

    def process_request_thread(self, request, client_address):
        try:
            if self.ssl_context is not None:
                # Handshake only in the worker, never on the listening socket:
                # an idle TCP peer otherwise stalls every subscription request.
                request.settimeout(self.handshake_timeout)
                request = self.ssl_context.wrap_socket(
                    request, server_side=True, do_handshake_on_connect=False
                )
                request.do_handshake()
            request.settimeout(self.request_timeout)
            self.finish_request(request, client_address)
        except (TimeoutError, ssl.SSLError, ConnectionError):
            # Incomplete handshakes and disconnected clients are normal on a
            # public port; release their socket and capacity without log spam.
            pass
        except Exception:
            self.handle_error(request, client_address)
        finally:
            try:
                self.shutdown_request(request)
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
    ctx = None
    if args.cert and args.key:
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        ctx.load_cert_chain(args.cert, args.key)
    srv = Server((args.bind, args.port), Handler, Handler.dirpath, ssl_context=ctx)
    srv.serve_forever()


if __name__ == "__main__":
    main()
