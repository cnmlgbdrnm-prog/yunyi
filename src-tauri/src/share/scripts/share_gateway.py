#!/usr/bin/env python3
"""vpn-ctl 分享入口网关：TCP/UDP 转发、字节计量、令牌桶限速、配额与到期。"""
import argparse, asyncio, json, os, signal, socket, time


class Bucket:
    def __init__(self, rate_bps):
        self.rate = max(0, int(rate_bps))
        # 桶容量至少 64KB：TCP 单次 read 就是 64KB，容量小于它会让 take 永远凑不齐。
        self.cap = float(max(self.rate, 65536))
        self.tokens = self.cap
        self.ts = time.monotonic()

    def _refill(self):
        now = time.monotonic()
        if self.rate:
            self.tokens = min(self.cap, self.tokens + (now - self.ts) * self.rate)
        self.ts = now

    async def take(self, n):
        if self.rate <= 0 or n <= 0:
            return
        while True:
            self._refill()
            if self.tokens >= n:
                self.tokens -= n
                return
            await asyncio.sleep((n - self.tokens) / self.rate)

    def try_take(self, n):
        if self.rate <= 0:
            return True
        self._refill()
        if self.tokens >= n:
            self.tokens -= n
            return True
        return False


class Gateway:
    def __init__(self, cfg, usage_path):
        self.cfg = cfg
        self.usage_path = usage_path
        self.used = 0
        self.used_up = 0
        self.used_down = 0
        # 新口径：上下行各自独立配额。两个字段都缺失时回落到旧的合计配额，
        # 保证尚未编辑过的旧分享在共享脚本升级后仍按原方式运行。
        self.quota_up = cfg.get("quota_up_bytes")
        self.quota_down = cfg.get("quota_down_bytes")
        self.split_quota = self.quota_up is not None or self.quota_down is not None
        self.quota_up = int(self.quota_up or 0)
        self.quota_down = int(self.quota_down or 0)
        self.blocked = None
        self.blocked_at = None
        self.blocked_direction = None
        self.servers = []
        self.transports = []
        self.conns = set()  # 已建立的 TCP 客户端连接；停用时主动断开
        self.up = Bucket(cfg.get("rate_up_mbps", 0) * 125000)
        self.down = Bucket(cfg.get("rate_down_mbps", 0) * 125000)
        self.load()

    def load(self):
        try:
            with open(self.usage_path, "r", encoding="utf-8") as f:
                u = json.load(f)
            self.used = int(u.get("used_bytes", 0))
            has_split_usage = "used_up_bytes" in u or "used_down_bytes" in u
            if has_split_usage:
                self.used_up = int(u.get("used_up_bytes", 0) or 0)
                self.used_down = int(u.get("used_down_bytes", 0) or 0)
                if self.split_quota:
                    self.used = self.used_up + self.used_down
            elif self.split_quota:
                # 旧 usage 只有合计，没有方向拆分：全部计入下行。历史流量以下载
                # 为主，这样升级后不会凭空多出一份额度。
                self.used_up = 0
                self.used_down = self.used
            self.blocked = u.get("blocked")
            self.blocked_at = u.get("blocked_at")
            self.blocked_direction = u.get("blocked_direction")
        except (OSError, ValueError, TypeError):
            self.used = 0
            self.used_up = 0
            self.used_down = 0
            self.blocked = None
            self.blocked_at = None
            self.blocked_direction = None

    def flush(self):
        data = {
            "used_bytes": self.used,
            "used_up_bytes": self.used_up,
            "used_down_bytes": self.used_down,
            "blocked": self.blocked,
            "blocked_at": self.blocked_at,
            "blocked_direction": self.blocked_direction,
            "updated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        }
        tmp = self.usage_path + ".tmp"
        with open(tmp, "w", encoding="utf-8") as f:
            json.dump(data, f, ensure_ascii=False)
        os.replace(tmp, self.usage_path)

    def charge(self, n, direction):
        """按方向计费；direction 只会是 "up" 或 "down"。"""
        if self.blocked:
            return False
        if self.split_quota:
            quota = self.quota_up if direction == "up" else self.quota_down
            used = self.used_up if direction == "up" else self.used_down
            if quota > 0 and used + n >= quota:
                if direction == "up":
                    self.used_up = quota
                else:
                    self.used_down = quota
                self.used = self.used_up + self.used_down
                self.block("quota", direction)
                return False
            if direction == "up":
                self.used_up += n
            else:
                self.used_down += n
            self.used = self.used_up + self.used_down
            return True
        # 旧版合计配额：保留原有行为，方向计数只用于展示。
        quota = int(self.cfg.get("quota_bytes", 0))
        if quota > 0 and self.used + n >= quota:
            self.used = quota
            self.block("quota")
            return False
        self.used += n
        if direction == "up":
            self.used_up += n
        else:
            self.used_down += n
        return True

    def block(self, reason, direction=None):
        if self.blocked:
            return
        self.blocked = reason
        self.blocked_direction = direction
        self.blocked_at = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        self.flush()
        for s in self.servers:
            s.close()
        self.servers.clear()
        for t in self.transports:
            try:
                t.close()
            except Exception:
                pass
        self.transports.clear()
        for w in list(self.conns):
            try:
                w.close()
            except Exception:
                pass
        self.conns.clear()

    def expired(self):
        exp = int(self.cfg.get("expires_at_unix", 0))
        return exp > 0 and time.time() >= exp

    async def pipe(self, reader, writer, bucket, direction):
        try:
            while not self.blocked:
                # 1s 超时只为定期检查 blocked；超时不会丢已缓冲数据。
                try:
                    data = await asyncio.wait_for(reader.read(65536), timeout=1.0)
                except asyncio.TimeoutError:
                    continue
                if not data:
                    break
                if not self.charge(len(data), direction):
                    break
                await bucket.take(len(data))
                if self.blocked:
                    break
                writer.write(data)
                await writer.drain()
        except (ConnectionError, OSError):
            pass
        finally:
            try:
                writer.close()
            except Exception:
                pass

    async def handle_tcp(self, reader, writer, target):
        if self.blocked:
            writer.close()
            return
        self.conns.add(writer)
        try:
            ur, uw = await asyncio.open_connection("127.0.0.1", target)
        except OSError:
            self.conns.discard(writer)
            writer.close()
            return
        try:
            await asyncio.gather(
                self.pipe(reader, uw, self.up, "up"),
                self.pipe(ur, writer, self.down, "down"),
                return_exceptions=True,
            )
        finally:
            self.conns.discard(writer)
            try:
                writer.close()
            except Exception:
                pass


class UdpUpstream(asyncio.DatagramProtocol):
    def __init__(self, public, client, gateway):
        self.public = public
        self.client = client
        self.gateway = gateway

    def datagram_received(self, data, addr):
        # 目标返回客户端：下行。
        if not self.gateway.charge(len(data), "down"):
            return
        if not self.gateway.down.try_take(len(data)):
            return
        self.public.sendto(data, self.client)


class UdpPublic(asyncio.DatagramProtocol):
    def __init__(self, target, gateway):
        self.target = target
        self.gateway = gateway
        self.peers = {}
        self.lock = asyncio.Lock()

    def connection_made(self, transport):
        self.transport = transport

    async def _upstream(self, addr):
        async with self.lock:
            entry = self.peers.get(addr)
            if entry:
                entry[1] = time.monotonic()
                return entry[0]
            loop = asyncio.get_running_loop()
            tr, _ = await loop.create_datagram_endpoint(
                lambda: UdpUpstream(self.transport, addr, self.gateway),
                remote_addr=("127.0.0.1", self.target),
            )
            self.peers[addr] = [tr, time.monotonic()]
            return tr

    def datagram_received(self, data, addr):
        # 客户端发往目标：上行。
        if self.gateway.blocked or not self.gateway.charge(len(data), "up"):
            return
        if not self.gateway.up.try_take(len(data)):
            return
        asyncio.create_task(self._send(data, addr))

    async def _send(self, data, addr):
        tr = await self._upstream(addr)
        tr.sendto(data)

    async def sweep(self):
        while True:
            await asyncio.sleep(30)
            now = time.monotonic()
            for addr, entry in list(self.peers.items()):
                if now - entry[1] > 60:
                    entry[0].close()
                    del self.peers[addr]


async def flusher(gw):
    while True:
        await asyncio.sleep(5)
        gw.flush()


def bind_socket(kind, host, port):
    """创建分享入口套接字。

    `host = "::"` 时必须显式关闭 IPV6_V6ONLY：Python 的 asyncio 在 IPv6 通配
    地址上默认建 IPv6-only 套接字，IPv4 客户端会看到「端口关了」。分享页生成的
    节点可能用 IPv4 或 IPv6，两种客户端都要能连进来。
    """
    family = socket.AF_INET6 if ":" in host else socket.AF_INET
    sock = socket.socket(family, kind)
    try:
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        if family == socket.AF_INET6:
            try:
                sock.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 0)
            except (AttributeError, OSError):
                # 平台不支持双栈时保持 IPv6-only，至少 IPv6 客户端还能用。
                pass
        addr = (host, port, 0, 0) if family == socket.AF_INET6 else (host, port)
        sock.bind(addr)
        sock.setblocking(False)
        return sock
    except BaseException:
        sock.close()
        raise


async def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--config", required=True)
    ap.add_argument("--usage", required=True)
    ap.add_argument("--bind", default="0.0.0.0")
    args = ap.parse_args()
    with open(args.config, "r", encoding="utf-8") as f:
        cfg = json.load(f)
    gw = Gateway(cfg, args.usage)
    gw.flush()
    loop = asyncio.get_running_loop()
    stopping = asyncio.Event()

    async def stop():
        gw.flush()
        for s in gw.servers:
            s.close()
        for t in gw.transports:
            t.close()
        stopping.set()

    # 先注册信号再进任何等待：已到期的分享启动后不监听，也必须能被 systemctl
    # stop 优雅收尾。loop.stop() 会让 asyncio.run 以「Event loop stopped before
    # Future completed」退出 1，被 systemd 记为失败，所以这里只置事件、让 main 返回。
    try:
        for sig in (signal.SIGTERM, signal.SIGINT):
            loop.add_signal_handler(sig, lambda: asyncio.create_task(stop()))
    except NotImplementedError:
        pass  # Windows 本地测试；生产在 Linux 上走 add_signal_handler

    if gw.expired():
        gw.block("expired")
    if gw.blocked:
        await stopping.wait()
        return

    for entry in cfg.get("ports", []):
        public = int(entry["public"])
        target = int(entry["target"])
        proto = entry.get("proto", "tcp")
        if "tcp" in proto:
            srv = await asyncio.start_server(
                lambda r, w, t=target: gw.handle_tcp(r, w, t),
                sock=bind_socket(socket.SOCK_STREAM, args.bind, public),
            )
            gw.servers.append(srv)
        if "udp" in proto:
            proto_obj = UdpPublic(target, gw)
            tr, _ = await loop.create_datagram_endpoint(
                lambda p=proto_obj: p,
                sock=bind_socket(socket.SOCK_DGRAM, args.bind, public),
            )
            gw.transports.append(tr)
            asyncio.create_task(proto_obj.sweep())

    asyncio.create_task(flusher(gw))
    while not stopping.is_set():
        if gw.expired():
            gw.block("expired")
        try:
            await asyncio.wait_for(stopping.wait(), timeout=1.0)
        except asyncio.TimeoutError:
            pass


if __name__ == "__main__":
    asyncio.run(main())
