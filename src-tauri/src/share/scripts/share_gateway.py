#!/usr/bin/env python3
"""vpn-ctl 分享入口网关：TCP/UDP 转发、字节计量、令牌桶限速、配额与到期。"""
import argparse, asyncio, json, os, signal, time


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
        self.blocked = None
        self.blocked_at = None
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
            self.blocked = u.get("blocked")
            self.blocked_at = u.get("blocked_at")
        except (OSError, ValueError, TypeError):
            self.used = 0

    def flush(self):
        data = {
            "used_bytes": self.used,
            "blocked": self.blocked,
            "blocked_at": self.blocked_at,
            "updated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        }
        tmp = self.usage_path + ".tmp"
        with open(tmp, "w", encoding="utf-8") as f:
            json.dump(data, f, ensure_ascii=False)
        os.replace(tmp, self.usage_path)

    def charge(self, n):
        if self.blocked:
            return False
        quota = int(self.cfg.get("quota_bytes", 0))
        if quota > 0 and self.used + n >= quota:
            self.used = quota
            self.block("quota")
            return False
        self.used += n
        return True

    def block(self, reason):
        if self.blocked:
            return
        self.blocked = reason
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

    async def pipe(self, reader, writer, bucket):
        try:
            while not self.blocked:
                # 1s 超时只为定期检查 blocked；超时不会丢已缓冲数据。
                try:
                    data = await asyncio.wait_for(reader.read(65536), timeout=1.0)
                except asyncio.TimeoutError:
                    continue
                if not data:
                    break
                if not self.charge(len(data)):
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
                self.pipe(reader, uw, self.up),
                self.pipe(ur, writer, self.down),
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
        if not self.gateway.charge(len(data)):
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
        if self.gateway.blocked or not self.gateway.charge(len(data)):
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
                lambda r, w, t=target: gw.handle_tcp(r, w, t), args.bind, public
            )
            gw.servers.append(srv)
        if "udp" in proto:
            proto_obj = UdpPublic(target, gw)
            tr, _ = await loop.create_datagram_endpoint(
                lambda p=proto_obj: p, local_addr=(args.bind, public)
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
