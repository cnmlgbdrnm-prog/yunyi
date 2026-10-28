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
        self.history_path = usage_path + '.history'
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
        self.upstreams = set()
        self.tcp_tasks = set()
        self.udp_protocols = []
        self.stopping = False
        # Per-share bounds shared by all listeners; old configs need no migration.
        self.max_tcp_connections = max(1, int(cfg.get("max_tcp_connections", 256)))
        self.max_udp_peers = max(1, int(cfg.get("max_udp_peers", 256)))
        self.max_udp_pending_packets = max(1, int(cfg.get("max_udp_pending_packets", 1024)))
        self.max_udp_pending_bytes = max(65536, int(cfg.get("max_udp_pending_bytes", 4 * 1024 * 1024)))
        self.max_udp_buffer_bytes = max(65536, int(cfg.get("max_udp_buffer_bytes", 128 * 1024)))
        self.tcp_idle_timeout = max(1, float(cfg.get("tcp_idle_timeout", 300)))
        self.connect_timeout = max(0.1, float(cfg.get("connect_timeout", 10)))
        self.udp_peer_count = 0
        self.udp_pending_packets = 0
        self.udp_pending_bytes = 0
        self.rejected_tcp = 0
        self.dropped_rate = {"up": 0, "down": 0}
        self.dropped_resource = {"up": 0, "down": 0}
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
            if os.path.exists(self.usage_path) and not os.path.exists(self.history_path):
                raise  # Never turn damaged historical usage into free quota.
            self.used = 0
            self.used_up = 0
            self.used_down = 0
            self.blocked = None
            self.blocked_at = None
            self.blocked_direction = None
        if os.path.exists(self.history_path):
            with open(self.history_path, 'r', encoding='utf-8') as f:
                previous = json.load(f)
            self.used_up = max(self.used_up, int(previous['used_up_bytes']))
            self.used_down = max(self.used_down, int(previous['used_down_bytes']))
            self.used = max(self.used, int(previous['used_bytes']))
            if self.split_quota:
                self.used = self.used_up + self.used_down

    def flush(self):
        data = {
            "used_bytes": self.used,
            "used_up_bytes": self.used_up,
            "used_down_bytes": self.used_down,
            "blocked": self.blocked,
            "blocked_at": self.blocked_at,
            "blocked_direction": self.blocked_direction,
            "updated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            # Diagnostics since process start, not additional billable bytes.
            "runtime": {
                "tcp_connections": len(self.conns),
                "udp_peers": self.udp_peer_count,
                "udp_pending_packets": self.udp_pending_packets,
                "udp_pending_bytes": self.udp_pending_bytes,
                "rejected_tcp": self.rejected_tcp,
                "dropped_rate_bytes": self.dropped_rate.copy(),
                "dropped_resource_bytes": self.dropped_resource.copy(),
            },
        }
        # A separate high-water record survives legacy clients writing zero to
        # usage.json. Only deletion of the whole share removes its history.
        history_tmp = self.history_path + '.tmp'
        with open(history_tmp, 'w', encoding='utf-8') as f:
            json.dump({k:data[k] for k in ('used_bytes','used_up_bytes','used_down_bytes')},f)
        os.replace(history_tmp, self.history_path)
        tmp = self.usage_path + ".tmp"
        with open(tmp, "w", encoding="utf-8") as f:
            json.dump(data, f, ensure_ascii=False)
        os.replace(tmp, self.usage_path)

    def charge(self, n, direction):
        """按方向计费；direction 只会是 "up" 或 "down"。"""
        if self.blocked or self.stopping:
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
        self.close_connections()

    def close_connections(self):
        for s in self.servers:
            s.close()
        self.servers.clear()
        for t in self.transports:
            try:
                t.close()
            except Exception:
                pass
        self.transports.clear()
        for protocol in self.udp_protocols:
            protocol.close()
        for w in list(self.conns | self.upstreams):
            try:
                w.close()
            except Exception:
                pass

    def send_datagram(self, transport, data, addr, direction):
        # asyncio UDP transports otherwise buffer without a size limit when
        # their socket becomes unwritable.
        if (transport.is_closing()
                or transport.get_write_buffer_size() + len(data) > self.max_udp_buffer_bytes):
            self.dropped_resource[direction] += len(data)
            return
        try:
            transport.sendto(data, addr)
        except OSError:
            self.dropped_resource[direction] += len(data)

    def expired(self):
        exp = int(self.cfg.get("expires_at_unix", 0))
        return exp > 0 and time.time() >= exp

    async def pipe(self, reader, writer, bucket, direction, activity):
        while not self.blocked and not self.stopping:
            try:
                data = await asyncio.wait_for(reader.read(65536), timeout=1.0)
            except asyncio.TimeoutError:
                if time.monotonic() - activity[0] >= self.tcp_idle_timeout:
                    raise TimeoutError("TCP connection idle")
                continue
            if not data:
                # EOF ends only this direction: a peer can still send its
                # response after receiving the complete request.
                if writer.can_write_eof():
                    writer.write_eof()
                    await writer.drain()
                return
            activity[0] = time.monotonic()
            if not self.charge(len(data), direction):
                break
            await bucket.take(len(data))
            if self.blocked or self.stopping:
                break
            writer.write(data)
            await asyncio.wait_for(writer.drain(), timeout=self.tcp_idle_timeout)
            activity[0] = time.monotonic()

    async def handle_tcp(self, reader, writer, target):
        if self.blocked or self.stopping or len(self.conns) >= self.max_tcp_connections:
            self.rejected_tcp += 1
            writer.close()
            return
        self.conns.add(writer)
        task = asyncio.current_task()
        self.tcp_tasks.add(task)
        uw = None
        pipes = []
        try:
            ur, uw = await asyncio.wait_for(
                asyncio.open_connection("127.0.0.1", target, limit=32768),
                timeout=self.connect_timeout,
            )
            self.upstreams.add(uw)
            for w in (writer, uw):
                w.transport.set_write_buffer_limits(high=65536, low=16384)
            activity = [time.monotonic()]
            pipes = [
                asyncio.create_task(self.pipe(reader, uw, self.up, "up", activity)),
                asyncio.create_task(self.pipe(ur, writer, self.down, "down", activity)),
            ]
            await asyncio.gather(*pipes)
        except (ConnectionError, OSError, asyncio.TimeoutError):
            pass
        finally:
            for pipe in pipes:
                if not pipe.done():
                    pipe.cancel()
            await asyncio.gather(*pipes, return_exceptions=True)
            writers = [writer] + ([uw] if uw is not None else [])
            for w in writers:
                w.close()
            await asyncio.gather(
                *(asyncio.wait_for(w.wait_closed(), timeout=1) for w in writers),
                return_exceptions=True,
            )
            self.conns.discard(writer)
            self.upstreams.discard(uw)
            self.tcp_tasks.discard(task)


class UdpUpstream(asyncio.DatagramProtocol):
    def __init__(self, public, client, gateway, touch=None):
        self.public = public
        self.client = client
        self.gateway = gateway
        self.touch = touch

    def datagram_received(self, data, addr):
        # 目标返回客户端：下行。
        if self.touch:
            self.touch()
        if not self.gateway.charge(len(data), "down"):
            return
        if not self.gateway.down.try_take(len(data)):
            self.gateway.dropped_rate["down"] += len(data)
            return
        self.gateway.send_datagram(self.public, data, self.client, "down")


class UdpPublic(asyncio.DatagramProtocol):
    def __init__(self, target, gateway):
        self.target = target
        self.gateway = gateway
        self.peers = {}
        self.queue = asyncio.Queue(maxsize=gateway.max_udp_pending_packets)
        self.transport = None
        self.worker = None
        self.sweeper = None
        self.closed = False
        gateway.udp_protocols.append(self)

    def connection_made(self, transport):
        self.transport = transport
        self.worker = asyncio.create_task(self.drain_queue())
        self.sweeper = asyncio.create_task(self.sweep())

    def touch(self, addr):
        if addr in self.peers:
            self.peers[addr][1] = time.monotonic()

    async def _upstream(self, addr):
        entry = self.peers.get(addr)
        if entry:
            entry[1] = time.monotonic()
            return entry[0]
        if self.closed or self.gateway.udp_peer_count >= self.gateway.max_udp_peers:
            return None
        # Reserve before awaiting: the limit applies to all UDP listeners.
        self.gateway.udp_peer_count += 1
        tr = None
        try:
            loop = asyncio.get_running_loop()
            tr, _ = await asyncio.wait_for(
                loop.create_datagram_endpoint(
                    lambda: UdpUpstream(self.transport, addr, self.gateway, lambda: self.touch(addr)),
                    remote_addr=("127.0.0.1", self.target),
                ), timeout=self.gateway.connect_timeout,
            )
            if self.closed or self.gateway.blocked or self.gateway.stopping:
                return None
            self.peers[addr] = [tr, time.monotonic()]
            return tr
        finally:
            if addr not in self.peers:
                self.gateway.udp_peer_count -= 1
                if tr is not None:
                    tr.close()

    def datagram_received(self, data, addr):
        # 客户端发往目标：上行。
        if self.closed or not self.gateway.charge(len(data), "up"):
            return
        if not self.gateway.up.try_take(len(data)):
            self.gateway.dropped_rate["up"] += len(data)
            return
        entry = self.peers.get(addr)
        if entry:
            self.touch(addr)
            self.gateway.send_datagram(entry[0], data, None, "up")
            return
        gw = self.gateway
        if (gw.udp_pending_packets >= gw.max_udp_pending_packets
                or gw.udp_pending_bytes + len(data) > gw.max_udp_pending_bytes):
            gw.dropped_resource["up"] += len(data)
            return
        self.queue.put_nowait((data, addr))
        gw.udp_pending_packets += 1
        gw.udp_pending_bytes += len(data)

    async def drain_queue(self):
        while not self.closed:
            data, addr = await self.queue.get()
            try:
                tr = await self._upstream(addr)
                if tr is not None:
                    self.gateway.send_datagram(tr, data, None, "up")
                else:
                    self.gateway.dropped_resource["up"] += len(data)
            except OSError:
                self.gateway.dropped_resource["up"] += len(data)
            finally:
                self.gateway.udp_pending_packets -= 1
                self.gateway.udp_pending_bytes -= len(data)
                self.queue.task_done()

    def close(self):
        if self.closed:
            return
        self.closed = True
        for task in (self.worker, self.sweeper):
            if task is not None:
                task.cancel()
        while not self.queue.empty():
            data, _ = self.queue.get_nowait()
            self.gateway.udp_pending_packets -= 1
            self.gateway.udp_pending_bytes -= len(data)
            self.queue.task_done()
        for tr, _ in self.peers.values():
            tr.close()
            self.gateway.udp_peer_count -= 1
        self.peers.clear()
        if self.transport is not None:
            self.transport.close()

    def connection_lost(self, exc):
        self.close()

    async def sweep(self):
        while True:
            await asyncio.sleep(30)
            now = time.monotonic()
            for addr, entry in list(self.peers.items()):
                if now - entry[1] > 60:
                    entry[0].close()
                    del self.peers[addr]
                    self.gateway.udp_peer_count -= 1


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
        if gw.stopping:
            return
        gw.stopping = True
        gw.close_connections()
        tasks = list(gw.tcp_tasks)
        tasks += [task for p in gw.udp_protocols for task in (p.worker, p.sweeper) if task is not None]
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
        gw.flush()
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
                limit=32768,
            )
            gw.servers.append(srv)
        if "udp" in proto:
            proto_obj = UdpPublic(target, gw)
            tr, _ = await loop.create_datagram_endpoint(
                lambda p=proto_obj: p,
                sock=bind_socket(socket.SOCK_DGRAM, args.bind, public),
            )
            gw.transports.append(tr)

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
